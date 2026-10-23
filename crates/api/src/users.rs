use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use mulsanne_api::{normalize_display_name, Me, RegisterRequest, UpdateMeRequest};
use uuid::Uuid;

use crate::auth::{AuthUser, ClientIp, SignedKey};
use crate::error::{AppError, AppResult};
use crate::AppState;

const ME_COLUMNS: &str = "id, display_name, EXTRACT(EPOCH FROM created_at)::BIGINT";

type MeRow = (Uuid, Option<String>, i64);

fn to_me((id, display_name, created_at): MeRow) -> Me {
    Me { id: id.to_string(), display_name, created_at }
}

fn display_name(name: Option<&str>) -> AppResult<Option<String>> {
    normalize_display_name(name).map_err(AppError::bad_request)
}

/// Registers the signing key, or returns the existing user for a known key
/// (so re-enabling online features keeps the same identity).
pub async fn register(
    State(state): State<AppState>,
    SignedKey(key): SignedKey,
    ClientIp(ip): ClientIp,
    Json(req): Json<RegisterRequest>,
) -> AppResult<(StatusCode, Json<Me>)> {
    let name = display_name(req.display_name.as_deref())?;

    let existing: Option<MeRow> = sqlx::query_as(&format!("SELECT {ME_COLUMNS} FROM users WHERE public_key = $1"))
        .bind(&key[..])
        .fetch_optional(&state.db)
        .await?;
    if let Some(row) = existing {
        return Ok((StatusCode::OK, Json(to_me(row))));
    }

    if let Some(ip) = ip {
        if !state.registrations.check(ip) {
            return Err(AppError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Too many new registrations from this network; try again later",
            ));
        }
    }

    // ON CONFLICT covers two concurrent registrations of the same key.
    let row: MeRow = sqlx::query_as(&format!(
        "INSERT INTO users (public_key, display_name) VALUES ($1, $2)
         ON CONFLICT (public_key) DO UPDATE SET public_key = EXCLUDED.public_key
         RETURNING {ME_COLUMNS}"
    ))
    .bind(&key[..])
    .bind(name)
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(to_me(row))))
}

pub async fn me(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Me>> {
    let row: MeRow = sqlx::query_as(&format!("SELECT {ME_COLUMNS} FROM users WHERE id = $1"))
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(to_me(row)))
}

pub async fn update_me(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<UpdateMeRequest>,
) -> AppResult<Json<Me>> {
    let name = display_name(req.display_name.as_deref())?;
    let row: MeRow = sqlx::query_as(&format!("UPDATE users SET display_name = $2 WHERE id = $1 RETURNING {ME_COLUMNS}"))
        .bind(user.id)
        .bind(name)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(to_me(row)))
}

/// Deletes the user. Everything they upload later references `users(id)`
/// with `ON DELETE CASCADE`, so this removes all of their data.
pub async fn delete_me(State(state): State<AppState>, user: AuthUser) -> AppResult<StatusCode> {
    sqlx::query("DELETE FROM users WHERE id = $1").bind(user.id).execute(&state.db).await?;
    Ok(StatusCode::NO_CONTENT)
}
