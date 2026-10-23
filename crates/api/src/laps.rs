use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use mulsanne_api::{
    BoardSummary, LapStatus, LapUpload, LapUploaded, Leaderboard, LeaderboardEntry, LeaderboardQuery, SESSION_TYPES,
};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::tracks::{self, Expected};
use crate::AppState;

/// Sector times must add up to the lap time within this (s).
const SECTOR_TOLERANCE: f64 = 0.05;
/// Laps this much faster than the board's record are held for review.
const HOLD_MARGIN: f64 = 0.03;
/// No car in the game is faster than this (m/s, ~450 km/h).
const MAX_SPEED: f64 = 125.0;
/// 2024-01-01, before any lap Mulsanne could have recorded.
const EARLIEST_LAP: i64 = 1_704_067_200;
const MAX_TEXT: usize = 100;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_lap", message)
}

fn text(name: &str, value: &str) -> AppResult<()> {
    if value.trim().is_empty() || value.chars().count() > MAX_TEXT || value.chars().any(char::is_control) {
        return Err(invalid(format!("Invalid {name}")));
    }
    Ok(())
}

/// Plausibility checks that need no database. Best-effort, not anti-cheat.
pub fn validate(lap: &LapUpload, now: i64) -> AppResult<()> {
    text("track", &lap.track)?;
    text("car", &lap.car)?;
    text("vehicle name", &lap.vehicle_name)?;
    text("car class", &lap.car_class)?;
    if lap.app_version.is_empty() || lap.app_version.len() > 32 {
        return Err(invalid("Invalid app version"));
    }
    if !SESSION_TYPES.contains(&lap.session_type.as_str()) {
        return Err(invalid("Unknown session type"));
    }
    if lap.game_version.is_some_and(|v| v <= 0) {
        return Err(invalid("Invalid game version"));
    }

    let numbers = [lap.track_length, lap.lap_time, lap.s1, lap.s2, lap.s3, lap.max_speed];
    let optional = [lap.fuel_used, lap.ve_used].into_iter().flatten();
    if numbers.into_iter().chain(optional.clone()).any(|v| !v.is_finite()) || optional.into_iter().any(|v| v < 0.0) {
        return Err(invalid("Invalid number"));
    }
    if !(1_000.0..=25_000.0).contains(&lap.track_length) {
        return Err(invalid("Track length is out of range"));
    }
    if !(20.0..=1_800.0).contains(&lap.lap_time) {
        return Err(invalid("Lap time is out of range"));
    }
    if [lap.s1, lap.s2, lap.s3].iter().any(|s| *s <= 0.0) || (lap.s1 + lap.s2 + lap.s3 - lap.lap_time).abs() > SECTOR_TOLERANCE {
        return Err(invalid("Sector times don't add up to the lap time"));
    }
    let average_speed = lap.track_length / lap.lap_time;
    if lap.max_speed <= 0.0 || lap.max_speed > MAX_SPEED || average_speed > lap.max_speed + 1.0 {
        return Err(invalid("Speed is implausible"));
    }
    if lap.recorded_at < EARLIEST_LAP || lap.recorded_at > now + 300 {
        return Err(invalid("Recording time is out of range"));
    }
    Ok(())
}

fn parse_status(status: &str) -> LapStatus {
    if status == "held" {
        LapStatus::Held
    } else {
        LapStatus::Ranked
    }
}

fn uploaded((id, status): (Uuid, String)) -> Json<LapUploaded> {
    Json(LapUploaded { id: id.to_string(), status: parse_status(&status) })
}

