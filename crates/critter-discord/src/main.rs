mod admin;
mod db;
mod push;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Context;
use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
    routing::get,
};
use axum_extra::{
    TypedHeader,
    headers::{Authorization, authorization::Bearer},
    typed_header::TypedHeaderRejection,
};
use clap::Parser;
use sqlx::PgPool;
use subtle::ConstantTimeEq;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

/// Command line interface for the pusher. The `about` text is pulled from
/// `Cargo.toml`.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(flatten)]
    config: Config,
}

/// Configuration for the pusher, from command line flags or environment
/// variables.
#[derive(clap::Args, Clone, Debug)]
pub struct Config {
    /// Postgres connection string for the pusher's own database.
    #[arg(long, env)]
    pub database_url: String,
    /// Bearer token for `/webhooks`. If unset, this is generated and logged
    /// on start.
    #[arg(long, env)]
    pub admin_token: Option<String>,
    /// Address to bind the admin HTTP server to.
    #[arg(long, env, default_value = "0.0.0.0:8081")]
    pub bind_addr: SocketAddr,
    /// Base URL of the critter-server to pull photos from.
    #[arg(long, env)]
    pub server_url: String,
    /// How often to check for webhooks that are due, in seconds.
    #[arg(long, env, default_value_t = 60)]
    pub poll_interval_secs: u64,
    /// The user agent sent to the critter-server.
    #[arg(
        long,
        env,
        default_value = "critter-discord (https://github.com/kaylendog/daily-critters)"
    )]
    pub user_agent: String,
}

/// The application state, shared between request handlers and the scheduler.
#[derive(Clone)]
pub struct App {
    /// The pusher's own Postgres connection pool.
    pub pool: PgPool,
    /// The configuration for the pusher.
    pub config: Arc<Config>,
    /// HTTP client for talking to the critter-server.
    pub http: reqwest::Client,
    /// Serenity's HTTP client. Webhooks need no bot token.
    pub discord: Arc<serenity::http::Http>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging and tracing.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "critter_discord=info,tower_http=info".into()),
        )
        .init();

    // Load this crate's `.env` for local development. A missing file is fine.
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    let mut args = Args::parse();

    // If the admin token is not set, generate a random one and log it.
    if args.config.admin_token.as_deref().is_none_or(str::is_empty) {
        let token: String =
            rand::random::<[u8; 32]>().iter().map(|byte| format!("{byte:02x}")).collect();
        tracing::warn!(%token, "ADMIN_TOKEN was not set, generated a random one");
        args.config.admin_token = Some(token);
    }

    // Setup the Postgres connection pool and run migrations.
    let pool =
        PgPool::connect(&args.config.database_url).await.context("connecting to postgres")?;
    sqlx::migrate!().run(&pool).await?;

    // Prepare the HTTP clients and application state.
    let http = reqwest::Client::builder()
        .user_agent(&args.config.user_agent)
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(5))
        .build()?;
    let app = App {
        pool,
        config: Arc::new(args.config),
        http,
        discord: Arc::new(serenity::http::Http::new("")),
    };

    tokio::spawn(push::scheduler(app.clone()));

    // Setup the HTTP router and start listening for requests.
    let router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(admin::router(app.clone()))
        .layer(TraceLayer::new_for_http())
        .with_state(app.clone());

    // Start the HTTP server and listen for shutdown signals.
    let listener = tokio::net::TcpListener::bind(app.config.bind_addr).await?;
    tracing::info!(addr = %app.config.bind_addr, "listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;
    Ok(())
}

/// Middleware that requires `Authorization: Bearer <ADMIN_TOKEN>`. A missing
/// or malformed header is a 401 like a wrong token, so the extractor's own
/// rejection is caught rather than returned as a 400.
pub async fn require_admin(
    State(app): State<App>,
    bearer: Result<TypedHeader<Authorization<Bearer>>, TypedHeaderRejection>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let expected = app.config.admin_token.as_deref().unwrap_or_default();
    let presented = bearer.as_ref().map(|header| header.token()).unwrap_or_default();
    let matches: bool = presented.as_bytes().ct_eq(expected.as_bytes()).into();
    if expected.is_empty() || !matches {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}
