use std::net::SocketAddr;

use mulsanne_server::tracks;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage:
  mulsanne-server                          run the API server
  mulsanne-server tracks list              show uploaded and verified track lengths
  mulsanne-server tracks set <track> <m>   set a verified length and re-check its laps
  mulsanne-server tracks unset <track>     go back to the drivers' consensus";

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "mulsanne_server=info,tower_http=info".into()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let db = PgPoolOptions::new().max_connections(10).connect(&database_url).await?;
    sqlx::migrate!("../../migrations").run(&db).await?;

    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => serve(db).await,
        ["tracks", rest @ ..] => tracks_command(&db, rest).await,
        _ => Err(USAGE.into()),
    }
}

async fn serve(db: PgPool) -> Result<()> {
    let bind: SocketAddr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()).parse()?;
    let trust_forwarded = std::env::var("TRUST_FORWARDED").is_ok_and(|v| v == "1" || v == "true");
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

async fn tracks_command(db: &PgPool, args: &[&str]) -> Result<()> {
    match args {
        ["list"] => {
            for t in tracks::list(db).await? {
                let length = |v: Option<f64>| v.map_or("-".into(), |v| format!("{v:.1} m"));
                println!(
                    "{}\n  verified: {}  median: {}  drivers: {}  held for length: {}",
                    t.track,
                    length(t.verified),
                    length(t.median),
                    t.drivers,
                    t.held_for_length
                );
                for (length, drivers) in t.lengths {
                    println!("    {length:.1} m  ({drivers} drivers)");
                }
            }
        }
        ["set", track, length] => {
            let length: f64 = length.parse().map_err(|_| "length must be a number of metres")?;
            if !(1_000.0..=25_000.0).contains(&length) {
                return Err("length must be between 1000 and 25000 m".into());
            }
            let (held, ranked) = tracks::set_length(db, track, length).await?;
            println!("{track}: verified length {length:.1} m; {held} laps held, {ranked} laps ranked again");
        }
        ["unset", track] => {
            if tracks::unset_length(db, track).await? {
                println!("{track}: verified length removed");
            } else {
                println!("{track} has no verified length");
            }
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}
