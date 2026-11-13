//! Live timing shares: the app pushes a JSON frame about once a second, and
//! anyone with the link watches it in a browser over server-sent events.
//!
//! Frames live in memory only (a [`watch`] channel per share), so this needs
//! a single server instance. The `live_shares` table records each share's
//! owner, so after a restart the owner's next push brings the share back
//! under the same link and viewers' `EventSource`s reconnect to it.
//!
//! The frame and track map are opaque to the server: it checks that they are
//! JSON objects and relays them. Their schema belongs to the app and the
//! viewer page (`mulsanne-web`, `/live`).

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::stream::{self, Stream};
use mulsanne_api::{LivePushed, LiveShareCreated};
use serde_json::Value;
use tokio::sync::watch;
use uuid::Uuid;

use crate::auth::{AuthUser, SignedKey};
use crate::error::{AppError, AppResult};
use crate::AppState;

/// A share ends this long after it was created, even if the app keeps
/// pushing; long enough for a 24-hour race with its warmup.
pub const MAX_AGE: Duration = Duration::from_secs(26 * 3600);
/// A share ends when the app stops pushing for this long (closed or crashed).
pub const IDLE: Duration = Duration::from_secs(3 * 60);
/// Frames closer together than this are rejected; the app sends one a second.
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(200);
pub const MAX_SHARES: usize = 1_000;
pub const MAX_VIEWERS_PER_SHARE: usize = 200;
pub const MAX_VIEWERS: usize = 5_000;
/// New shares per user per hour.
pub const SHARES_PER_HOUR: usize = 60;
const KEEP_ALIVE: Duration = Duration::from_secs(15);

type Payload = Option<Arc<str>>;

struct Share {
    owner: [u8; 32],
    user: Uuid,
    expires: Instant,
    activity: Mutex<Activity>,
    frame: watch::Sender<Payload>,
    map: watch::Sender<Payload>,
    viewers: Arc<AtomicUsize>,
}

impl Share {
    fn new(owner: [u8; 32], user: Uuid, opened: Instant, expires: Instant) -> Self {
        Self {
            owner,
            user,
            expires,
            activity: Mutex::new(Activity { last_frame: None, last_map: None, last_seen: opened }),
            frame: watch::Sender::new(None),
            map: watch::Sender::new(None),
            viewers: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn viewers(&self) -> usize {
        self.viewers.load(Ordering::Relaxed)
    }

    fn activity(&self) -> std::sync::MutexGuard<'_, Activity> {
        self.activity.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// When the owner last pushed. Frames and maps are spaced separately, so a
/// map sent along with the first frame on a new track doesn't hold it back.
struct Activity {
    last_frame: Option<Instant>,
    last_map: Option<Instant>,
    /// The last push of either kind, or when the share opened.
    last_seen: Instant,
}

impl Activity {
    /// Records a push at `now`, unless the previous one of its kind was too recent.
    fn admit(last: &mut Option<Instant>, last_seen: &mut Instant, now: Instant) -> Result<(), PushError> {
        if last.is_some_and(|t| now.duration_since(t) < MIN_FRAME_INTERVAL) {
            return Err(PushError::TooFast);
        }
        *last = Some(now);
        *last_seen = now;
        Ok(())
    }
}

/// Every open share. Dropping a share drops its senders, which ends its
/// viewers' event streams.
#[derive(Default)]
pub struct Hub {
    shares: Mutex<HashMap<String, Arc<Share>>>,
    viewers: Arc<AtomicUsize>,
}

/// Why a push to a share was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum PushError {
    NotFound,
    NotOwner,
    TooFast,
}

impl From<PushError> for AppError {
    fn from(e: PushError) -> Self {
        match e {
            PushError::NotFound => not_found(),
            // Same answer as an unknown id, so a viewer can't probe whose share it is.
            PushError::NotOwner => not_found(),
            PushError::TooFast => {
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "Live frames are sent too often")
            }
        }
    }
}

fn not_found() -> AppError {
    AppError::new(StatusCode::NOT_FOUND, "not_found", "This live session has ended or the link is wrong")
}

impl Hub {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Share>>> {
        self.shares.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, id: &str) -> Option<Arc<Share>> {
        self.lock().get(id).cloned()
    }

    /// Opens a share, ending any other share of the same user. Returns
    /// `false` when the server is full.
    pub fn open(&self, id: &str, owner: [u8; 32], user: Uuid, now: Instant, expires: Instant) -> bool {
        let mut shares = self.lock();
        shares.retain(|_, s| s.user != user);
        if shares.len() >= MAX_SHARES {
            return false;
        }
        shares.insert(id.to_owned(), Arc::new(Share::new(owner, user, now, expires)));
        true
    }

    pub fn end(&self, id: &str) {
        self.lock().remove(id);
    }

