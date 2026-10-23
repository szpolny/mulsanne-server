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

/// Client IP. Directly exposed, it's the socket address. Behind `hops`
/// trusted proxies that each append to `X-Forwarded-For`, it's the entry
/// `hops` from the right: anything further left was sent by the client and
/// can be forged.
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub Option<IpAddr>);

pub fn forwarded_ip(header: Option<&str>, hops: usize) -> Option<IpAddr> {
    let entries: Vec<&str> = header?.split(',').map(str::trim).collect();
    entries.len().checked_sub(hops).and_then(|i| entries.get(i)).and_then(|ip| ip.parse().ok())
}

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        if state.proxy_hops > 0 {
            let header = parts.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
            return Ok(ClientIp(forwarded_ip(header, state.proxy_hops)));
        }
        Ok(ClientIp(parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip())))
    }
}

/// `GET /v1/ip`: the client IP as the server sees it, for checking
/// `TRUSTED_PROXY_HOPS` on a deployment.
pub async fn show_ip(ClientIp(ip): ClientIp) -> String {
    ip.map_or_else(|| "unknown".into(), |ip| ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarded_ip_counts_trusted_hops_from_the_right() {
        let header = Some("6.6.6.6, 1.2.3.4, 35.191.0.1");
        // One proxy (appends the client's IP): the rightmost entry.
        assert_eq!(forwarded_ip(header, 1), "35.191.0.1".parse().ok());
        // Two (client IP, then a load balancer's): second from the right;
        // the forged leftmost entry is never used.
        assert_eq!(forwarded_ip(header, 2), "1.2.3.4".parse().ok());
        assert_eq!(forwarded_ip(header, 4), None);
        assert_eq!(forwarded_ip(None, 1), None);
        assert_eq!(forwarded_ip(Some("garbage"), 1), None);
    }
}
