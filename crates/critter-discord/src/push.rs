//! The scheduler. Once a minute, fetch the current slot's photo from the
//! critter-server for every webhook that has not seen it yet, and post it.

use std::time::Duration;

use chrono::{DateTime, NaiveTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use critter_types::{Kind, Photo};
use reqwest::StatusCode;
use serde_json::json;

use crate::{
    App,
    db::{self, Cadence},
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
    let due = db::due_webhooks(&app.pool, Kind::Hour.slot_containing(now)).await?;

    for webhook in due {
        // Rows are validated on insert, so a parse failure here means the
        // database was edited by hand. Skip rather than crash the loop.
        let (Some(cadence), Ok(timezone)) =
            (Cadence::parse(&webhook.cadence), webhook.timezone.parse::<Tz>())
        else {
            tracing::warn!(id = %webhook.id, "webhook row is malformed, skipping");
            continue;
        };
        let kind = cadence.kind();
        let slot = match cadence {
            Cadence::Hourly => kind.slot_containing(now),
            Cadence::Daily => {
                match due_day(now, webhook.post_at, timezone, webhook.last_fired_slot) {
                    Some(slot) => slot,
                    None => continue,
                }
            }
        };

        // The server answers 503 while its pool is empty. Try again next tick.
        let photo = match fetch_photo(app, &webhook.species, kind, slot).await {
            Ok(photo) => photo,
            Err(error) => {
                tracing::warn!(id = %webhook.id, %error, "could not fetch photo, will retry");
                continue;
            }
        };

        // Retry transient failures and rate limits next tick. Any other 4xx
        // means the message itself is bad, so give up on this slot rather
        // than hammer Discord.
        let result = post(app, &webhook.url, &photo).await;
        match result.as_ref().map_err(reqwest::Error::status) {
            Ok(()) => {
                tracing::info!(id = %webhook.id, %slot, "webhook delivered");
                db::mark_fired(&app.pool, webhook.id, slot).await?;
            }
            // The webhook was deleted, or the bot was removed with it. Every
            // subscription sharing it is dead.
            Err(Some(StatusCode::NOT_FOUND)) => {
                tracing::warn!(id = %webhook.id, "webhook is gone, dropping its subscriptions");
                db::delete_webhook_url(&app.pool, &webhook.url).await?;
            }
            Err(Some(status))
                if status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS =>
            {
                tracing::warn!(id = %webhook.id, %status, "webhook rejected, skipping slot");
                db::mark_fired(&app.pool, webhook.id, slot).await?;
            }
            Err(_) => tracing::warn!(id = %webhook.id, error = ?result, "webhook post failed"),
        }
    }
    Ok(())
}

/// The day slot a daily subscription should post now, if any. Today's local
/// post time must have passed, and its UTC day must not have been posted yet.
fn due_day(
    now: DateTime<Utc>,
    post_at: NaiveTime,
    timezone: Tz,
    last_fired: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    let local = now.with_timezone(&timezone).date_naive().and_time(post_at);
    // A time inside a DST gap does not exist that day, so post an hour later.
    let fire = timezone
        .from_local_datetime(&local)
        .earliest()
        .or_else(|| timezone.from_local_datetime(&(local + TimeDelta::hours(1))).earliest())?
        .with_timezone(&Utc);
    let slot = Kind::Day.slot_containing(fire);
    (now >= fire && last_fired.is_none_or(|last| last < slot)).then_some(slot)
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

/// Execute the webhook. Serenity has no Components V2 types yet, so this is a
/// plain request. Webhooks drop `components` without `with_components`.
/// Errors carry no URL, since the URL holds the webhook's token.
pub async fn post(app: &App, url: &str, photo: &Photo) -> reqwest::Result<()> {
    app.http
        .post(url)
        .query(&[("with_components", "true")])
        .json(&message(photo, 0))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?;
    Ok(())
}

/// Marks a message as using Components V2. Content and embeds are then
/// not allowed.
pub const IS_COMPONENTS_V2: u64 = 1 << 15;

/// A Components V2 message: one container holding the photo, then a link to
/// the source page, the author and the licence. `flags` are added to
/// [`IS_COMPONENTS_V2`].
pub fn message(photo: &Photo, flags: u64) -> serde_json::Value {
    let mut caption =
        vec![format!("[Link]({})", photo.source_url.replace('(', "%28").replace(')', "%29"))];
    caption.extend(photo.author.as_deref().map(escape));
    caption.push(escape(&photo.license));
    json!({
        "flags": IS_COMPONENTS_V2 | flags,
        "components": [{
            "type": 17, // Container
            "components": [
                {
                    "type": 12, // Media gallery
                    "items": [{ "media": { "url": photo.image_url }, "description": photo.species }],
                },
                { "type": 10, "content": caption.join(" · ") }, // Text display
            ],
        }],
    })
}

/// Escape inline Discord markdown, so an author named `_foo_` is not
/// italicised. Line-start syntax like `#` cannot trigger mid-caption.
fn escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| ("\\*_~`|[]()".contains(c).then_some('\\')).into_iter().chain([c]))
        .collect()
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
    fn message_carries_attribution() {
        assert_eq!(
            message(&fox(), 0),
            json!({
                "flags": 32768,
                "components": [{
                    "type": 17,
                    "components": [
                        {
                            "type": 12,
                            "items": [{
                                "media": { "url": "https://upload.wikimedia.org/fox.jpg" },
                                "description": "fox",
                            }],
                        },
                        {
                            "type": 10,
                            "content": "[Link](https://commons.wikimedia.org/wiki/File:fox.jpg) · Alexis · CC-BY-4.0",
                        },
                    ],
                }],
            })
        );
    }

    #[test]
    fn caption_survives_markdown_in_metadata() {
        let photo = Photo {
            source_url: "https://commons.wikimedia.org/wiki/File:Fox_(1).jpg".into(),
            author: Some("_Alexis_ [bot]".into()),
            ..fox()
        };
        assert_eq!(
            message(&photo, 64)["components"][0]["components"][1]["content"],
            r"[Link](https://commons.wikimedia.org/wiki/File:Fox_%281%29.jpg) · \_Alexis\_ \[bot\] · CC-BY-4.0"
        );
        let anonymous = Photo { author: None, ..fox() };
        assert_eq!(
            message(&anonymous, 0)["components"][0]["components"][1]["content"],
            "[Link](https://commons.wikimedia.org/wiki/File:fox.jpg) · CC-BY-4.0"
        );
    }

    #[test]
    fn daily_posts_follow_local_time() {
        let at = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        let utc = |d, h, m| Utc.with_ymd_and_hms(2026, 10, d, h, m, 0).unwrap();
        let day = |d| Utc.with_ymd_and_hms(2026, 10, d, 0, 0, 0).unwrap();
        let stockholm = chrono_tz::Europe::Stockholm;

        // 09:00 in Stockholm is 08:00 UTC after the end of summer time.
        assert_eq!(due_day(utc(30, 7, 59), at(9, 0), stockholm, None), None);
        assert_eq!(due_day(utc(30, 8, 0), at(9, 0), stockholm, None), Some(day(30)));
        assert_eq!(due_day(utc(30, 8, 0), at(9, 0), stockholm, Some(day(30))), None);
        assert_eq!(due_day(utc(31, 8, 0), at(9, 0), stockholm, Some(day(30))), Some(day(31)));

        // 00:30 in Tokyo is 15:30 UTC the day before, so it posts that day.
        assert_eq!(due_day(utc(29, 16, 0), at(0, 30), chrono_tz::Asia::Tokyo, None), Some(day(29)));

        // 02:30 does not exist when summer time starts, so it posts an hour
        // late.
        let spring = Utc.with_ymd_and_hms(2026, 3, 29, 1, 29, 0).unwrap();
        assert_eq!(due_day(spring, at(2, 30), stockholm, None), None);
        assert_eq!(
            due_day(spring + TimeDelta::minutes(1), at(2, 30), stockholm, None),
            Some(Utc.with_ymd_and_hms(2026, 3, 29, 0, 0, 0).unwrap())
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