pub async fn upload(
    State(state): State<AppState>,
    user: AuthUser,
    Json(lap): Json<LapUpload>,
) -> AppResult<(StatusCode, Json<LapUploaded>)> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    validate(&lap, now)?;

    let existing_sql = "SELECT id, status FROM laps
         WHERE user_id = $1 AND track = $2 AND car = $3 AND recorded_at = to_timestamp($4)";
    let existing: Option<(Uuid, String)> = sqlx::query_as(existing_sql)
        .bind(user.id)
        .bind(&lap.track)
        .bind(&lap.car)
        .bind(lap.recorded_at as f64)
        .fetch_optional(&state.db)
        .await?;
    if let Some(row) = existing {
        return Ok((StatusCode::OK, uploaded(row)));
    }

    // Counted after the duplicate check, so retries of stored laps are free.
    if !state.lap_uploads.check(user.id) {
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many lap uploads; the rest will be sent later",
        ));
    }

    let mut held_reason = match tracks::expected_length(&state.db, &lap.track, user.id).await? {
        Expected::Verified(length) if !tracks::matches(lap.track_length, length) => {
            return Err(invalid("Track length doesn't match this track"));
        }
        Expected::Verified(_) => None,
        _ if !tracks::plausible(&lap.track, lap.track_length) => Some("track_length"),
        Expected::Consensus(length) if !tracks::matches(lap.track_length, length) => Some("track_length"),
        _ => None,
    };
    if held_reason.is_none() {
        let record: Option<f64> = sqlx::query_scalar(
            "SELECT MIN(lap_time) FROM laps WHERE status = 'ranked' AND track = $1 AND car_class = $2",
        )
        .bind(&lap.track)
        .bind(&lap.car_class)
        .fetch_one(&state.db)
        .await?;
        if record.is_some_and(|best| lap.lap_time < best * (1.0 - HOLD_MARGIN)) {
            held_reason = Some("fast");
        }
    }

    let inserted: Option<(Uuid, String)> = sqlx::query_as(
        "INSERT INTO laps (user_id, track, track_length, car, vehicle_name, car_class, session_type, game_version,
                           lap_time, s1, s2, s3, fuel_used, ve_used, max_speed, recorded_at, app_version, anonymous,
                           status, held_reason)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, to_timestamp($16), $17, $18, $19, $20)
         ON CONFLICT (user_id, track, car, recorded_at) DO NOTHING
         RETURNING id, status",
    )
    .bind(user.id)
    .bind(&lap.track)
    .bind(lap.track_length)
    .bind(&lap.car)
    .bind(&lap.vehicle_name)
    .bind(&lap.car_class)
    .bind(&lap.session_type)
    .bind(lap.game_version)
    .bind(lap.lap_time)
    .bind(lap.s1)
    .bind(lap.s2)
    .bind(lap.s3)
    .bind(lap.fuel_used)
    .bind(lap.ve_used)
    .bind(lap.max_speed)
    .bind(lap.recorded_at as f64)
    .bind(&lap.app_version)
    .bind(lap.anonymous)
    .bind(if held_reason.is_some() { "held" } else { "ranked" })
    .bind(held_reason)
    .fetch_optional(&state.db)
    .await?;
    match inserted {
        Some(row) => Ok((StatusCode::CREATED, uploaded(row))),
        // Lost a race with a concurrent upload of the same lap.
        None => {
            let row: (Uuid, String) = sqlx::query_as(existing_sql)
                .bind(user.id)
                .bind(&lap.track)
                .bind(&lap.car)
                .bind(lap.recorded_at as f64)
                .fetch_one(&state.db)
                .await?;
            Ok((StatusCode::OK, uploaded(row)))
        }
    }
}

pub async fn delete(State(state): State<AppState>, user: AuthUser, Path(id): Path<Uuid>) -> AppResult<StatusCode> {
    let has_trace: Option<bool> = sqlx::query_scalar("DELETE FROM laps WHERE id = $1 AND user_id = $2 RETURNING has_trace")
        .bind(id)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await?;
    match (has_trace, &state.traces) {
        (None, _) => return Err(AppError::new(StatusCode::NOT_FOUND, "not_found", "Lap not found")),
        (Some(true), Some(store)) => crate::traces::delete_objects(store, &[id]).await,
        _ => {}
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(sqlx::FromRow)]
struct EntryRow {
    rank: i64,
    id: Uuid,
    driver: Option<String>,
    car: String,
    vehicle_name: String,
    session_type: String,
    game_version: Option<i32>,
    has_trace: bool,
    lap_time: f64,
    s1: f64,
    s2: f64,
    s3: f64,
    recorded_at: i64,
    total: i64,
}

pub async fn leaderboard(State(state): State<AppState>, Query(q): Query<LeaderboardQuery>) -> AppResult<Json<Leaderboard>> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = q.offset.unwrap_or(0).max(0);
    // Best ranked lap per driver; ties keep the earlier lap.
    let rows: Vec<EntryRow> = sqlx::query_as(
        "WITH best AS (
             SELECT DISTINCT ON (l.user_id) l.*
             FROM laps l
             WHERE l.status = 'ranked' AND l.track = $1 AND l.car_class = $2
               AND ($3::text IS NULL OR l.car = $3) AND ($4::text IS NULL OR l.session_type = $4)
               AND ($5::int IS NULL OR l.game_version = $5)
             ORDER BY l.user_id, l.lap_time, l.recorded_at
         ), ranked AS (
             SELECT b.*, RANK() OVER (ORDER BY b.lap_time) AS rank, COUNT(*) OVER () AS total FROM best b
         )
         SELECT r.rank, r.id, CASE WHEN r.anonymous THEN NULL ELSE u.display_name END AS driver,
                r.car, r.vehicle_name, r.session_type, r.game_version, r.has_trace, r.lap_time, r.s1, r.s2, r.s3,
                EXTRACT(EPOCH FROM r.recorded_at)::BIGINT AS recorded_at, r.total
         FROM ranked r JOIN users u ON u.id = r.user_id
         ORDER BY r.rank, r.recorded_at
         LIMIT $6 OFFSET $7",
    )
    .bind(&q.track)
    .bind(&q.car_class)
    .bind(&q.car)
    .bind(&q.session_type)
    .bind(q.game_version)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    let total = rows.first().map_or(0, |r| r.total);
    let entries = rows
        .into_iter()
        .map(|r| LeaderboardEntry {
            rank: r.rank,
            lap_id: r.id.to_string(),
            driver: r.driver,
            car: r.car,
            vehicle_name: r.vehicle_name,
            session_type: r.session_type,
            game_version: r.game_version,
            has_trace: r.has_trace,
            lap_time: r.lap_time,
            s1: r.s1,
            s2: r.s2,
            s3: r.s3,
            recorded_at: r.recorded_at,
        })
        .collect();
    Ok(Json(Leaderboard { entries, total }))
}

