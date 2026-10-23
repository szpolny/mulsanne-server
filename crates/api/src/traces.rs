//! Lap telemetry traces: gzipped JSON column arrays (the app's `LapTrace`),
//! kept in object storage at `traces/<lap id>.json.gz`.
//!
//! Only each driver's best lap per track and class keeps a trace: uploading
//! one for a slower lap is refused, and a new best replaces the old one.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use flate2::read::GzDecoder;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::AppState;

/// Largest compressed trace accepted (a Le Mans lap is ~500 KB).
pub const MAX_COMPRESSED: usize = 2 * 1024 * 1024;
/// Largest trace after decompression.
const MAX_DECOMPRESSED: u64 = 16 * 1024 * 1024;
/// 30 Hz for half an hour, with headroom.
const MAX_SAMPLES: usize = 120_000;
/// The trace must span this much of the track.
const MIN_COVERAGE: f64 = 0.95;
/// Its last sample must be this close to the lap time (s).
const TIME_TOLERANCE: f64 = 1.0;
/// Lap distance may step back this much between samples (m) before it
/// counts as going backwards.
const DISTANCE_JITTER: f64 = 5.0;

pub type Store = Arc<dyn ObjectStore>;

/// S3-compatible storage (R2, B2, MinIO, …) from `S3_*` variables, or
/// `None` when `S3_BUCKET` isn't set (traces are then unavailable).
pub fn store_from_env() -> Result<Option<Store>, Box<dyn std::error::Error>> {
    let Ok(bucket) = std::env::var("S3_BUCKET") else { return Ok(None) };
    let var = |name| std::env::var(name).map_err(|_| format!("{name} is not set"));
    let store = AmazonS3Builder::new()
        .with_bucket_name(bucket)
        .with_endpoint(var("S3_ENDPOINT")?)
        .with_region(std::env::var("S3_REGION").unwrap_or_else(|_| "auto".into()))
        .with_access_key_id(var("S3_ACCESS_KEY_ID")?)
        .with_secret_access_key(var("S3_SECRET_ACCESS_KEY")?)
        // Plain http is only for a local MinIO.
        .with_allow_http(std::env::var("S3_ALLOW_HTTP").is_ok_and(|v| v == "1" || v == "true"))
        .build()?;
    Ok(Some(Arc::new(store)))
}

fn key(lap: Uuid) -> ObjectPath {
    ObjectPath::from(format!("traces/{lap}.json.gz"))
}

fn store(state: &AppState) -> AppResult<&Store> {
    state
        .traces
        .as_ref()
        .ok_or_else(|| AppError::new(StatusCode::SERVICE_UNAVAILABLE, "no_traces", "Trace storage isn't available"))
}

fn invalid(message: impl Into<String>) -> AppError {
    AppError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_trace", message)
}

fn storage_error(e: object_store::Error) -> AppError {
    tracing::error!("trace storage: {e}");
    AppError::new(StatusCode::SERVICE_UNAVAILABLE, "storage", "Trace storage is unavailable; try again later")
}

/// Deletes stored traces; failures are logged, since the rows no longer
/// point at them anyway.
pub async fn delete_objects(store: &Store, laps: &[Uuid]) {
    for lap in laps {
        if let Err(e) = store.delete(&key(*lap)).await {
            if !matches!(e, object_store::Error::NotFound { .. }) {
                tracing::warn!("could not delete trace {lap}: {e}");
            }
        }
    }
}

#[derive(Deserialize)]
struct Columns {
    t: Vec<f64>,
    d: Vec<f64>,
    speed: Vec<f64>,
    #[serde(flatten)]
    rest: HashMap<String, Value>,
}

/// What the trace is checked against.
pub struct LapFacts {
    pub lap_time: f64,
    pub track_length: f64,
    pub max_speed: f64,
}

/// Decompresses a trace and checks it describes the lap it's attached to.
/// Best-effort, like the lap checks: it catches broken and mismatched
/// uploads, not a determined forger.
pub fn validate(gz: &[u8], lap: &LapFacts) -> AppResult<()> {
    let mut json = Vec::new();
    GzDecoder::new(gz)
        .take(MAX_DECOMPRESSED + 1)
        .read_to_end(&mut json)
        .map_err(|_| invalid("Trace isn't valid gzip"))?;
    if json.len() as u64 > MAX_DECOMPRESSED {
        return Err(invalid("Trace is too large"));
    }
    let c: Columns = serde_json::from_slice(&json).map_err(|e| invalid(format!("Trace isn't valid: {e}")))?;

    let n = c.t.len();
    if !(100..=MAX_SAMPLES).contains(&n) {
        return Err(invalid("Trace has too few or too many samples"));
    }
    let same_length = |v: &Value| v.as_array().is_some_and(|a| a.len() == n && a.iter().all(Value::is_number));
    if c.d.len() != n || c.speed.len() != n || !c.rest.values().all(same_length) {
        return Err(invalid("Trace columns have different lengths"));
    }
    if c.t.iter().chain(&c.d).chain(&c.speed).any(|v| !v.is_finite()) {
        return Err(invalid("Trace has invalid numbers"));
    }
    if c.t.windows(2).any(|w| w[1] < w[0]) || (c.t[n - 1] - lap.lap_time).abs() > TIME_TOLERANCE {
        return Err(invalid("Trace timing doesn't match the lap time"));
    }
    if c.d.windows(2).any(|w| w[1] < w[0] - DISTANCE_JITTER) {
        return Err(invalid("Trace distance goes backwards"));
    }
    let (min_d, max_d) = c.d.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &d| (lo.min(d), hi.max(d)));
    if max_d - min_d < lap.track_length * MIN_COVERAGE || max_d > lap.track_length * 1.02 {
        return Err(invalid("Trace doesn't cover the lap"));
    }
    let top = c.speed.iter().cloned().fold(0.0, f64::max);
    if c.speed.iter().any(|s| *s < 0.0) || (top - lap.max_speed).abs() > 1.0 {
        return Err(invalid("Trace speed doesn't match the lap"));
    }
    Ok(())
}

