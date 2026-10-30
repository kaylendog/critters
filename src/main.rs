use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Context;
use axum::{Router, routing::get};
use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use sqlx::PgPool;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

/// Top-level command line interface for the service. Note: the `about` text is
/// pulled from `Cargo.toml`.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(flatten)]
    config: Config,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Configuration for the service, from command line flags or environment
/// variables.
#[derive(clap::Args, Clone, Debug)]
pub struct Config {
    /// Postgres connection string, e.g. `postgres://user:pass@host/dbname`.
    #[arg(long, env)]
    pub database_url: String,
    /// Bearer token for `/label` and `/webhooks`. If unset, this is generated
    /// and logged on start.
    #[arg(long, env)]
    pub admin_token: Option<String>,
    /// Address to bind the HTTP server to.
    #[arg(long, env, default_value = "0.0.0.0:8080")]
    pub bind_addr: SocketAddr,
    /// Path to a directory where the service can store its data, including
    /// cached images and the ranker model.
    #[arg(long, env, default_value = "./data")]
    pub data_dir: PathBuf,
    /// Fraction of each species pool, by score, that is eligible for slots.
    #[arg(long, env, default_value_t = 0.5)]
    pub keep_fraction: f64,
    /// How often to ingest new images from Wikimedia Commons, in hours.
    #[arg(long, env, default_value_t = 6)]
    pub ingest_interval_hours: u64,
    /// How many images to fetch per species when seeding the database.
    #[arg(long, env, default_value_t = 300)]
    pub seed_per_species: usize,
    /// Minimum date for images to consider, in YYYY-MM-DD format. Requests for
    /// images older than this will be ignored.
    #[arg(long, env, default_value = "2026-01-01")]
    pub min_slot: NaiveDate,
    /// Comma-separated licence allowlist. Defaults to CC-BY-SA and CC-BY.
    #[arg(long, env, value_delimiter = ',')]
    pub licenses: Option<Vec<String>>,
    /// The user agent to use when fetching images from Wikimedia Commons.
    /// Defaults containing the project name and a link to the source code.
    #[arg(
        long,
        env,
        default_value = "daily-critters (https://github.com/kaylendog/daily-critters)"
    )]
    pub user_agent: String,
}

/// An enumeration of subcommands supported by the service.
#[derive(Subcommand)]
enum Command {
    /// Run the HTTP API, ingest timer and webhook scheduler (default).
    Serve,
    /// Fetch candidate images once and exit.
    Seed {
        #[arg(long)]
        per_species: Option<usize>,
    },
    /// Retrain the ranker from stored pairs and rescore every image.
    Train,
}

/// The application state, shared between all request handlers.
#[derive(Clone)]
pub struct App {
    /// The Postgres connection pool.
    pub pool: PgPool,
    /// The configuration for the service.
    pub config: Arc<Config>,
    /// A shared HTTP client for fetching images from upstream sources.
    pub http: reqwest::Client,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging and tracing.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "daily_critters=info,tower_http=info".into()),
        )
        .init();

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

    // Prepare the HTTP client and application state.
    let http = reqwest::Client::builder().user_agent(&args.config.user_agent).build()?;
    let app = App { pool, config: Arc::new(args.config), http };

    match args.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(app).await,
        Command::Seed { per_species: _ } => anyhow::bail!("seed: not implemented yet"),
        Command::Train => anyhow::bail!("train: not implemented yet"),
    }
}

/// Serve the HTTP API and run the ingest timer and webhook scheduler.
async fn serve(app: App) -> anyhow::Result<()> {
    // Setup the HTTP router and start listening for requests.
    let router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
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