    pub fn end_user(&self, user: Uuid) {
        self.lock().retain(|_, s| s.user != user);
    }

    pub fn contains(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    fn owned(&self, id: &str, key: &[u8; 32]) -> Result<Arc<Share>, PushError> {
        let share = self.get(id).ok_or(PushError::NotFound)?;
        if &share.owner != key {
            return Err(PushError::NotOwner);
        }
        Ok(share)
    }

    /// Publishes a frame; returns the number of viewers and whether a map was sent.
    pub fn push_frame(&self, id: &str, key: &[u8; 32], frame: Arc<str>, now: Instant) -> Result<(usize, bool), PushError> {
        let share = self.owned(id, key)?;
        {
            let a = &mut *share.activity();
            Activity::admit(&mut a.last_frame, &mut a.last_seen, now)?;
        }
        share.frame.send_replace(Some(frame));
        let has_map = share.map.borrow().is_some();
        Ok((share.viewers(), has_map))
    }

    pub fn push_map(&self, id: &str, key: &[u8; 32], map: Arc<str>, now: Instant) -> Result<(), PushError> {
        let share = self.owned(id, key)?;
        {
            let a = &mut *share.activity();
            Activity::admit(&mut a.last_map, &mut a.last_seen, now)?;
        }
        share.map.send_replace(Some(map));
        Ok(())
    }

    /// Ends shares past [`MAX_AGE`], and shares whose app stopped pushing for
    /// [`IDLE`] (counted from when they were opened if it never pushed).
    pub fn sweep(&self, now: Instant) {
        self.lock().retain(|_, s| {
            now < s.expires && now.duration_since(s.activity().last_seen) < IDLE
        });
    }

    /// Runs [`Hub::sweep`] every half minute.
    pub async fn sweep_forever(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            self.sweep(Instant::now());
        }
    }

    /// The latest frame and map, and the viewer count.
    fn latest(&self, id: &str) -> Option<(Payload, Payload, usize)> {
        let share = self.get(id)?;
        let frame = share.frame.borrow().clone();
        let map = share.map.borrow().clone();
        Some((frame, map, share.viewers()))
    }

    /// Subscribes a viewer, or `None` if the share doesn't exist; `Err` when
    /// there are too many viewers.
    fn watch(&self, id: &str) -> Option<Result<Viewer, AppError>> {
        let share = self.get(id)?;
        let full = || AppError::new(StatusCode::SERVICE_UNAVAILABLE, "full", "Too many people are watching; try again later");
        let Some(share_guard) = Guard::acquire(&share.viewers, MAX_VIEWERS_PER_SHARE) else {
            return Some(Err(full()));
        };
        let Some(total_guard) = Guard::acquire(&self.viewers, MAX_VIEWERS) else {
            return Some(Err(full()));
        };
        let mut frame = share.frame.subscribe();
        let mut map = share.map.subscribe();
        let mut pending = VecDeque::new();
        if let Some(m) = map.borrow_and_update().clone() {
            pending.push_back(event("map", &m));
        }
        if let Some(f) = frame.borrow_and_update().clone() {
            pending.push_back(event("frame", &f));
        }
        Some(Ok(Viewer { frame, map, pending, done: false, _guards: (share_guard, total_guard) }))
    }
}

/// Counts a viewer while it's alive.
struct Guard(Arc<AtomicUsize>);

impl Guard {
    fn acquire(count: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        count.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < max).then_some(n + 1)).ok()?;
        Some(Self(count.clone()))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn event(name: &str, data: &str) -> Event {
    Event::default().event(name).data(data)
}

/// One browser's event stream. Holds receivers only, so ending the share
/// (dropping its senders) ends the stream with an `end` event.
struct Viewer {
    frame: watch::Receiver<Payload>,
    map: watch::Receiver<Payload>,
    pending: VecDeque<Event>,
    done: bool,
    _guards: (Guard, Guard),
}

impl Viewer {
    async fn next(&mut self) -> Option<Event> {
        loop {
            if let Some(e) = self.pending.pop_front() {
                return Some(e);
            }
            if self.done {
                return None;
            }
            let (name, changed) = tokio::select! {
                r = self.map.changed() => ("map", r.map(|_| self.map.borrow_and_update().clone())),
                r = self.frame.changed() => ("frame", r.map(|_| self.frame.borrow_and_update().clone())),
            };
            match changed {
                Ok(Some(data)) => self.pending.push_back(event(name, &data)),
                Ok(None) => {}
                Err(_) => {
                    self.done = true;
                    self.pending.push_back(Event::default().event("end").data("{}"));
                }
            }
        }
    }

