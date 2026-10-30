//! Postgres queries. Everything that touches the schema lives here. Queries
//! are checked at compile time against `DATABASE_URL`, or against `.sqlx/`
//! when `SQLX_OFFLINE=true`. Run `cargo sqlx prepare` after changing one.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::sources::Candidate;

/// An image row, without the embedding and score.
#[derive(Debug)]
pub struct Image {
    pub species: String,
    pub source: String,
    pub image_url: String,
    pub source_url: String,
    pub author: Option<String>,
    pub author_url: Option<String>,
    pub license: String,
    pub license_url: Option<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

/// Insert a candidate image. Returns `true` if it was new, `false` if the same
/// source id was already present.
pub async fn insert_image(pool: &PgPool, candidate: &Candidate) -> sqlx::Result<bool> {
    let result = sqlx::query!(
        "insert into images (species, source, source_id, image_url, source_url, author, author_url,
                             license, license_url, width, height)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         on conflict (source, source_id) do nothing",
        candidate.species,
        candidate.source,
        candidate.source_id,
        candidate.image_url,
        candidate.source_url,
        candidate.author,
        candidate.author_url,
        candidate.license,
        candidate.license_url,
        candidate.width,
        candidate.height,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Total number of images across all species.
pub async fn count_images(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"select count(*) as "count!" from images"#).fetch_one(pool).await
}

/// The image pinned to a slot, if any.
pub async fn slot_image(
    pool: &PgPool,
    species: &str,
    kind: &str,
    starts_at: DateTime<Utc>,
) -> sqlx::Result<Option<Image>> {
    sqlx::query_as!(
        Image,
        "select species, source, image_url, source_url, author, author_url, license,
                license_url, width, height
         from images
         where id = (select image_id from slots where species = $1 and kind = $2 and starts_at = $3)",
        species,
        kind,
        starts_at,
    )
    .fetch_optional(pool)
    .await
}

/// Ids of images eligible for a slot, in a stable order. Without a trained
/// model every image is eligible. With one, only the top `keep_fraction` by
/// score qualifies, and unscored images are left out.
pub async fn eligible_ids(
    pool: &PgPool,
    species: &str,
    keep_fraction: f64,
) -> sqlx::Result<Vec<i64>> {
    // ponytail: loads every eligible id per unpinned slot. Fine for thousands
    // of images. Cache the cutoff per species if the pool grows large.
    sqlx::query_scalar!(
        "select id from images
         where species = $1
           and case when exists (select 1 from model)
               then score >= (select percentile_cont(1 - $2::float8) within group (order by score)
                              from images where species = $1 and score is not null)
               else true end
         order by id",
        species,
        keep_fraction,
    )
    .fetch_all(pool)
    .await
}

/// Pin an image to a slot. Does nothing if the slot is already pinned.
pub async fn pin_slot(
    pool: &PgPool,
    species: &str,
    kind: &str,
    starts_at: DateTime<Utc>,
    image_id: i64,
) -> sqlx::Result<()> {
    sqlx::query!(
        "insert into slots (species, kind, starts_at, image_id) values ($1, $2, $3, $4)
         on conflict do nothing",
        species,
        kind,
        starts_at,
        image_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}
