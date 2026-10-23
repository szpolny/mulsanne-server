pub mod auth;
pub mod error;
pub mod ratelimit;
pub mod users;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::{middleware, Router};
use sqlx::PgPool;

/// Largest request body accepted by any route. Raised per route later for
/// lap traces.
pub const BODY_LIMIT: usize = 64 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    /// Read the client IP from `X-Forwarded-For` (only behind a trusted proxy).
    pub trust_forwarded: bool,
    pub registrations: Arc<ratelimit::Limiter>,
}

impl AppState {
    pub fn new(db: PgPool, trust_forwarded: bool) -> Self {
        Self {
            db,
            trust_forwarded,
            // New identities per IP; re-registering a known key isn't counted.
            registrations: Arc::new(ratelimit::Limiter::new(5, Duration::from_secs(3600))),
        }
    }
}

pub fn app(state: AppState) -> Router {
    let signed = Router::new()
        .route("/v1/users", post(users::register))
        .route("/v1/me", get(users::me).patch(users::update_me).delete(users::delete_me))
        .layer(middleware::from_fn(auth::verify_signature));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(signed)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}
