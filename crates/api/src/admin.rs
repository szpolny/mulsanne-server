//! Admin dashboard API (`mulsanne.xyz/admin`): moderating the rankings and
//! display names, and watching live shares.
//!
//! The admin signs in with Steam (OpenID 2.0). Only the account in
//! `ADMIN_STEAM_ID` is let in; it is a deployment secret and never sent back.
//! A sign-in ends with a redirect to `ADMIN_WEB_URL#token=<token>`, and the
//! page sends that token as `Authorization: Bearer`. No cookies are involved,
//! so these routes allow any origin. Sessions are stored as a hash of the
//! token in `admin_sessions`, so they survive deploys.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth::ClientIp;
use crate::error::{AppError, AppResult};
use crate::ratelimit::Limiter;
use crate::AppState;

pub const STEAM_OPENID: &str = "https://steamcommunity.com/openid/login";
const OPENID_NS: &str = "http://specs.openid.net/auth/2.0";
const IDENTIFIER_SELECT: &str = "http://specs.openid.net/auth/2.0/identifier_select";
const STEAM_ID_PREFIX: &str = "https://steamcommunity.com/openid/id/";
/// A sign-in must come back from Steam within this.
const LOGIN_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_PENDING_LOGINS: usize = 1_000;
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;

pub struct Config {
    /// SteamID64 of the only account allowed in.
    steam_id: String,
    /// Where this API is reached from browsers, e.g. `https://api.mulsanne.xyz`.
    api_url: String,
    /// The dashboard page signing in returns to, e.g. `https://mulsanne.xyz/admin`.
    web_url: String,
    /// Steam's OpenID endpoint; a stand-in in tests.
    openid_endpoint: String,
    http: reqwest::Client,
    /// Sign-ins started and not yet back from Steam: state → when started.
    /// Each is used once, so a callback URL can't be replayed.
    pending: Mutex<HashMap<String, Instant>>,
    logins: Limiter<IpAddr>,
}

impl Config {
    pub fn new(steam_id: &str, api_url: &str, web_url: &str, openid_endpoint: &str) -> Result<Self, String> {
        if steam_id.len() != 17 || !steam_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err("ADMIN_STEAM_ID must be a 17-digit SteamID64".into());
        }
        for (name, url) in [("API_PUBLIC_URL", api_url), ("ADMIN_WEB_URL", web_url)] {
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(format!("{name} must be an http(s) URL"));
            }
        }
        Ok(Self {
            steam_id: steam_id.into(),
            api_url: api_url.trim_end_matches('/').into(),
            web_url: web_url.into(),
            openid_endpoint: openid_endpoint.into(),
            http: reqwest::Client::builder().timeout(Duration::from_secs(10)).build().map_err(|e| e.to_string())?,
            pending: Mutex::new(HashMap::new()),
            logins: Limiter::new(30, Duration::from_secs(3600)),
        })
    }

    /// From `ADMIN_STEAM_ID`, `API_PUBLIC_URL` and `ADMIN_WEB_URL`; `None`
    /// (admin disabled) without `ADMIN_STEAM_ID`.
    pub fn from_env() -> Result<Option<Self>, String> {
        let Some(steam_id) = std::env::var("ADMIN_STEAM_ID").ok().filter(|v| !v.trim().is_empty()) else {
            return Ok(None);
        };
        let api_url = std::env::var("API_PUBLIC_URL").map_err(|_| "API_PUBLIC_URL is not set")?;
        let web_url = std::env::var("ADMIN_WEB_URL").unwrap_or_else(|_| "https://mulsanne.xyz/admin".into());
        Self::new(steam_id.trim(), &api_url, &web_url, STEAM_OPENID).map(Some)
    }

    fn callback_url(&self, state: &str) -> String {
        format!("{}/v1/admin/steam/callback?state={state}", self.api_url)
    }

    fn back_to_web(&self, fragment: &str) -> Response {
        Redirect::to(&format!("{}#{fragment}", self.web_url)).into_response()
    }

    fn start(&self, now: Instant) -> Option<String> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, t| now.duration_since(*t) < LOGIN_TTL);
        if pending.len() >= MAX_PENDING_LOGINS {
            return None;
        }
        let state = Uuid::new_v4().simple().to_string();
        pending.insert(state.clone(), now);
        Some(state)
    }

    fn finish(&self, state: &str, now: Instant) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.remove(state).is_some_and(|t| now.duration_since(t) < LOGIN_TTL)
    }
}