    fn into_stream(self) -> impl Stream<Item = Result<Event, Infallible>> {
        stream::unfold(self, |mut v| async move { v.next().await.map(|e| (Ok(e), v)) })
    }
}

/// Re-serializes a JSON object compactly: one line, as SSE `data` needs.
fn compact_object(body: &[u8], what: &str) -> AppResult<Arc<str>> {
    match serde_json::from_slice::<Value>(body) {
        Ok(v @ Value::Object(_)) => Ok(v.to_string().into()),
        _ => Err(AppError::bad_request(format!("The {what} must be a JSON object"))),
    }
}

fn full_server() -> AppError {
    AppError::new(StatusCode::SERVICE_UNAVAILABLE, "full", "Too many live sessions right now; try again later")
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `POST /v1/live`: opens a share for the signed-in user, ending their
/// previous one.
pub async fn create(State(state): State<AppState>, SignedKey(key): SignedKey, user: AuthUser) -> AppResult<(StatusCode, Json<LiveShareCreated>)> {
    if !state.live_creates.check(user.id) {
        return Err(AppError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "Too many live sessions started; try again later"));
    }
    // 122 random bits; the id is the viewers' only credential.
    let id = Uuid::new_v4().simple().to_string();
    let expires_at = unix_now() + MAX_AGE.as_secs() as i64;

    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE live_shares SET ended_at = now() WHERE user_id = $1 AND ended_at IS NULL")
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO live_shares (id, user_id, expires_at) VALUES ($1, $2, to_timestamp($3))")
        .bind(&id)
        .bind(user.id)
        .bind(expires_at as f64)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let now = Instant::now();
    if !state.live.open(&id, key, user.id, now, now + MAX_AGE) {
        return Err(full_server());
    }
    Ok((StatusCode::CREATED, Json(LiveShareCreated { id, expires_at })))
}

/// Brings back an open share of `key` after a server restart.
async fn restore(state: &AppState, id: &str, key: &[u8; 32]) -> AppResult<()> {
    if state.live.contains(id) {
        return Ok(());
    }
    let row: Option<(Uuid, Vec<u8>, i64)> = sqlx::query_as(
        "SELECT s.user_id, u.public_key, EXTRACT(EPOCH FROM s.expires_at)::BIGINT
         FROM live_shares s JOIN users u ON u.id = s.user_id
         WHERE s.id = $1 AND s.ended_at IS NULL AND s.expires_at > now()",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some((user, owner, expires_at)) = row else { return Err(not_found()) };
    if owner.as_slice() != key {
        return Err(not_found());
    }
    let left = Duration::from_secs((expires_at - unix_now()).max(0) as u64);
    let now = Instant::now();
    if !state.live.open(id, *key, user, now, now + left) {
        return Err(full_server());
    }
    tracing::info!("restored live share after a restart");
    Ok(())
}

/// `PUT /v1/live/{id}/frame`: the latest timing frame.
pub async fn push_frame(
    State(state): State<AppState>,
    SignedKey(key): SignedKey,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> AppResult<Json<LivePushed>> {
    let frame = compact_object(&body, "frame")?;
    restore(&state, &id, &key).await?;
    let (viewers, has_map) = state.live.push_frame(&id, &key, frame, Instant::now())?;
    Ok(Json(LivePushed { viewers, has_map }))
}

/// `PUT /v1/live/{id}/map`: the track outline, sent when the track changes.
pub async fn push_map(
    State(state): State<AppState>,
    SignedKey(key): SignedKey,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> AppResult<StatusCode> {
    let map = compact_object(&body, "track map")?;
    restore(&state, &id, &key).await?;
    state.live.push_map(&id, &key, map, Instant::now())?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /v1/live/{id}`: stops sharing; viewers get an `end` event.
pub async fn end(State(state): State<AppState>, SignedKey(key): SignedKey, Path(id): Path<String>) -> AppResult<StatusCode> {
    let ended = sqlx::query(
        "UPDATE live_shares s SET ended_at = now() FROM users u
         WHERE s.id = $1 AND u.id = s.user_id AND u.public_key = $2 AND s.ended_at IS NULL",
    )
    .bind(&id)
    .bind(&key[..])
    .execute(&state.db)
    .await?;
    if ended.rows_affected() == 0 && state.live.owned(&id, &key).is_err() {
        return Err(not_found());
    }
    state.live.end(&id);
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/live/{id}`: the latest frame and map, `null` until sent.
pub async fn latest(State(state): State<AppState>, Path(id): Path<String>) -> AppResult<Response> {
    let (frame, map, viewers) = state.live.latest(&id).ok_or_else(not_found)?;
    // Both are stored as validated, compact JSON.
    let body = format!(
        r#"{{"frame":{},"map":{},"viewers":{viewers}}}"#,
        frame.as_deref().unwrap_or("null"),
        map.as_deref().unwrap_or("null"),
    );
    Ok(([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body).into_response())
}

/// `GET /v1/live/{id}/events`: server-sent events. `map` and `frame` carry
/// the latest payloads (each sent right away if there is one), and `end`
/// means the share is over.
pub async fn events(State(state): State<AppState>, Path(id): Path<String>) -> AppResult<Response> {
    let viewer = state.live.watch(&id).ok_or_else(not_found)??;
    let sse = Sse::new(viewer.into_stream()).keep_alive(KeepAlive::new().interval(KEEP_ALIVE));
    // Ask proxies not to buffer the stream.
    Ok(([("x-accel-buffering", "no")], sse).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [1; 32];
    const OTHER: [u8; 32] = [2; 32];

    fn hub_with(id: &str, now: Instant) -> Hub {
        let hub = Hub::default();
        assert!(hub.open(id, KEY, Uuid::nil(), now, now + MAX_AGE));
        hub
    }

    #[test]
    fn only_the_owner_pushes_and_pushes_are_spaced() {
        let t = Instant::now();
        let hub = hub_with("a", t);
        assert_eq!(hub.push_frame("a", &OTHER, "{}".into(), t), Err(PushError::NotOwner));
        assert_eq!(hub.push_frame("nope", &KEY, "{}".into(), t), Err(PushError::NotFound));
        assert_eq!(hub.push_frame("a", &KEY, "{}".into(), t), Ok((0, false)));
        assert_eq!(hub.push_frame("a", &KEY, "{}".into(), t + Duration::from_millis(50)), Err(PushError::TooFast));
        assert_eq!(hub.push_frame("a", &KEY, "{}".into(), t + MIN_FRAME_INTERVAL), Ok((0, false)));
    }

    #[test]
    fn opening_a_share_ends_the_users_previous_one() {
        let t = Instant::now();
        let hub = hub_with("a", t);
        assert!(hub.open("b", KEY, Uuid::nil(), t, t + MAX_AGE));
        assert!(!hub.contains("a"));
        assert!(hub.contains("b"));
    }

    #[test]
    fn sweep_ends_idle_and_expired_shares() {
        let t = Instant::now();
        let hub = hub_with("a", t);
        hub.push_frame("a", &KEY, "{}".into(), t).unwrap();
        hub.sweep(t + IDLE - Duration::from_secs(1));
        assert!(hub.contains("a"));
        hub.sweep(t + IDLE);
        assert!(!hub.contains("a"));

        // Never pushed: idle from when it opened.
        let hub = hub_with("b", t);
        hub.sweep(t + IDLE);
        assert!(!hub.contains("b"));

        // Pushing doesn't keep a share past its expiry.
        let hub = Hub::default();
        hub.open("c", KEY, Uuid::nil(), t, t + Duration::from_secs(10));
        hub.push_frame("c", &KEY, "{}".into(), t + Duration::from_secs(10)).unwrap();
        hub.sweep(t + Duration::from_secs(10));
        assert!(!hub.contains("c"));
    }

    #[tokio::test]
    async fn viewers_get_the_latest_state_then_updates_then_end() {
        let t = Instant::now();
        let hub = hub_with("a", t);
        hub.push_map("a", &KEY, r#"{"m":1}"#.into(), t).unwrap();
        hub.push_frame("a", &KEY, r#"{"f":1}"#.into(), t).unwrap();

        let mut viewer = hub.watch("a").unwrap().unwrap();
        assert_eq!(hub.latest("a").unwrap().2, 1);
        let names = |e: Event| format!("{e:?}");
        assert!(names(viewer.next().await.unwrap()).contains("map"));
        assert!(names(viewer.next().await.unwrap()).contains(r#"{\"f\":1}"#));

        hub.push_frame("a", &KEY, r#"{"f":2}"#.into(), t + MIN_FRAME_INTERVAL).unwrap();
        assert!(names(viewer.next().await.unwrap()).contains(r#"{\"f\":2}"#));

        hub.end("a");
        assert!(names(viewer.next().await.unwrap()).contains("end"));
        assert!(viewer.next().await.is_none());
        drop(viewer);
        assert_eq!(hub.viewers.load(Ordering::Relaxed), 0);
        assert!(hub.watch("a").is_none());
    }

    #[test]
    fn viewer_limit_per_share() {
        let hub = hub_with("a", Instant::now());
        let viewers: Vec<_> = (0..MAX_VIEWERS_PER_SHARE).map(|_| hub.watch("a").unwrap().unwrap()).collect();
        assert!(hub.watch("a").unwrap().is_err());
        drop(viewers);
        assert!(hub.watch("a").unwrap().is_ok());
    }

    #[test]
    fn frames_must_be_json_objects() {
        assert_eq!(&*compact_object(b"{ \"a\" :\n 1 }", "frame").unwrap(), r#"{"a":1}"#);
        assert!(compact_object(b"[1]", "frame").is_err());
        assert!(compact_object(b"nope", "frame").is_err());
    }
}
