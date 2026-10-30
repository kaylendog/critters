mod api;
mod db;
mod sources;

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

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
    /// Bearer token for `/label`. If unset, this is generated and logged on
    /// start.
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
    /// How often to ingest new images from Wikimedia Commons and iNaturalist,
    /// in hours.
    #[arg(long, env, default_value_t = 6)]
    pub ingest_interval_hours: u64,
    /// How many images to fetch per species and per source on each ingest.
    #[arg(long, env, default_value_t = 300)]
    pub seed_per_species: usize,
    /// Earliest slot date served, in YYYY-MM-DD format. Requests for slots
    /// before this return 404, so public traffic cannot fill the slots table.
    #[arg(long, env, default_value = "2026-01-01")]
    pub min_slot: NaiveDate,
    /// Comma-separated licence families to accept, e.g. `CC0,CC-BY`. Defaults
    /// to every family that permits non-commercial redistribution.
    #[arg(long, env, value_delimiter = ',')]
    pub licenses: Option<Vec<String>>,
    /// The user agent sent to upstream APIs. Wikimedia asks for contact
    /// details here. Defaults to the project name and a link to the source.
    #[arg(
        long,
        env,
        default_value = "critter-server (https://github.com/kaylendog/daily-critters)"
    )]
    pub user_agent: String,
}

/// An enumeration of subcommands supported by the service.
#[derive(Subcommand)]
enum Command {
    /// Run the HTTP API and the ingest timer (default).
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
                .unwrap_or_else(|_| "critter_server=info,tower_http=info".into()),
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

    // Prepare the HTTP client and application state.
    let http = reqwest::Client::builder().user_agent(&args.config.user_agent).build()?;
    let app = App { pool, config: Arc::new(args.config), http };

    match args.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(app).await,
        Command::Seed { per_species } => {
            sources::ingest(&app, per_species.unwrap_or(app.config.seed_per_species)).await
        }
        Command::Train => anyhow::bail!("train: not implemented yet"),
    }
}

/// Serve the HTTP API and run the ingest timer.
async fn serve(app: App) -> anyhow::Result<()> {
    tokio::spawn(ingest_loop(app.clone()));

    // Setup the HTTP router and start listening for requests.
    let router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api::router())
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

/// Ingest on a timer. An empty database is seeded straight away, otherwise the
/// first run waits a full interval.
async fn ingest_loop(app: App) {
    let interval = Duration::from_secs(app.config.ingest_interval_hours * 3600);
    let empty = db::count_images(&app.pool).await.map(|count| count == 0).unwrap_or(false);
    if !empty {
        tokio::time::sleep(interval).await;
    }
    loop {
        if let Err(error) = sources::ingest(&app, app.config.seed_per_species).await {
            tracing::error!(%error, "ingest failed");
        }
        tokio::time::sleep(interval).await;
    }
}
