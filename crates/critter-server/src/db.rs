//! Postgres queries. Everything that touches the schema lives here. Queries
//! are checked at compile time against `DATABASE_URL`, or against `.sqlx/`
//! when `SQLX_OFFLINE=true`. Run `cargo sqlx prepare` from this crate's
//! directory after changing one.

use chrono::{DateTime, Utc};
use serde::Serialize;
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

/// Ids and URLs of images without an embedding, oldest first.
pub async fn unembedded_images(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<(i64, String)>> {
    sqlx::query!(
        "select id, image_url from images where embedding is null order by id limit $1",
        limit,
    )
    .map(|row| (row.id, row.image_url))
    .fetch_all(pool)
    .await
}

/// Store an image's embedding.
pub async fn set_embedding(pool: &PgPool, id: i64, embedding: &[f32]) -> sqlx::Result<()> {
    sqlx::query!("update images set embedding = $2 where id = $1", id, embedding)
        .execute(pool)
        .await?;
    Ok(())
}

/// What the labelling page needs to show one candidate.
#[derive(Debug, Serialize)]
pub struct LabelImage {
    pub id: i64,
    pub image_url: String,
    pub source_url: String,
}

/// Two distinct random embedded images of a species, or `None` if there are
/// fewer than two.
pub async fn random_pair(
    pool: &PgPool,
    species: &str,
) -> sqlx::Result<Option<(LabelImage, LabelImage)>> {
    let mut rows = sqlx::query_as!(
        LabelImage,
        "select id, image_url, source_url from images
         where species = $1 and embedding is not null and not hidden
         order by random() limit 2",
        species,
    )
    .fetch_all(pool)
    .await?;
    Ok(match (rows.pop(), rows.pop()) {
        (Some(b), Some(a)) => Some((a, b)),
        _ => None,
    })
}

/// Never serve an image again, whatever its score. Any slot already pinned to
/// it is unpinned so a repeat request picks something else. Returns `false`
/// if the id does not exist.
pub async fn hide_image(pool: &PgPool, id: i64) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let result =
        sqlx::query!("update images set hidden = true where id = $1", id).execute(&mut *tx).await?;
    sqlx::query!("delete from slots where image_id = $1", id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

/// Record that one image beat another.
pub async fn insert_pair(pool: &PgPool, winner: i64, loser: i64) -> sqlx::Result<()> {
    sqlx::query!("insert into pairs (winner_id, loser_id) values ($1, $2)", winner, loser)
        .execute(pool)
        .await?;
    Ok(())
}

/// Total labelled pairs.
pub async fn count_pairs(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"select count(*) as "count!" from pairs"#).fetch_one(pool).await
}

/// Embeddings for every pair where both images are embedded, as
/// `(winner, loser)`.
pub async fn pair_embeddings(pool: &PgPool) -> sqlx::Result<Vec<(Vec<f32>, Vec<f32>)>> {
    sqlx::query!(
        r#"select w.embedding as "winner!", l.embedding as "loser!"
           from pairs p
           join images w on w.id = p.winner_id
           join images l on l.id = p.loser_id
           where w.embedding is not null and l.embedding is not null"#
    )
    .map(|row| (row.winner, row.loser))
    .fetch_all(pool)
    .await
}

/// Replace the single stored model.
pub async fn save_model(pool: &PgPool, weights: &[f32]) -> sqlx::Result<()> {
    sqlx::query!(
        "insert into model (id, weights, trained_at) values (1, $1, now())
         on conflict (id) do update set weights = excluded.weights, trained_at = now()",
        weights,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Recompute every embedded image's score as the dot product with the stored
/// model, in one statement.
pub async fn rescore(pool: &PgPool) -> sqlx::Result<()> {
    sqlx::query!(
        "update images
         set score = (select sum(e * w) from unnest(images.embedding, model.weights) as t(e, w))
         from model
         where images.embedding is not null"
    )
    .execute(pool)
    .await?;
    Ok(())
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
           and not hidden
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