fn config(state: &AppState) -> AppResult<&Config> {
    state
        .admin
        .as_deref()
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "not_found", "Admin sign-in is not set up"))
}

fn hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// `GET /v1/admin/steam/login`: sends the browser to Steam to sign in.
pub async fn login(State(state): State<AppState>, ClientIp(ip): ClientIp) -> AppResult<Response> {
    let config = config(&state)?;
    if ip.is_some_and(|ip| !config.logins.check(ip)) {
        return Err(AppError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "Too many sign-ins; try again later"));
    }
    let Some(login) = config.start(Instant::now()) else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "full",
            "Too many sign-ins in progress; try again later",
        ));
    };
    let url = reqwest::Url::parse_with_params(
        &config.openid_endpoint,
        [
            ("openid.ns", OPENID_NS),
            ("openid.mode", "checkid_setup"),
            ("openid.return_to", &config.callback_url(&login)),
            ("openid.realm", &config.api_url),
            ("openid.identity", IDENTIFIER_SELECT),
            ("openid.claimed_id", IDENTIFIER_SELECT),
        ],
    )
    .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "Bad OpenID endpoint"))?;
    Ok(Redirect::to(url.as_str()).into_response())
}

/// `GET /v1/admin/steam/callback`: Steam sends the browser back here. On
/// success it goes on to the dashboard with a session token in the URL
/// fragment (never sent to a server); otherwise with `#error=<code>`.
pub async fn callback(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let config = config(&state)?;
    let steam_id = match verify(config, &params).await {
        Ok(id) => id,
        Err(code) => return Ok(config.back_to_web(&format!("error={code}"))),
    };
    if steam_id != config.steam_id {
        tracing::warn!("refused admin sign-in from another Steam account");
        return Ok(config.back_to_web("error=forbidden"));
    }
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query("DELETE FROM admin_sessions WHERE expires_at < now()").execute(&state.db).await?;
    sqlx::query("INSERT INTO admin_sessions (token_hash, steam_id, expires_at) VALUES ($1, $2, now() + make_interval(secs => $3))")
        .bind(hash(&token))
        .bind(&steam_id)
        .bind(SESSION_TTL.as_secs_f64())
        .execute(&state.db)
        .await?;
    tracing::info!("admin signed in");
    Ok(config.back_to_web(&format!("token={token}")))
}

/// Checks a positive assertion from Steam and returns its SteamID64, or an
/// error code for the dashboard.
async fn verify(config: &Config, params: &HashMap<String, String>) -> Result<String, &'static str> {
    let get = |k: &str| params.get(k).map(String::as_str).unwrap_or("");
    if get("openid.mode") == "cancel" {
        return Err("cancelled");
    }
    let login = get("state");
    if !config.finish(login, Instant::now()) {
        return Err("expired");
    }
    if get("openid.mode") != "id_res"
        || get("openid.ns") != OPENID_NS
        || get("openid.op_endpoint") != config.openid_endpoint
        || get("openid.return_to") != config.callback_url(login)
    {
        return Err("invalid");
    }
    let claimed = get("openid.claimed_id");
    let Some(steam_id) = claimed.strip_prefix(STEAM_ID_PREFIX) else { return Err("invalid") };
    if get("openid.identity") != claimed || steam_id.len() != 17 || !steam_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid");
    }
    // The signed fields must include the identity and where it was sent.
    let signed: Vec<&str> = get("openid.signed").split(',').collect();
    if !["claimed_id", "identity", "return_to", "op_endpoint", "response_nonce"].iter().all(|f| signed.contains(f)) {
        return Err("invalid");
    }

    // Ask Steam whether it really made this assertion.
    let mut form: Vec<(&str, &str)> =
        params.iter().filter(|(k, _)| k.starts_with("openid.")).map(|(k, v)| (k.as_str(), v.as_str())).collect();
    form.retain(|(k, _)| *k != "openid.mode");
    form.push(("openid.mode", "check_authentication"));
    let res = config.http.post(&config.openid_endpoint).form(&form).send().await.map_err(|e| {
        tracing::warn!("Steam OpenID check failed: {e}");
        "steam_unavailable"
    })?;
    let body = res.text().await.map_err(|_| "steam_unavailable")?;
    if !body.lines().any(|l| l.trim() == "is_valid:true") {
        return Err("invalid");
    }
    Ok(steam_id.to_owned())
}

