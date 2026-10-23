pub mod auth;
pub mod error;
pub mod laps;
pub mod ratelimit;
pub mod tracks;
pub mod users;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post};
use axum::{middleware, Router};
use sqlx::PgPool;
use uuid::Uuid;

/// Largest request body accepted by any route. Raised per route later for
/// lap traces.
pub const BODY_LIMIT: usize = 64 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    /// Read the client IP from `X-Forwarded-For` (only behind a trusted proxy).
    pub trust_forwarded: bool,
    pub registrations: Arc<ratelimit::Limiter<IpAddr>>,
    pub lap_uploads: Arc<ratelimit::Limiter<Uuid>>,
}

impl AppState {
    pub fn new(db: PgPool, trust_forwarded: bool) -> Self {
        Self::with_lap_limit(db, trust_forwarded, LAP_UPLOADS_PER_HOUR)
    }

    pub fn with_lap_limit(db: PgPool, trust_forwarded: bool, laps_per_hour: usize) -> Self {
        Self {
            db,
            trust_forwarded,
            // New identities per IP; re-registering a known key isn't counted.
            registrations: Arc::new(ratelimit::Limiter::new(5, Duration::from_secs(3600))),
            // New laps per user; re-uploading a stored lap isn't counted.
            lap_uploads: Arc::new(ratelimit::Limiter::new(laps_per_hour, Duration::from_secs(3600))),
        }
    }
}

/// Driving produces well under 60 laps an hour; the rest is headroom for
/// uploading past personal bests and sessions in bulk.
pub const LAP_UPLOADS_PER_HOUR: usize = 300;

pub fn app(state: AppState) -> Router {
    let signed = Router::new()
        .route("/v1/users", post(users::register))
        .route("/v1/me", get(users::me).patch(users::update_me).delete(users::delete_me))
        .route("/v1/laps", post(laps::upload))
        .route("/v1/laps/{id}", delete(laps::delete))
        .layer(middleware::from_fn(auth::verify_signature));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/leaderboards", get(laps::leaderboard))
        .route("/v1/leaderboards/boards", get(laps::boards))
        .merge(signed)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}
