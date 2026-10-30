//! Admin routes for managing webhook registrations.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    routing::{get, post},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    App,
    db::{self, Cadence},
    require_admin,
};

/// Body of `POST /webhooks`.
#[derive(Debug, Deserialize)]
struct NewWebhook {
    url: String,
    species: String,
    cadence: Cadence,
}

/// Admin-only routes. Every route requires the bearer token.
pub fn router(app: App) -> Router<App> {
    Router::new()
        .route("/webhooks", post(create))
        .route("/webhooks/{id}", get(show).delete(remove))
        .route_layer(middleware::from_fn_with_state(app, require_admin))
}

/// `POST /webhooks` - register a webhook. 422 for an unknown species or a URL
/// that is not a Discord webhook.
async fn create(
    State(app): State<App>,
    Json(new): Json<NewWebhook>,
) -> Result<(StatusCode, Json<db::Webhook>), StatusCode> {
    if !critter_types::SPECIES.contains(&new.species.as_str()) || !is_discord_webhook(&new.url) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let webhook = db::insert_webhook(&app.pool, &new.url, &new.species, new.cadence)
        .await
        .map_err(internal)?;
    Ok((StatusCode::CREATED, Json(webhook)))
}

/// `GET /webhooks/{id}` - fetch one registration.
async fn show(
    State(app): State<App>,
    Path(id): Path<Uuid>,
) -> Result<Json<db::Webhook>, StatusCode> {
    db::get_webhook(&app.pool, id).await.map_err(internal)?.map(Json).ok_or(StatusCode::NOT_FOUND)
}

/// `DELETE /webhooks/{id}` - remove a registration. 204 on success.
async fn remove(State(app): State<App>, Path(id): Path<Uuid>) -> StatusCode {
    match db::delete_webhook(&app.pool, id).await {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(error) => internal(error),
    }
}

fn internal(error: sqlx::Error) -> StatusCode {
    tracing::error!(%error, "webhook query failed");
    StatusCode::INTERNAL_SERVER_ERROR
}

/// Only accept well-formed webhook URLs on Discord's own hosts, so the pusher
/// cannot be pointed at arbitrary servers. Serenity checks the host, the id
/// and the token shape. We additionally insist on HTTPS.
fn is_discord_webhook(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else { return false };
    parsed.scheme() == "https" && serenity::utils::parse_webhook(&parsed).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-abcd";

    #[test]
    fn only_accepts_well_formed_discord_urls() {
        assert!(is_discord_webhook(&format!(
            "https://discord.com/api/webhooks/123456789012345678/{TOKEN}"
        )));
        assert!(!is_discord_webhook(&format!(
            "https://example.com/api/webhooks/123456789012345678/{TOKEN}"
        )));
        assert!(!is_discord_webhook(&format!(
            "http://discord.com/api/webhooks/123456789012345678/{TOKEN}"
        )));
        assert!(!is_discord_webhook("https://discord.com/api/webhooks/1/fake"));
    }
}