/// A request with a valid admin session.
pub struct Admin {
    token_hash: Vec<u8>,
}

impl FromRequestParts<AppState> for Admin {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let config = config(state)?;
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| AppError::unauthorized("Sign in with Steam"))?;
        let token_hash = hash(token.trim());
        let steam_id: Option<String> =
            sqlx::query_scalar("SELECT steam_id FROM admin_sessions WHERE token_hash = $1 AND expires_at > now()")
                .bind(&token_hash)
                .fetch_optional(&state.db)
                .await?;
        // A session of an account that is no longer the admin is refused.
        match steam_id {
            Some(id) if id == config.steam_id => Ok(Admin { token_hash }),
            _ => Err(AppError::unauthorized("Your session has ended; sign in again")),
        }
    }
}

#[derive(Serialize)]
pub struct Session {
    pub expires_at: i64,
}

/// `GET /v1/admin/session`: whether the token is still good.
pub async fn session(State(state): State<AppState>, admin: Admin) -> AppResult<Json<Session>> {
    let expires_at: i64 =
        sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM expires_at)::BIGINT FROM admin_sessions WHERE token_hash = $1")
            .bind(&admin.token_hash)
            .fetch_one(&state.db)
            .await?;
    Ok(Json(Session { expires_at }))
}

/// `DELETE /v1/admin/session`: signs out.
pub async fn logout(State(state): State<AppState>, admin: Admin) -> AppResult<StatusCode> {
    sqlx::query("DELETE FROM admin_sessions WHERE token_hash = $1").bind(&admin.token_hash).execute(&state.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn page(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    (limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT), offset.unwrap_or(0).max(0))
}

fn not_found(what: &str) -> AppError {
    AppError::new(StatusCode::NOT_FOUND, "not_found", format!("{what} not found"))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Board {
    pub track: String,
    pub car_class: String,
    pub ranked: i64,
    pub held: i64,
    pub drivers: i64,
    pub best_lap: Option<f64>,
}

/// `GET /v1/admin/boards`: every track and class with laps, held ones too.
pub async fn boards(State(state): State<AppState>, _: Admin) -> AppResult<Json<Vec<Board>>> {
    let rows = sqlx::query_as(
        "SELECT track, car_class,
                COUNT(*) FILTER (WHERE status = 'ranked') AS ranked,
                COUNT(*) FILTER (WHERE status = 'held') AS held,
                COUNT(DISTINCT user_id) AS drivers,
                MIN(lap_time) FILTER (WHERE status = 'ranked') AS best_lap
         FROM laps GROUP BY track, car_class ORDER BY track, car_class",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct LapsQuery {
    pub track: Option<String>,
    pub car_class: Option<String>,
    /// `ranked`, `held`, or all laps when missing.
    pub status: Option<String>,
    pub user: Option<Uuid>,
    /// Only each driver's best lap, as the public rankings show.
    #[serde(default)]
    pub best: bool,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Lap {
    pub id: Uuid,
    pub user_id: Uuid,
    /// The driver's name, even on laps they uploaded anonymously.
    pub driver: Option<String>,
    pub anonymous: bool,
    pub track: String,
    pub track_length: f64,
    pub car_class: String,
    pub car: String,
    pub vehicle_name: String,
    pub session_type: String,
    pub game_version: Option<i32>,
    pub lap_time: f64,
    pub s1: f64,
    pub s2: f64,
    pub s3: f64,
    pub max_speed: f64,
    pub status: String,
    pub held_reason: Option<String>,
    pub has_trace: bool,
    pub app_version: String,
    pub recorded_at: i64,
    pub created_at: i64,
    #[serde(skip)]
    pub total: i64,
}

#[derive(Serialize)]
pub struct Laps {
    pub laps: Vec<Lap>,
    pub total: i64,
}

/// `GET /v1/admin/laps`: laps fastest first.
pub async fn laps(State(state): State<AppState>, _: Admin, Query(q): Query<LapsQuery>) -> AppResult<Json<Laps>> {
    let (limit, offset) = page(q.limit, q.offset);
    let laps: Vec<Lap> = sqlx::query_as(
        "WITH f AS (
             SELECT * FROM laps
             WHERE ($1::text IS NULL OR track = $1) AND ($2::text IS NULL OR car_class = $2)
               AND ($3::text IS NULL OR status = $3) AND ($4::uuid IS NULL OR user_id = $4)
         ), picked AS (
             SELECT * FROM f WHERE NOT $5
             UNION ALL
             SELECT * FROM (SELECT DISTINCT ON (user_id) * FROM f ORDER BY user_id, lap_time, recorded_at) b WHERE $5
         )
         SELECT p.id, p.user_id, u.display_name AS driver, p.anonymous, p.track, p.track_length, p.car_class, p.car,
                p.vehicle_name, p.session_type, p.game_version, p.lap_time, p.s1, p.s2, p.s3, p.max_speed, p.status,
                p.held_reason, p.has_trace, p.app_version,
                EXTRACT(EPOCH FROM p.recorded_at)::BIGINT AS recorded_at,
                EXTRACT(EPOCH FROM p.created_at)::BIGINT AS created_at,
                COUNT(*) OVER () AS total
         FROM picked p JOIN users u ON u.id = p.user_id
         ORDER BY p.lap_time, p.recorded_at
         LIMIT $6 OFFSET $7",
    )
    .bind(&q.track)
    .bind(&q.car_class)
    .bind(&q.status)
    .bind(q.user)
    .bind(q.best)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let total = laps.first().map_or(0, |l| l.total);
    Ok(Json(Laps { laps, total }))
}

/// `DELETE /v1/admin/laps/{id}`: removes a lap (and its trace) from the
/// rankings. The app doesn't upload a lap again once the server stored it.
pub async fn delete_lap(State(state): State<AppState>, _: Admin, Path(id): Path<Uuid>) -> AppResult<StatusCode> {
    let has_trace: Option<bool> = sqlx::query_scalar("DELETE FROM laps WHERE id = $1 RETURNING has_trace")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    match (has_trace, &state.traces) {
        (None, _) => return Err(not_found("Lap")),
        (Some(true), Some(store)) => crate::traces::delete_objects(store, &[id]).await,
        _ => {}
    }
    tracing::info!("admin deleted lap {id}");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct UsersQuery {
    /// Part of a display name.
    pub q: Option<String>,
    /// Only users with a display name.
    #[serde(default)]
    pub named: bool,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub display_name: Option<String>,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub laps: i64,
    pub live_shares: i64,
    #[serde(skip)]
    pub total: i64,
}

#[derive(Serialize)]
pub struct Users {
    pub users: Vec<User>,
    pub total: i64,
}

/// `GET /v1/admin/users`: drivers, most recently seen first.
pub async fn users(State(state): State<AppState>, _: Admin, Query(q): Query<UsersQuery>) -> AppResult<Json<Users>> {
    let (limit, offset) = page(q.limit, q.offset);
    let search = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| {
        let escaped = s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        format!("%{escaped}%")
    });
    let users: Vec<User> = sqlx::query_as(
        "SELECT u.id, u.display_name,
                EXTRACT(EPOCH FROM u.created_at)::BIGINT AS created_at,
                EXTRACT(EPOCH FROM u.last_seen_at)::BIGINT AS last_seen_at,
                (SELECT COUNT(*) FROM laps l WHERE l.user_id = u.id) AS laps,
                (SELECT COUNT(*) FROM live_shares s WHERE s.user_id = u.id) AS live_shares,
                COUNT(*) OVER () AS total
         FROM users u
         WHERE ($1::text IS NULL OR u.display_name ILIKE $1) AND (NOT $2 OR u.display_name IS NOT NULL)
         ORDER BY u.last_seen_at DESC
         LIMIT $3 OFFSET $4",
    )
    .bind(search)
    .bind(q.named)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let total = users.first().map_or(0, |u| u.total);
    Ok(Json(Users { users, total }))
}

/// `DELETE /v1/admin/users/{id}/display_name`: clears a name that isn't OK,
/// so the driver shows as Anonymous. They can choose a new one in the app.
pub async fn clear_display_name(
    State(state): State<AppState>,
    _: Admin,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let cleared = sqlx::query("UPDATE users SET display_name = NULL WHERE id = $1").bind(id).execute(&state.db).await?;
    if cleared.rows_affected() == 0 {
        return Err(not_found("User"));
    }
    tracing::info!("admin cleared the display name of user {id}");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct LiveQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Serialize)]
pub struct CurrentShare {
    pub id: String,
    pub user_id: Uuid,
    pub driver: Option<String>,
    pub started_at: Option<i64>,
    pub viewers: usize,
    /// Seconds since the app last pushed.
    pub idle: u64,
    /// From the latest frame, when there is one.
    pub track: Option<String>,
    pub session: Option<String>,
    pub car: Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct PastShare {
    pub id: String,
    pub user_id: Uuid,
    pub driver: Option<String>,
    pub started_at: i64,
    /// `None` while it may still be open.
    pub ended_at: Option<i64>,
    #[sqlx(default)]
    pub live: bool,
    #[serde(skip)]
    pub total: i64,
}

#[derive(Serialize)]
pub struct Live {
    pub current: Vec<CurrentShare>,
    pub history: Vec<PastShare>,
    pub total: i64,
}

fn frame_text(frame: &Value, path: &[&str]) -> Option<String> {
    let v = path.iter().try_fold(frame, |v, k| v.get(k))?;
    v.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// `GET /v1/admin/live`: who is sharing right now, and every share newest first.
pub async fn live(State(state): State<AppState>, _: Admin, Query(q): Query<LiveQuery>) -> AppResult<Json<Live>> {
    let (limit, offset) = page(q.limit, q.offset);
    let open = state.live.snapshot(Instant::now());
    let ids: Vec<String> = open.iter().map(|s| s.id.clone()).collect();
    let started: HashMap<String, (Option<String>, i64)> = sqlx::query_as::<_, (String, Option<String>, i64)>(
        "SELECT s.id, u.display_name, EXTRACT(EPOCH FROM s.created_at)::BIGINT
         FROM live_shares s JOIN users u ON u.id = s.user_id WHERE s.id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|(id, name, at)| (id, (name, at)))
    .collect();

    let mut current: Vec<CurrentShare> = open
        .into_iter()
        .map(|s| {
            let frame = s.frame.as_deref().and_then(|f| serde_json::from_str::<Value>(f).ok()).unwrap_or(Value::Null);
            let (driver, started_at) = started.get(&s.id).cloned().map_or((None, None), |(n, at)| (n, Some(at)));
            CurrentShare {
                user_id: s.user,
                driver,
                started_at,
                viewers: s.viewers,
                idle: s.idle.as_secs(),
                track: frame_text(&frame, &["session", "track"]),
                session: frame_text(&frame, &["session", "kind"]),
                car: frame_text(&frame, &["player", "vehicleName"]),
                id: s.id,
            }
        })
        .collect();
    current.sort_by_key(|s| std::cmp::Reverse(s.started_at));

    let mut history: Vec<PastShare> = sqlx::query_as(
        "SELECT s.id, s.user_id, u.display_name AS driver,
                EXTRACT(EPOCH FROM s.created_at)::BIGINT AS started_at,
                EXTRACT(EPOCH FROM s.ended_at)::BIGINT AS ended_at,
                COUNT(*) OVER () AS total
         FROM live_shares s JOIN users u ON u.id = s.user_id
         ORDER BY s.created_at DESC
         LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    for share in &mut history {
        share.live = ids.contains(&share.id);
    }
    let total = history.first().map_or(0, |s| s.total);
    Ok(Json(Live { current, history, total }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::new("76561197960287930", "https://api.example/", "https://web.example/admin", STEAM_OPENID).unwrap()
    }

    #[test]
    fn checks_its_settings() {
        assert!(Config::new("123", "https://a", "https://b", STEAM_OPENID).is_err());
        assert!(Config::new("7656119796028793x", "https://a", "https://b", STEAM_OPENID).is_err());
        assert!(Config::new("76561197960287930", "api.example", "https://b", STEAM_OPENID).is_err());
        assert_eq!(config().callback_url("s"), "https://api.example/v1/admin/steam/callback?state=s");
    }

    #[test]
    fn sign_ins_are_used_once_and_expire() {
        let config = config();
        let t = Instant::now();
        let a = config.start(t).unwrap();
        assert!(config.finish(&a, t));
        assert!(!config.finish(&a, t));
        assert!(!config.finish("unknown", t));
        let b = config.start(t).unwrap();
        assert!(!config.finish(&b, t + LOGIN_TTL));
    }

    #[test]
    fn reads_text_from_frames() {
        let frame = serde_json::json!({ "session": { "track": "Spa", "kind": "" }, "player": null });
        assert_eq!(frame_text(&frame, &["session", "track"]).as_deref(), Some("Spa"));
        assert_eq!(frame_text(&frame, &["session", "kind"]), None);
        assert_eq!(frame_text(&frame, &["player", "vehicleName"]), None);
    }
}
