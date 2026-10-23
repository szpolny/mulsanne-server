use std::net::{IpAddr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::extract::{ConnectInfo, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use mulsanne_api::signing::{self, HEADER_KEY, HEADER_SIGNATURE, HEADER_TIMESTAMP};
use uuid::Uuid;

use crate::error::AppError;
use crate::{traces, AppState, BODY_LIMIT};

/// Public key of a request that passed [`verify_signature`].
#[derive(Debug, Clone, Copy)]
pub struct SignedKey(pub [u8; 32]);

/// Verifies the signature headers against the method, path and body, then
/// passes the request on with a [`SignedKey`] extension.
pub async fn verify_signature(req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let limit = if parts.uri.path().ends_with("/trace") { traces::MAX_COMPRESSED } else { BODY_LIMIT };
    let Ok(body) = to_bytes(body, limit).await else {
        return AppError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "Request body is too large").into_response();
    };
    let header = |name| parts.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("");
    let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);

    let key = match signing::verify(
        header(HEADER_KEY),
        header(HEADER_TIMESTAMP),
        header(HEADER_SIGNATURE),
        parts.method.as_str(),
        path,
        &body,
        now,
    ) {
        Ok(key) => key,
        Err(e) => return AppError::unauthorized(e.message()).into_response(),
    };

    let mut req = Request::from_parts(parts, Body::from(body));
    req.extensions_mut().insert(SignedKey(key.to_bytes()));
    next.run(req).await
}

impl<S: Send + Sync> FromRequestParts<S> for SignedKey {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, AppError> {
        parts
            .extensions
            .get::<SignedKey>()
            .copied()
            .ok_or_else(|| AppError::unauthorized("Request is not signed"))
    }
}

/// A registered user making a signed request.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub id: Uuid,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let SignedKey(key) = SignedKey::from_request_parts(parts, state).await?;
        let id: Option<Uuid> = sqlx::query_scalar(
            "UPDATE users SET last_seen_at = now() WHERE public_key = $1 RETURNING id",
        )
        .bind(&key[..])
        .fetch_optional(&state.db)
        .await?;
        id.map(|id| AuthUser { id })
            .ok_or_else(|| AppError::unauthorized("This key is not registered; enable online features again"))
    }
}

/// Client IP: the socket address, or the first `X-Forwarded-For` hop when
/// the server runs behind a trusted proxy.
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub Option<IpAddr>);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let forwarded = state
            .trust_forwarded
            .then(|| parts.headers.get("x-forwarded-for")?.to_str().ok()?.split(',').next()?.trim().parse().ok())
            .flatten();
        let socket = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
        Ok(ClientIp(forwarded.or(socket)))
    }
}
