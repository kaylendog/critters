//! Slot endpoints. One photo per species per hour or day, chosen on first
//! request and pinned forever after.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::Redirect,
    routing::get,
};
use chrono::{DateTime, NaiveDate, Utc};
use critter_types::{Kind, Photo};
use sqlx::PgPool;

use crate::{
    App, Config, db,
    sources::{self, Species},
};

/// Build the slot payload from an image row.
fn photo(image: db::Image, kind: Kind, slot: DateTime<Utc>) -> Photo {
    Photo {
        species: image.species,
        kind,
        slot,
        image_url: image.image_url,
        source_url: image.source_url,
        source: image.source,
        author: image.author,
        author_url: image.author_url,
        license: image.license,
        license_url: image.license_url,
        width: image.width,
        height: image.height,
    }
}

/// Routes for daily and hourly slots, plus `/image` variants that redirect
/// straight to the picture.
pub fn router() -> Router<App> {
    Router::new()
        .route("/species", get(species))
        .route("/{species}/{year}/{month}/{day}", get(day))
        .route("/{species}/{year}/{month}/{day}/image", get(day_image))
        .route("/{species}/{year}/{month}/{day}/{hour}", get(hour))
        .route("/{species}/{year}/{month}/{day}/{hour}/image", get(hour_image))
}

/// Path parameters for a daily slot.
type DayPath = Path<(String, i32, u32, u32)>;
/// Path parameters for an hourly slot.
type HourPath = Path<(String, i32, u32, u32, u32)>;

/// `GET /species` - the species slugs this server serves.
async fn species() -> Json<&'static [&'static str]> {
    Json(critter_types::SPECIES)
}

/// `GET /{species}/{year}/{month}/{day}` - the day's photo as JSON.
async fn day(State(app): State<App>, Path((species, y, m, d)): DayPath) -> Response {
    lookup(&app, &species, y, m, d, None).await.map(Json)
}

/// `GET /{species}/{year}/{month}/{day}/image` - redirect to the day's picture.
async fn day_image(State(app): State<App>, Path((species, y, m, d)): DayPath) -> ImageResponse {
    lookup(&app, &species, y, m, d, None).await.map(|photo| Redirect::temporary(&photo.image_url))
}

/// `GET /{species}/{year}/{month}/{day}/{hour}` - the hour's photo as JSON.
async fn hour(State(app): State<App>, Path((species, y, m, d, h)): HourPath) -> Response {
    lookup(&app, &species, y, m, d, Some(h)).await.map(Json)
}

/// `GET /{species}/{year}/{month}/{day}/{hour}/image` - redirect to the hour's
/// picture.
async fn hour_image(
    State(app): State<App>,
    Path((species, y, m, d, h)): HourPath,
) -> ImageResponse {
    lookup(&app, &species, y, m, d, Some(h))
        .await
        .map(|photo| Redirect::temporary(&photo.image_url))
}

type Response = Result<Json<Photo>, StatusCode>;
type ImageResponse = Result<Redirect, StatusCode>;

/// Validate the path and resolve the slot. Unknown species, impossible dates,
/// future slots and slots before `min_slot` are 404. An empty pool is 503.
async fn lookup(
    app: &App,
    species: &str,
    year: i32,
    month: u32,
    day: u32,
    hour: Option<u32>,
) -> Result<Photo, StatusCode> {
    let species = sources::species(species).ok_or(StatusCode::NOT_FOUND)?;
    let date = NaiveDate::from_ymd_opt(year, month, day).ok_or(StatusCode::NOT_FOUND)?;
    let (kind, starts_at) =
        slot_start(app.config.min_slot, Utc::now(), date, hour).ok_or(StatusCode::NOT_FOUND)?;

    match pick_for_slot(&app.pool, &app.config, species, kind, starts_at).await {
        Ok(Some(photo)) => Ok(photo),
        Ok(None) => Err(StatusCode::SERVICE_UNAVAILABLE),
        Err(error) => {
            tracing::error!(%error, species = species.slug, "slot lookup failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Turn a date and optional hour into a slot start, or `None` if the slot is
/// out of range.
fn slot_start(
    min_slot: NaiveDate,
    now: DateTime<Utc>,
    date: NaiveDate,
    hour: Option<u32>,
) -> Option<(Kind, DateTime<Utc>)> {
    let (kind, time) = match hour {
        Some(hour) => (Kind::Hour, date.and_hms_opt(hour, 0, 0)?),
        None => (Kind::Day, date.and_hms_opt(0, 0, 0)?),
    };
    let starts_at = time.and_utc();
    (date >= min_slot && starts_at <= now).then_some((kind, starts_at))
}

/// The photo for a slot. Returns the pinned image if one exists, otherwise
/// picks one from the eligible pool and pins it. `None` means the pool is
/// empty.
pub async fn pick_for_slot(
    pool: &PgPool,
    config: &Config,
    species: &Species,
    kind: Kind,
    starts_at: DateTime<Utc>,
) -> anyhow::Result<Option<Photo>> {
    if let Some(image) = db::slot_image(pool, species.slug, kind.as_str(), starts_at).await? {
        return Ok(Some(photo(image, kind, starts_at)));
    }

    // Pick by hashing the slot, so the same slot picks the same image even if
    // two requests race. The insert ignores conflicts and we re-read, so both
    // callers see whichever pin landed first.
    let ids = db::eligible_ids(pool, species.slug, config.keep_fraction).await?;
    if ids.is_empty() {
        return Ok(None);
    }
    let key = format!("{}/{}/{}", species.slug, kind.as_str(), starts_at.to_rfc3339());
    let chosen = ids[(fnv1a(key.as_bytes()) % ids.len() as u64) as usize];
    db::pin_slot(pool, species.slug, kind.as_str(), starts_at, chosen).await?;

    let image = db::slot_image(pool, species.slug, kind.as_str(), starts_at)
        .await?
        .ok_or_else(|| anyhow::anyhow!("slot vanished after pinning"))?;
    Ok(Some(photo(image, kind, starts_at)))
}

/// 64-bit FNV-1a. Stable across Rust versions, unlike `DefaultHasher`.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn fnv1a_matches_reference_vectors() {
        assert_eq!(fnv1a(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn slot_start_rejects_out_of_range() {
        let min = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 30, 15, 30, 0).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();

        let (kind, at) = slot_start(min, now, today, Some(15)).unwrap();
        assert_eq!((kind, at), (Kind::Hour, Utc.with_ymd_and_hms(2026, 10, 30, 15, 0, 0).unwrap()));
        assert_eq!(slot_start(min, now, today, None).unwrap().0, Kind::Day);

        // Later today, a hour that does not exist, and before the minimum.
        assert!(slot_start(min, now, today, Some(16)).is_none());
        assert!(slot_start(min, now, today, Some(24)).is_none());
        let old = NaiveDate::from_ymd_opt(2025, 12, 31).unwrap();
        assert!(slot_start(min, now, old, None).is_none());
    }
}