pub async fn boards(State(state): State<AppState>) -> AppResult<Json<Vec<BoardSummary>>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        track: String,
        car_class: String,
        drivers: i64,
        best_lap: f64,
        game_versions: Option<Vec<i32>>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT track, car_class, COUNT(DISTINCT user_id) AS drivers, MIN(lap_time) AS best_lap,
                array_agg(DISTINCT game_version ORDER BY game_version DESC)
                    FILTER (WHERE game_version IS NOT NULL) AS game_versions
         FROM laps WHERE status = 'ranked' GROUP BY track, car_class ORDER BY track, car_class",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| BoardSummary {
                track: r.track,
                car_class: r.car_class,
                drivers: r.drivers,
                best_lap: r.best_lap,
                game_versions: r.game_versions.unwrap_or_default(),
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_760_000_000;

    fn lap() -> LapUpload {
        LapUpload {
            track: "Circuit de Spa-Francorchamps".into(),
            track_length: 7004.0,
            car: "Ferrari 296 LMGT3".into(),
            vehicle_name: "Ferrari 296 LMGT3 #55".into(),
            car_class: "GT3".into(),
            session_type: "Practice".into(),
            game_version: Some(1123),
            lap_time: 138.5,
            s1: 42.1,
            s2: 60.2,
            s3: 36.2,
            fuel_used: Some(3.1),
            ve_used: None,
            max_speed: 75.0,
            recorded_at: NOW - 60,
            app_version: "0.5.0".into(),
            anonymous: false,
        }
    }

    fn rejects(change: impl FnOnce(&mut LapUpload)) -> bool {
        let mut l = lap();
        change(&mut l);
        validate(&l, NOW).is_err()
    }

    #[test]
    fn accepts_a_plausible_lap() {
        validate(&lap(), NOW).unwrap();
    }

    #[test]
    fn rejects_implausible_laps() {
        assert!(rejects(|l| l.s3 += 1.0));
        assert!(rejects(|l| l.s1 = 0.0));
        assert!(rejects(|l| l.lap_time = 5.0));
        assert!(rejects(|l| l.track_length = 100.0));
        assert!(rejects(|l| l.max_speed = 200.0));
        // Average speed above the top speed.
        assert!(rejects(|l| l.max_speed = 40.0));
        assert!(rejects(|l| l.recorded_at = NOW + 3600));
        assert!(rejects(|l| l.recorded_at = 0));
        assert!(rejects(|l| l.session_type = "Hotlap".into()));
        assert!(rejects(|l| l.track = " ".into()));
        assert!(rejects(|l| l.car = "x".repeat(101)));
        assert!(rejects(|l| l.lap_time = f64::NAN));
        assert!(rejects(|l| l.fuel_used = Some(-1.0)));
        assert!(rejects(|l| l.ve_used = Some(f64::INFINITY)));
        assert!(rejects(|l| l.game_version = Some(0)));
    }
}
