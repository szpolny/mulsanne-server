pub mod auth;
pub mod error;
pub mod laps;
pub mod live;
pub mod ratelimit;
pub mod traces;
pub mod tracks;
pub mod users;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post, put};
use axum::{middleware, Router};
use sqlx::PgPool;
use uuid::Uuid;

/// Largest request body accepted by any route except trace uploads
/// ([`traces::MAX_COMPRESSED`]).
pub const BODY_LIMIT: usize = 64 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    /// Trusted proxies in front of the server that append to
    /// `X-Forwarded-For`; 0 uses the socket address (see [`auth::ClientIp`]).
    pub proxy_hops: usize,
    pub registrations: Arc<ratelimit::Limiter<IpAddr>>,
    pub lap_uploads: Arc<ratelimit::Limiter<Uuid>>,
    /// Object storage for lap traces; `None` disables them.
    pub traces: Option<traces::Store>,
    /// Open live timing shares (in memory; see [`live`]).
    pub live: Arc<live::Hub>,
    /// New live shares per user.
    pub live_creates: Arc<ratelimit::Limiter<Uuid>>,
}

impl AppState {
    pub fn new(db: PgPool) -> Self {
        Self::with_lap_limit(db, LAP_UPLOADS_PER_HOUR)
    }

    pub fn with_lap_limit(db: PgPool, laps_per_hour: usize) -> Self {
        Self {
            db,
            proxy_hops: 0,
            // New identities per IP; re-registering a known key isn't counted.
            registrations: Arc::new(ratelimit::Limiter::new(5, Duration::from_secs(3600))),
            // New laps per user; re-uploading a stored lap isn't counted.
            lap_uploads: Arc::new(ratelimit::Limiter::new(laps_per_hour, Duration::from_secs(3600))),
            traces: None,
            live: Arc::new(live::Hub::default()),
            live_creates: Arc::new(ratelimit::Limiter::new(live::SHARES_PER_HOUR, Duration::from_secs(3600))),
        }
    }

    pub fn with_traces(mut self, store: Option<traces::Store>) -> Self {
        self.traces = store;
        self
    }

    pub fn with_proxy_hops(mut self, hops: usize) -> Self {
        self.proxy_hops = hops;
        self
    }
}

/// The deployed release, e.g. `0.2.0` in production or `testing-1a2b3c4` on
/// testing; set by the deploy workflow in `MULSANNE_VERSION`. App releases
/// check production's before shipping (`GET /v1/version`).
pub fn version() -> String {
    std::env::var("MULSANNE_VERSION").unwrap_or_else(|_| "dev".into())
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
        .route("/v1/live", post(live::create))
        .route("/v1/live/{id}/frame", put(live::push_frame))
        .route("/v1/live/{id}/map", put(live::push_map))
        // route_layer: only matched routes need a signature, so unknown paths get a 404.
        .route_layer(middleware::from_fn(auth::verify_signature));

    // Downloading a trace is public; uploading one is signed and may be larger.
    let trace = get(traces::get).merge(
        put(traces::put)
            .route_layer(middleware::from_fn(auth::verify_signature))
            .route_layer(DefaultBodyLimit::max(traces::MAX_COMPRESSED)),
    );

    // Watching a live share is public and happens from the website, so it
    // allows any origin; ending one is signed. No cookies are involved.
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([axum::http::Method::GET]);
    let live_share = get(live::latest)
        .route_layer(cors.clone())
        .merge(delete(live::end).route_layer(middleware::from_fn(auth::verify_signature)));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/live/{id}", live_share)
        .route("/v1/live/{id}/events", get(live::events).route_layer(cors))
        .route("/v1/ip", get(auth::show_ip))
        .route("/v1/version", get(|| async { version() }))
        .route("/v1/leaderboards", get(laps::leaderboard))
        .route("/v1/leaderboards/boards", get(laps::boards))
        .route("/v1/laps/{id}/trace", trace)
        .merge(signed)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}
