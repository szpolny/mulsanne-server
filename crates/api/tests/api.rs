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
