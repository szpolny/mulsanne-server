//! Request-level tests. Tests that touch the database are ignored by
//! default; run them with a disposable Postgres:
//! `DATABASE_URL=postgres://… cargo test -- --include-ignored`

use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use ed25519_dalek::SigningKey;
use mulsanne_api::signing::{self, HEADER_KEY, HEADER_SIGNATURE, HEADER_TIMESTAMP};
use mulsanne_api::{ApiError, Me};
use mulsanne_server::{app, AppState};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// App whose pool never connects; for requests rejected before any query.
fn offline_app() -> Router {
    let db = PgPoolOptions::new().connect_lazy("postgres://localhost/unused").unwrap();
    app(AppState::new(db, false))
}

fn signed(key: &SigningKey, method: &str, path: &str, body: Option<Value>, timestamp: i64) -> Request<Body> {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let (k, t, s) = signing::sign(key, method, path, timestamp, body.as_bytes());
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header(HEADER_KEY, k)
        .header(HEADER_TIMESTAMP, t)
        .header(HEADER_SIGNATURE, s)
        .body(Body::from(body))
        .unwrap()
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn health() {
    let req = Request::get("/healthz").body(Body::empty()).unwrap();
    let res = offline_app().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn rejects_unsigned_stale_and_tampered_requests() {
    let app = offline_app();
    let key = SigningKey::from_bytes(&[1; 32]);

    let (status, body) = send(&app, Request::get("/v1/me").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(serde_json::from_value::<ApiError>(body).unwrap().code, "unauthorized");

    let (status, _) = send(&app, signed(&key, "GET", "/v1/me", None, now() - 120)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Signed for one path, sent to another.
    let mut req = signed(&key, "GET", "/v1/me", None, now());
    *req.uri_mut() = "/v1/me?x=1".parse().unwrap();
    let (status, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rejects_oversized_bodies() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let name = "x".repeat(mulsanne_server::BODY_LIMIT);
    let (status, _) = send(&offline_app(), signed(&key, "POST", "/v1/users", Some(json!({ "display_name": name })), now())).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "needs DATABASE_URL"]
async fn user_lifecycle(db: PgPool) {
    let app = app(AppState::new(db, false));
    let key = SigningKey::from_bytes(&[2; 32]);

    // Unknown keys can't use /v1/me.
    let (status, _) = send(&app, signed(&key, "GET", "/v1/me", None, now())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = send(&app, signed(&key, "POST", "/v1/users", Some(json!({ "display_name": " Szymon " })), now())).await;
    assert_eq!(status, StatusCode::CREATED);
    let me: Me = serde_json::from_value(body).unwrap();
    assert_eq!(me.display_name.as_deref(), Some("Szymon"));

    // Registering again returns the same user.
    let (status, body) = send(&app, signed(&key, "POST", "/v1/users", Some(json!({})), now())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_value::<Me>(body).unwrap().id, me.id);

    let (status, _) = send(&app, signed(&key, "PATCH", "/v1/me", Some(json!({ "display_name": "x" })), now())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = send(&app, signed(&key, "PATCH", "/v1/me", Some(json!({ "display_name": null })), now())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_value::<Me>(body).unwrap().display_name, None);

    let (status, _) = send(&app, signed(&key, "DELETE", "/v1/me", None, now())).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, signed(&key, "GET", "/v1/me", None, now())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

fn lap_json(time: f64, recorded_at: i64, anonymous: bool) -> Value {
    json!({
        "track": "Spa", "track_length": 7004.0, "car": "Ferrari 296 LMGT3", "vehicle_name": "Ferrari #55",
        "car_class": "GT3", "session_type": "Practice", "lap_time": time,
        "s1": time * 0.3, "s2": time * 0.4, "s3": time * 0.3,
        "max_speed": 75.0, "recorded_at": recorded_at, "app_version": "0.5.0", "anonymous": anonymous,
    })
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "needs DATABASE_URL"]
async fn lap_upload_and_leaderboard(db: PgPool) {
    let app = app(AppState::new(db, false));
    let alice = SigningKey::from_bytes(&[3; 32]);
    let bob = SigningKey::from_bytes(&[4; 32]);
    for (key, name) in [(&alice, "Alice"), (&bob, "Bob")] {
        send(&app, signed(key, "POST", "/v1/users", Some(json!({ "display_name": name })), now())).await;
    }
    let t = now() - 600;

    let (status, body) = send(&app, signed(&alice, "POST", "/v1/laps", Some(lap_json(140.0, t, false)), now())).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["status"], "ranked");
    // Same lap again: stored once.
    let (status, again) = send(&app, signed(&alice, "POST", "/v1/laps", Some(lap_json(140.0, t, false)), now())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["id"], body["id"]);
    // Alice's slower lap doesn't replace her best.
    send(&app, signed(&alice, "POST", "/v1/laps", Some(lap_json(141.0, t + 200, false)), now())).await;
    let (_, bob_lap) = send(&app, signed(&bob, "POST", "/v1/laps", Some(lap_json(139.0, t, true)), now())).await;

    // More than 3% faster than the record: held.
    let (_, held) = send(&app, signed(&bob, "POST", "/v1/laps", Some(lap_json(120.0, t + 300, false)), now())).await;
    assert_eq!(held["status"], "held");

    let req = Request::get("/v1/leaderboards?track=Spa&car_class=GT3").body(Body::empty()).unwrap();
    let (status, board) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(board["total"], 2);
    assert_eq!(board["entries"][0]["driver"], Value::Null, "Bob uploaded anonymously");
    assert_eq!(board["entries"][0]["lap_time"], 139.0);
    assert_eq!(board["entries"][1]["driver"], "Alice");
    assert_eq!(board["entries"][1]["lap_time"], 140.0);

    // Only the owner can delete a lap.
    let path = format!("/v1/laps/{}", bob_lap["id"].as_str().unwrap());
    let (status, _) = send(&app, signed(&alice, "DELETE", &path, None, now())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, signed(&bob, "DELETE", &path, None, now())).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let req = Request::get("/v1/leaderboards/boards").body(Body::empty()).unwrap();
    let (_, boards) = send(&app, req).await;
    assert_eq!(boards[0]["drivers"], 1);
}

async fn register(app: &Router, seed: u8) -> SigningKey {
    let key = SigningKey::from_bytes(&[seed; 32]);
    send(app, signed(&key, "POST", "/v1/users", Some(json!({})), now())).await;
    key
}

fn with_length(time: f64, recorded_at: i64, length: f64) -> Value {
    let mut lap = lap_json(time, recorded_at, false);
    lap["track_length"] = json!(length);
    lap
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "needs DATABASE_URL"]
async fn track_lengths(db: PgPool) {
    let app = app(AppState::new(db.clone(), false));
    let t = now() - 600;

    // A first upload with a wrong length decides nothing on its own.
    let wrong = register(&app, 10).await;
    let (_, body) = send(&app, signed(&wrong, "POST", "/v1/laps", Some(with_length(150.0, t, 6500.0)), now())).await;
    assert_eq!(body["status"], "ranked");

    // Three more drivers agree on the real length; the outlier doesn't block them.
    for seed in 11..14 {
        let key = register(&app, seed).await;
        let (_, body) = send(&app, signed(&key, "POST", "/v1/laps", Some(with_length(150.0, t, 7004.0)), now())).await;
        assert_eq!(body["status"], "ranked");
    }

    // Now the consensus holds mismatching laps instead of ranking them.
    let late = register(&app, 14).await;
    let (_, body) = send(&app, signed(&late, "POST", "/v1/laps", Some(with_length(149.0, t, 6500.0)), now())).await;
    assert_eq!(body["status"], "held");

    // An admin-verified length re-checks existing laps and rejects new mismatches.
    let (held, ranked) = mulsanne_server::tracks::set_length(&db, "Spa", 6500.0).await.unwrap();
    assert_eq!((held, ranked), (3, 1));
    let (held, ranked) = mulsanne_server::tracks::set_length(&db, "Spa", 7004.0).await.unwrap();
    assert_eq!((held, ranked), (2, 3));
    let (status, _) = send(&app, signed(&late, "POST", "/v1/laps", Some(with_length(149.0, t + 1, 6500.0)), now())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../../migrations")]
#[ignore = "needs DATABASE_URL"]
async fn lap_upload_rate_limit(db: PgPool) {
    let app = app(AppState::with_lap_limit(db, false, 2));
    let key = register(&app, 20).await;
    let t = now() - 600;
    for i in 0..2 {
        let (status, _) = send(&app, signed(&key, "POST", "/v1/laps", Some(lap_json(140.0, t + i, false)), now())).await;
        assert_eq!(status, StatusCode::CREATED);
    }
    // Re-uploading a stored lap is free.
    let (status, _) = send(&app, signed(&key, "POST", "/v1/laps", Some(lap_json(140.0, t, false)), now())).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&app, signed(&key, "POST", "/v1/laps", Some(lap_json(140.0, t + 5, false)), now())).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    // Other drivers aren't affected.
    let other = register(&app, 21).await;
    let (status, _) = send(&app, signed(&other, "POST", "/v1/laps", Some(lap_json(140.0, t, false)), now())).await;
    assert_eq!(status, StatusCode::CREATED);
}
