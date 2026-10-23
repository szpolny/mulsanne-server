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
