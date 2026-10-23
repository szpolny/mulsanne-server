//! Types and request signing shared by the Mulsanne server and the app.
//!
//! The app is a public repo and can't depend on this crate yet, so
//! `src-tauri/src/online.rs` there mirrors [`signing`]. Both sides test
//! against the same vector (`signing::tests::matches_app_vector`); keep them
//! in sync when changing the scheme.

use serde::{Deserialize, Serialize};

pub mod signing;

/// The signed-in user, as returned by `/v1/users` and `/v1/me`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Me {
    pub id: String,
    /// `None` means the user shows as "Anonymous".
    pub display_name: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
}

/// `POST /v1/users`: registers the signing key. Idempotent; registering a
/// known key returns the existing user.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegisterRequest {
    #[serde(default)]
    pub display_name: Option<String>,
}

/// `PATCH /v1/me`. `None` or an empty name clears it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateMeRequest {
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Error body for every non-2xx response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

/// Session types as the app labels them.
pub const SESSION_TYPES: &[&str] = &["Test Day", "Practice", "Qualifying", "Warmup", "Race"];

/// `POST /v1/laps`: one clean flying lap. Times in seconds, distances in
/// metres, speeds in m/s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LapUpload {
    pub track: String,
    pub track_length: f64,
    pub car: String,
    pub vehicle_name: String,
    pub car_class: String,
    pub session_type: String,
    /// LMU's build number, when the app knows it.
    #[serde(default)]
    pub game_version: Option<i32>,
    pub lap_time: f64,
    pub s1: f64,
    pub s2: f64,
    pub s3: f64,
    #[serde(default)]
    pub fuel_used: Option<f64>,
    #[serde(default)]
    pub ve_used: Option<f64>,
    pub max_speed: f64,
    /// Unix seconds.
    pub recorded_at: i64,
    pub app_version: String,
    /// Hide the driver's name on leaderboards for this lap.
    #[serde(default)]
    pub anonymous: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LapStatus {
    /// On the leaderboards.
    Ranked,
    /// Hidden until reviewed: much faster than the record, or its track
    /// length doesn't match other drivers'.
    Held,
}

/// Response to `POST /v1/laps`. Uploading the same lap again returns the
/// original.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LapUploaded {
    pub id: String,
    pub status: LapStatus,
}

/// `GET /v1/leaderboards`: best ranked lap per driver.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LeaderboardQuery {
    pub track: String,
    pub car_class: String,
    #[serde(default)]
    pub car: Option<String>,
    #[serde(default)]
    pub session_type: Option<String>,
    #[serde(default)]
    pub game_version: Option<i32>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaderboardEntry {
    pub rank: i64,
    pub lap_id: String,
    /// `None` for anonymous laps.
    pub driver: Option<String>,
    pub car: String,
    pub vehicle_name: String,
    pub session_type: String,
    pub game_version: Option<i32>,
    /// Its telemetry can be downloaded from `GET /v1/laps/{lap_id}/trace`.
    pub has_trace: bool,
    pub lap_time: f64,
    pub s1: f64,
    pub s2: f64,
    pub s3: f64,
    /// Unix seconds.
    pub recorded_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Leaderboard {
    pub entries: Vec<LeaderboardEntry>,
    /// Drivers on the board, for paging.
    pub total: i64,
}

/// `GET /v1/leaderboards/boards`: every track and class with ranked laps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardSummary {
    pub track: String,
    pub car_class: String,
    pub drivers: i64,
    pub best_lap: f64,
    /// Game versions with ranked laps on this board, newest first.
    pub game_versions: Vec<i32>,
}

pub const DISPLAY_NAME_MIN: usize = 2;
pub const DISPLAY_NAME_MAX: usize = 32;

/// Trims a display name; empty means anonymous (`Ok(None)`).
pub fn normalize_display_name(name: Option<&str>) -> Result<Option<String>, &'static str> {
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    let len = name.chars().count();
    if !(DISPLAY_NAME_MIN..=DISPLAY_NAME_MAX).contains(&len) {
        return Err("Display name must be 2 to 32 characters");
    }
    if name.chars().any(char::is_control) {
        return Err("Display name contains invalid characters");
    }
    Ok(Some(name.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names() {
        assert_eq!(normalize_display_name(None), Ok(None));
        assert_eq!(normalize_display_name(Some("   ")), Ok(None));
        assert_eq!(normalize_display_name(Some("  Szymon ")), Ok(Some("Szymon".into())));
        assert!(normalize_display_name(Some("x")).is_err());
        assert!(normalize_display_name(Some(&"x".repeat(33))).is_err());
        assert!(normalize_display_name(Some("a\nb")).is_err());
        // Counted in characters, not bytes.
        assert_eq!(normalize_display_name(Some("Łódź")), Ok(Some("Łódź".into())));
    }
}
