mod db;
mod interactions;
mod push;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Context;
use axum::{Router, routing::get};
use clap::Parser;
use serenity::{all::ApplicationId, http::HttpBuilder, interactions_endpoint::Verifier};
use sqlx::PgPool;
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
    /// The Discord application's id, from the Developer Portal.
    #[arg(long, env)]
    pub discord_application_id: u64,
    /// The application's public key, in hex. Verifies interaction requests.
    #[arg(long, env)]
    pub discord_public_key: String,
    /// The bot token. Registers commands and creates webhooks.
    #[arg(long, env)]
    pub discord_token: String,
    /// Address to bind the HTTP server to. Discord must reach
    /// `/interactions` here over public HTTPS.
    #[arg(long, env, default_value = "0.0.0.0:8081")]
    pub bind_addr: SocketAddr,
    /// Base URL of the critter-server to pull photos from.
    #[arg(long, env)]
    pub server_url: String,
    /// Public HTTPS base URL of this bot, the one Discord sends interactions
    /// to. Posts use the species icons served here as avatars. Without it,
    /// posts use the webhook's default avatar.
    #[arg(long, env)]
    pub public_url: Option<String>,
    /// How often to check for webhooks that are due, in seconds.
    #[arg(long, env, default_value_t = 60)]
    pub poll_interval_secs: u64,
    /// The user agent sent to the critter-server.
    #[arg(long, env, default_value = "critter-discord (https://github.com/kaylendog/critters)")]
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
    /// Serenity's HTTP client, authenticated as the bot.
    pub discord: Arc<serenity::http::Http>,
    /// Checks that interaction requests really come from Discord.
    pub verifier: Verifier,
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
    let args = Args::parse();

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
    // Panics on a malformed key, which is fine at startup.
    let verifier = Verifier::new(&args.config.discord_public_key);
    let discord = HttpBuilder::new(&args.config.discord_token)
        .application_id(ApplicationId::new(args.config.discord_application_id))
        .build();
    let app =
        App { pool, config: Arc::new(args.config), http, discord: Arc::new(discord), verifier };

    interactions::register(&app).await.context("registering slash commands")?;
    tokio::spawn(push::scheduler(app.clone()));

    // Setup the HTTP router and start listening for requests.
    let router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/avatars/{file}", get(push::avatar))
        .merge(interactions::router())
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
