use std::net::SocketAddr;

use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "mulsanne_server=info,tower_http=info".into()))
        .init();

    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let bind: SocketAddr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()).parse()?;
    let trust_forwarded = std::env::var("TRUST_FORWARDED").is_ok_and(|v| v == "1" || v == "true");

    let db = PgPoolOptions::new().max_connections(10).connect(&database_url).await?;
    sqlx::migrate!("../../migrations").run(&db).await?;

    let app = mulsanne_server::app(mulsanne_server::AppState::new(db, trust_forwarded));
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("listening on {bind}");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