/// `PUT /v1/laps/{id}/trace`: the body is the gzipped trace.
pub async fn put(State(state): State<AppState>, user: AuthUser, Path(id): Path<Uuid>, body: Bytes) -> AppResult<StatusCode> {
    let store = store(&state)?;
    let lap: Option<(String, String, f64, f64, f64, i64)> = sqlx::query_as(
        "SELECT track, car_class, lap_time, track_length, max_speed, EXTRACT(EPOCH FROM recorded_at)::BIGINT
         FROM laps WHERE id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(user.id)
    .fetch_optional(&state.db)
    .await?;
    let Some((track, car_class, lap_time, track_length, max_speed, recorded_at)) = lap else {
        return Err(AppError::new(StatusCode::NOT_FOUND, "not_found", "Lap not found"));
    };

    // Only the driver's best lap on this board keeps a trace (ties: the earlier lap).
    let faster: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM laps WHERE user_id = $1 AND track = $2 AND car_class = $3 AND id <> $4
             AND (lap_time < $5 OR (lap_time = $5 AND EXTRACT(EPOCH FROM recorded_at)::BIGINT < $6)))",
    )
    .bind(user.id)
    .bind(&track)
    .bind(&car_class)
    .bind(id)
    .bind(lap_time)
    .bind(recorded_at)
    .fetch_one(&state.db)
    .await?;
    if faster {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "not_best",
            "Only your best lap on this track and class keeps a trace",
        ));
    }

    if body.len() > MAX_COMPRESSED {
        return Err(AppError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "Trace is too large"));
    }
    validate(&body, &LapFacts { lap_time, track_length, max_speed })?;

    store.put(&key(id), PutPayload::from(body)).await.map_err(storage_error)?;
    sqlx::query("UPDATE laps SET has_trace = true WHERE id = $1").bind(id).execute(&state.db).await?;

    // The previous best's trace isn't needed any more.
    let replaced: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE laps SET has_trace = false
         WHERE user_id = $1 AND track = $2 AND car_class = $3 AND id <> $4 AND has_trace RETURNING id",
    )
    .bind(user.id)
    .bind(&track)
    .bind(&car_class)
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    delete_objects(store, &replaced).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/laps/{id}/trace`: public for ranked laps, as the gzipped JSON.
pub async fn get(State(state): State<AppState>, Path(id): Path<Uuid>) -> AppResult<impl IntoResponse> {
    let store = store(&state)?;
    let available: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM laps WHERE id = $1 AND status = 'ranked' AND has_trace)")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    if !available {
        return Err(AppError::new(StatusCode::NOT_FOUND, "not_found", "No trace for this lap"));
    }
    let bytes = store.get(&key(id)).await.map_err(storage_error)?.bytes().await.map_err(storage_error)?;
    Ok(([(header::CONTENT_TYPE, "application/gzip")], bytes))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::write::GzEncoder;
    use flate2::Compression;
    use serde_json::json;

    use super::*;

    const LAP: LapFacts = LapFacts { lap_time: 100.0, track_length: 5000.0, max_speed: 80.0 };

    /// A plausible 100 s lap of a 5 km track at 10 Hz.
    fn trace() -> Value {
        let n = 1001;
        let t: Vec<f64> = (0..n).map(|i| i as f64 * 0.1).collect();
        let d: Vec<f64> = (0..n).map(|i| i as f64 * 4.995).collect();
        let speed: Vec<f64> = (0..n).map(|i| 40.0 + 40.0 * (i as f64 / (n - 1) as f64)).collect();
        json!({ "t": t, "d": d, "speed": speed, "throttle": vec![1.0; n], "gear": vec![4; n] })
    }

    fn gz(v: &Value) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(v.to_string().as_bytes()).unwrap();
        enc.finish().unwrap()
    }

    fn rejects(change: impl FnOnce(&mut Value)) -> bool {
        let mut v = trace();
        change(&mut v);
        validate(&gz(&v), &LAP).is_err()
    }

    #[test]
    fn accepts_a_matching_trace() {
        validate(&gz(&trace()), &LAP).unwrap();
    }

    #[test]
    fn rejects_mismatched_traces() {
        assert!(validate(b"not gzip", &LAP).is_err());
        assert!(validate(&gz(&json!({ "t": [] })), &LAP).is_err());
        // Wrong lap time, too short, wrong top speed.
        assert!(validate(&gz(&trace()), &LapFacts { lap_time: 90.0, ..LAP }).is_err());
        assert!(validate(&gz(&trace()), &LapFacts { track_length: 7000.0, ..LAP }).is_err());
        assert!(validate(&gz(&trace()), &LapFacts { max_speed: 90.0, ..LAP }).is_err());
        assert!(rejects(|v| v["throttle"] = json!([1.0])));
        assert!(rejects(|v| v["gear"] = json!(vec!["x"; 1001])));
        assert!(rejects(|v| v["d"][500] = json!(0.0)));
        assert!(rejects(|v| v["t"][500] = json!(0.0)));
        assert!(rejects(|v| v["speed"][10] = json!(-1.0)));
    }
}
