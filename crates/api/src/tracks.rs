//! Track lengths. A lap's track length must match the track's, so laps
//! from a different layout (or a tampered upload) don't share a board.
//!
//! The expected length is the admin-verified one when set
//! (`mulsanne-server tracks set`), otherwise the median of other drivers'
//! uploads once [`CONSENSUS_DRIVERS`] of them agree. No single uploader
//! decides it, and an admin can always correct it; setting a verified length
//! re-checks every lap on that track.

use sqlx::PgPool;
use uuid::Uuid;

/// Lengths may differ by this fraction (LMU reports the same layout
/// identically, so this only absorbs rounding).
pub const LENGTH_TOLERANCE: f64 = 0.005;
/// Other drivers needed before their median length is enforced.
pub const CONSENSUS_DRIVERS: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Expected {
    /// Set by an admin; mismatching laps are rejected.
    Verified(f64),
    /// Median of other drivers' uploads; mismatching laps are held.
    Consensus(f64),
    /// Too few uploads to tell.
    Unknown,
}

pub fn matches(length: f64, expected: f64) -> bool {
    (length - expected).abs() <= expected * LENGTH_TOLERANCE
}

/// Expected length of `track` for an upload by `user`, whose own laps don't
/// count towards the consensus.
pub async fn expected_length(db: &PgPool, track: &str, user: Uuid) -> sqlx::Result<Expected> {
    let verified: Option<f64> = sqlx::query_scalar("SELECT length FROM tracks WHERE name = $1")
        .bind(track)
        .fetch_optional(db)
        .await?;
    if let Some(length) = verified {
        return Ok(Expected::Verified(length));
    }
    // Each driver counts once, with their latest upload.
    let (median, drivers): (Option<f64>, i64) = sqlx::query_as(
        "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY track_length), COUNT(*)
         FROM (SELECT DISTINCT ON (user_id) track_length FROM laps
               WHERE track = $1 AND user_id <> $2 ORDER BY user_id, created_at DESC) latest",
    )
    .bind(track)
    .bind(user)
    .fetch_one(db)
    .await?;
    Ok(match median {
        Some(m) if drivers >= CONSENSUS_DRIVERS => Expected::Consensus(m),
        _ => Expected::Unknown,
    })
}

/// Sets the verified length of `track`, holds ranked laps that don't match
/// it and ranks laps held only for their length that do. Returns
/// (newly held, newly ranked).
pub async fn set_length(db: &PgPool, track: &str, length: f64) -> sqlx::Result<(u64, u64)> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO tracks (name, length) VALUES ($1, $2)
         ON CONFLICT (name) DO UPDATE SET length = EXCLUDED.length, updated_at = now()",
    )
    .bind(track)
    .bind(length)
    .execute(&mut *tx)
    .await?;
    let held = sqlx::query(
        "UPDATE laps SET status = 'held', held_reason = 'track_length'
         WHERE track = $1 AND status = 'ranked' AND abs(track_length - $2) > $2 * $3",
    )
    .bind(track)
    .bind(length)
    .bind(LENGTH_TOLERANCE)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let ranked = sqlx::query(
        "UPDATE laps SET status = 'ranked', held_reason = NULL
         WHERE track = $1 AND held_reason = 'track_length' AND abs(track_length - $2) <= $2 * $3",
    )
    .bind(track)
    .bind(length)
    .bind(LENGTH_TOLERANCE)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok((held, ranked))
}

/// Removes the verified length; uploads fall back to the consensus.
pub async fn unset_length(db: &PgPool, track: &str) -> sqlx::Result<bool> {
    Ok(sqlx::query("DELETE FROM tracks WHERE name = $1").bind(track).execute(db).await?.rows_affected() > 0)
}

#[derive(Debug)]
pub struct TrackLengths {
    pub track: String,
    pub verified: Option<f64>,
    /// Median of every driver's latest upload.
    pub median: Option<f64>,
    pub drivers: i64,
    /// Distinct lengths uploaded, with how many drivers reported each.
    pub lengths: Vec<(f64, i64)>,
    pub held_for_length: i64,
}

/// Every track with laps or a verified length, for reviewing lengths.
pub async fn list(db: &PgPool) -> sqlx::Result<Vec<TrackLengths>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        track: String,
        verified: Option<f64>,
        median: Option<f64>,
        drivers: i64,
        held_for_length: i64,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "WITH latest AS (
             SELECT DISTINCT ON (track, user_id) track, track_length FROM laps ORDER BY track, user_id, created_at DESC
         ), names AS (SELECT DISTINCT track AS name FROM laps UNION SELECT name FROM tracks)
         SELECT n.name AS track, t.length AS verified,
                (SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY track_length) FROM latest WHERE track = n.name) AS median,
                (SELECT COUNT(*) FROM latest WHERE track = n.name) AS drivers,
                (SELECT COUNT(*) FROM laps WHERE track = n.name AND held_reason = 'track_length') AS held_for_length
         FROM names n LEFT JOIN tracks t ON t.name = n.name ORDER BY n.name",
    )
    .fetch_all(db)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let lengths: Vec<(f64, i64)> = sqlx::query_as(
            "SELECT round(track_length::numeric, 1)::float8, COUNT(DISTINCT user_id)
             FROM laps WHERE track = $1 GROUP BY 1 ORDER BY 2 DESC, 1",
        )
        .bind(&r.track)
        .fetch_all(db)
        .await?;
        out.push(TrackLengths {
            track: r.track,
            verified: r.verified,
            median: r.median,
            drivers: r.drivers,
            lengths,
            held_for_length: r.held_for_length,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_tolerance() {
        assert!(matches(7004.0, 7004.0));
        assert!(matches(7030.0, 7004.0));
        assert!(!matches(7050.0, 7004.0));
        assert!(!matches(6500.0, 7004.0));
    }
}
