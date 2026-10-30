//! The scheduler. Once a minute, fetch the current slot's photo from the
//! critter-server for every webhook that has not seen it yet, and post it.

use std::time::Duration;

use chrono::{DateTime, Utc};
use critter_types::{Kind, Photo};
use serenity::{
    builder::{CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, ExecuteWebhook},
    http::{Http, HttpError},
    model::webhook::Webhook,
};

use crate::{
    App,
    db::{self, Cadence},
    interactions,
};

/// Check for due webhooks on an interval, forever.
pub async fn scheduler(app: App) {
    let mut timer = tokio::time::interval(Duration::from_secs(app.config.poll_interval_secs));
    loop {
        timer.tick().await;
        if let Err(error) = fire_due(&app).await {
            tracing::error!(%error, "scheduler failed");
        }
    }
}

/// Post the current slot's photo to every webhook that has not seen it yet.
async fn fire_due(app: &App) -> anyhow::Result<()> {
    let now = Utc::now();
    let due = db::due_webhooks(
        &app.pool,
        Kind::Hour.slot_containing(now),
        Kind::Day.slot_containing(now),
    )
    .await?;

    for webhook in due {
        // Rows are validated on insert, so a parse failure here means the
        // database was edited by hand. Skip rather than crash the loop.
        let Some(cadence) = Cadence::parse(&webhook.cadence) else {
            tracing::warn!(id = %webhook.id, "webhook row is malformed, skipping");
            continue;
        };
        let kind = cadence.kind();
        let slot = kind.slot_containing(now);

        // The server answers 503 while its pool is empty. Try again next tick.
        let photo = match fetch_photo(app, &webhook.species, kind, slot).await {
            Ok(photo) => photo,
            Err(error) => {
                tracing::warn!(id = %webhook.id, %error, "could not fetch photo, will retry");
                continue;
            }
        };

        // Retry transient failures next tick. A 4xx means the registration
        // itself is bad, so give up on this slot rather than hammer Discord.
        match post(&app.discord, &webhook.url, &photo).await {
            Ok(()) => {
                tracing::info!(id = %webhook.id, %slot, "webhook delivered");
                db::mark_fired(&app.pool, webhook.id, slot).await?;
            }
            // The webhook was deleted, or the bot was removed with it. Every
            // subscription sharing it is dead.
            Err(error) if interactions::status(&error) == Some(404) => {
                tracing::warn!(id = %webhook.id, "webhook is gone, dropping its subscriptions");
                db::delete_webhook_url(&app.pool, &webhook.url).await?;
            }
            Err(error) if is_permanent(&error) => {
                tracing::warn!(id = %webhook.id, %error, "webhook rejected, skipping slot");
                db::mark_fired(&app.pool, webhook.id, slot).await?;
            }
            Err(error) => tracing::warn!(id = %webhook.id, %error, "webhook post failed"),
        }
    }
    Ok(())
}

/// GET the slot's photo from the critter-server.
pub async fn fetch_photo(
    app: &App,
    species: &str,
    kind: Kind,
    slot: DateTime<Utc>,
) -> anyhow::Result<Photo> {
    let url = photo_url(&app.config.server_url, species, kind, slot);
    Ok(app.http.get(url).send().await?.error_for_status()?.json().await?)
}

/// Join the server base URL and the slot path, tolerating a trailing slash.
fn photo_url(server_url: &str, species: &str, kind: Kind, slot: DateTime<Utc>) -> String {
    format!("{}{}", server_url.trim_end_matches('/'), kind.path(species, slot))
}

/// Execute the webhook with a single embed.
async fn post(discord: &Http, url: &str, photo: &Photo) -> serenity::Result<()> {
    let webhook = Webhook::from_url(discord, url).await?;
    webhook.execute(discord, false, ExecuteWebhook::new().embed(embed(photo))).await?;
    Ok(())
}

/// An embed with the photo, a link back to the source page, the author, and
/// the licence in the footer.
pub fn embed(photo: &Photo) -> CreateEmbed {
    let source = match photo.source.as_str() {
        "commons" => "Wikimedia Commons",
        "inaturalist" => "iNaturalist",
        other => other,
    };
    let mut embed = CreateEmbed::new()
        .title(format!("{} of the {}", capitalise(&photo.species), photo.kind.as_str()))
        .url(&photo.source_url)
        .image(&photo.image_url)
        .footer(CreateEmbedFooter::new(format!("{} · {source}", photo.license)));
    if let Some(author) = &photo.author {
        let mut author = CreateEmbedAuthor::new(author);
        if let Some(url) = &photo.author_url {
            author = author.url(url);
        }
        embed = embed.author(author);
    }
    embed
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Whether retrying is pointless. Either Discord answered with a 4xx, or the
/// stored URL does not parse as a webhook at all.
fn is_permanent(error: &serenity::Error) -> bool {
    match error {
        serenity::Error::Http(HttpError::UnsuccessfulRequest(response)) => {
            response.status_code.is_client_error()
        }
        serenity::Error::Url(_) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    fn fox() -> Photo {
        Photo {
            species: "fox".into(),
            kind: Kind::Hour,
            slot: Utc.with_ymd_and_hms(2026, 10, 30, 15, 0, 0).unwrap(),
            image_url: "https://upload.wikimedia.org/fox.jpg".into(),
            source_url: "https://commons.wikimedia.org/wiki/File:fox.jpg".into(),
            source: "commons".into(),
            author: Some("Alexis".into()),
            author_url: Some("https://commons.wikimedia.org/wiki/User:Alexis".into()),
            license: "CC-BY-4.0".into(),
            license_url: None,
            width: None,
            height: None,
        }
    }

    #[test]
    fn embed_carries_attribution() {
        assert_eq!(
            serde_json::to_value(embed(&fox())).unwrap(),
            json!({
                "type": "rich",
                "title": "Fox of the hour",
                "url": "https://commons.wikimedia.org/wiki/File:fox.jpg",
                "image": {
                    "url": "https://upload.wikimedia.org/fox.jpg",
                    "proxy_url": null,
                    "width": null,
                    "height": null,
                },
                "author": { "name": "Alexis", "url": "https://commons.wikimedia.org/wiki/User:Alexis" },
                "footer": { "text": "CC-BY-4.0 · Wikimedia Commons" },
            })
        );
    }

    #[test]
    fn photo_url_joins_cleanly() {
        let slot = Utc.with_ymd_and_hms(2026, 10, 30, 15, 0, 0).unwrap();
        assert_eq!(
            photo_url("http://server:8080/", "fox", Kind::Hour, slot),
            "http://server:8080/fox/2026/10/30/15"
        );
        assert_eq!(
            photo_url("http://server:8080", "wolf", Kind::Day, slot),
            "http://server:8080/wolf/2026/10/30"
        );
    }
}
