//! Track lengths. A lap's track length must match the track's, so laps
//! from a different layout (or a tampered upload) don't share a board.
//!
//! The expected length is the admin-verified one when set
//! (`mulsanne-server tracks set`), otherwise the median of other drivers'
//! uploads once [`CONSENSUS_DRIVERS`] of them agree. Main layouts of known
//! circuits must also be within [`OFFICIAL_TOLERANCE`] of the official
//! length, so even a consensus can't move them far. No single uploader
//! decides a length, and an admin can always correct it; setting a verified
//! length re-checks every lap on that track.

use sqlx::PgPool;
use uuid::Uuid;

/// Lengths may differ by this fraction (LMU reports the same layout
/// identically, so this only absorbs rounding).
pub const LENGTH_TOLERANCE: f64 = 0.005;
/// Other drivers needed before their median length is enforced.
pub const CONSENSUS_DRIVERS: i64 = 3;

/// LMU reports its own lap distance, which may differ from the official
/// length by a little; anything further off is a different layout or bogus.
pub const OFFICIAL_TOLERANCE: f64 = 0.03;

struct Circuit {
    /// Words or phrases that identify the circuit in a track name.
    names: &'static [&'static str],
    /// Layout variants with their own length; skipped since no official
    /// length is published for them.
    variants: &'static [&'static str],
    /// Official length of the main layout (m).
    length: f64,
}

/// Official lengths of LMU circuits' main layouts, from the 2026 FIA WEC
/// calendar (Porsche Motorsport) and Coach Dave Academy's LMU track guide.
/// Paul Ricard is left out: its layouts differ and sources disagree.
const OFFICIAL: &[Circuit] = &[
    Circuit { names: &["sarthe", "le mans", "lemans"], variants: &["mulsanne", "no chicane", "no chicanes"], length: 13_626.0 },
    Circuit { names: &["spa", "francorchamps"], variants: &["endurance"], length: 7_004.0 },
    Circuit { names: &["monza"], variants: &["curva grande"], length: 5_793.0 },
    Circuit { names: &["fuji"], variants: &["classic", "no chicane"], length: 4_563.0 },
    Circuit { names: &["bahrain", "sakhir"], variants: &["outer", "endurance", "paddock"], length: 5_412.0 },
    Circuit { names: &["portimao", "algarve"], variants: &[], length: 4_653.0 },
    Circuit { names: &["sebring"], variants: &["school"], length: 6_019.0 },
    Circuit { names: &["imola", "enzo e dino ferrari"], variants: &[], length: 4_909.0 },
    Circuit { names: &["interlagos", "carlos pace", "sao paulo"], variants: &[], length: 4_309.0 },
    Circuit { names: &["lusail", "losail", "qatar"], variants: &["short"], length: 5_418.0 },
    Circuit { names: &["americas", "cota"], variants: &["national"], length: 5_513.0 },
    Circuit { names: &["silverstone"], variants: &["national", "international"], length: 5_891.0 },
    Circuit { names: &["barcelona", "catalunya"], variants: &[], length: 4_657.0 },
];

/// Lowercase ASCII words separated by single spaces, padded with spaces so
/// phrases can be matched as whole words.
fn normalize(name: &str) -> String {
    let folded: String = name
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            c if c.is_ascii_alphanumeric() => c,
            _ => ' ',
        })
        .collect();
    format!(" {} ", folded.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Official length of the main layout `track` names, if known.
pub fn official_length(track: &str) -> Option<f64> {
    let name = normalize(track);
    let has = |phrase: &&str| name.contains(&format!(" {phrase} "));
    let circuit = OFFICIAL.iter().find(|c| c.names.iter().any(has))?;
    (!circuit.variants.iter().any(has)).then_some(circuit.length)
}

/// Whether `length` is plausible for `track`: within [`OFFICIAL_TOLERANCE`]
/// of the official length, or the track has none.
pub fn plausible(track: &str, length: f64) -> bool {
    official_length(track).is_none_or(|official| (length - official).abs() <= official * OFFICIAL_TOLERANCE)
}

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
    fn official_lengths() {
        assert_eq!(official_length("Circuit de la Sarthe"), Some(13_626.0));
        assert_eq!(official_length("Circuit de Spa-Francorchamps"), Some(7_004.0));
        assert_eq!(official_length("Autódromo Internacional do Algarve"), Some(4_653.0));
        assert_eq!(official_length("Autódromo José Carlos Pace"), Some(4_309.0));
        assert_eq!(official_length("Bahrain International Circuit"), Some(5_412.0));
        // Variants have their own lengths.
        assert_eq!(official_length("Bahrain International Outer Circuit"), None);
        assert_eq!(official_length("Silverstone National"), None);
        assert_eq!(official_length("Circuit de la Sarthe Mulsanne No Chicanes"), None);
        assert_eq!(official_length("Lusail International Circuit Short"), None);
        assert_eq!(official_length("Circuit Paul Ricard"), None);
        // Whole words only: "spa" inside another word doesn't count.
        assert_eq!(official_length("Espana Raceway"), None);
    }

    #[test]
    fn plausibility() {
        assert!(plausible("Spa-Francorchamps", 7_004.0));
        assert!(plausible("Spa-Francorchamps", 7_150.0));
        assert!(!plausible("Spa-Francorchamps", 6_500.0));
        assert!(plausible("Unknown Raceway", 1_234.0));
    }

    #[test]
    fn length_tolerance() {
        assert!(matches(7004.0, 7004.0));
        assert!(matches(7030.0, 7004.0));
        assert!(!matches(7050.0, 7004.0));
        assert!(!matches(6500.0, 7004.0));
    }
}
