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
/// source id was already present. An existing image gets its author refreshed,
/// so parser fixes reach old rows on the next ingest.
pub async fn insert_image(pool: &PgPool, candidate: &Candidate) -> sqlx::Result<bool> {
    // `xmax = 0` is only true for freshly inserted rows, not updated ones.
    sqlx::query_scalar!(
        r#"insert into images as i (species, source, source_id, image_url, source_url, author,
                                    author_url, license, license_url, width, height)
           values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
           on conflict (source, source_id)
           do update set author = excluded.author, author_url = excluded.author_url
           returning (i.xmax = 0) as "inserted!""#,
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
    .fetch_one(pool)
    .await
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
    /// Current ranker score. Used to pick pairs, not shown on the page.
    #[serde(skip)]
    pub score: Option<f32>,
}

/// Up to `limit` random embedded, visible images of a species. With
/// `unlabelled_only`, images that appear in any pair are skipped, which is
/// what validation needs.
pub async fn random_candidates(
    pool: &PgPool,
    species: &str,
    unlabelled_only: bool,
    limit: i64,
) -> sqlx::Result<Vec<LabelImage>> {
    sqlx::query_as!(
        LabelImage,
        "select id, image_url, source_url, score from images
         where species = $1 and embedding is not null and not hidden
           and ($2 = false or not exists (
               select 1 from pairs where winner_id = images.id or loser_id = images.id))
         order by random() limit $3",
        species,
        unlabelled_only,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// How many past winners a hidden image is recorded as losing to.
const HIDE_LOSSES: i64 = 5;

/// Never serve an image again, whatever its score. Any slot already pinned to
/// it is unpinned so a repeat request picks something else. Returns `false`
/// if the id does not exist.
///
/// Hiding also teaches the ranker. The image is recorded as losing to the
/// `opponent` it was shown beside, and to a handful of random past winners of
/// the same species. A handful rather than all of them, so one hide cannot
/// outweigh hundreds of real verdicts.
///
/// With `validation`, the loss to `opponent` is a validation pair and the
/// synthetic losses are skipped, so nothing reaches training.
pub async fn hide_image(
    pool: &PgPool,
    id: i64,
    opponent: Option<i64>,
    validation: bool,
) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let result =
        sqlx::query!("update images set hidden = true where id = $1", id).execute(&mut *tx).await?;
    if result.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query!("delete from slots where image_id = $1", id).execute(&mut *tx).await?;

    if let Some(opponent) = opponent.filter(|opponent| *opponent != id) {
        sqlx::query!(
            "insert into pairs (winner_id, loser_id, validation) values ($1, $2, $3)",
            opponent,
            id,
            validation,
        )
        .execute(&mut *tx)
        .await?;
    }
    if validation {
        tx.commit().await?;
        return Ok(true);
    }
    sqlx::query!(
        "insert into pairs (winner_id, loser_id)
         select winner_id, $1 from (
             select distinct p.winner_id from pairs p
             join images w on w.id = p.winner_id
             where w.species = (select species from images where id = $1)
               and not w.hidden and p.winner_id <> $1
         ) winners
         order by random() limit $2",
        id,
        HIDE_LOSSES,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(true)
}

/// Whether an image currently qualifies for slots, by the same rule as
/// [`eligible_ids`]. `None` if the id does not exist.
pub async fn is_eligible(pool: &PgPool, id: i64, keep_fraction: f64) -> sqlx::Result<Option<bool>> {
    sqlx::query_scalar!(
        r#"select (not i.hidden and coalesce(
                     case when exists (select 1 from model)
                          then i.score >= (select percentile_cont(1 - $2::float8) within group (order by score)
                                           from images where species = i.species and score is not null)
                          else true end,
                     false)) as "eligible!"
           from images i where i.id = $1"#,
        id,
        keep_fraction,
    )
    .fetch_optional(pool)
    .await
}

/// One row of the ranking page.
#[derive(Debug, Serialize)]
pub struct RankedImage {
    pub id: i64,
    pub score: Option<f32>,
    pub image_url: String,
    pub source_url: String,
    pub hidden: bool,
    /// Whether the image currently qualifies for slots.
    pub eligible: bool,
}

/// Every image of a species, best first, with the same eligibility rule as
/// [`eligible_ids`]. Hidden images sort last.
pub async fn ranking(
    pool: &PgPool,
    species: &str,
    keep_fraction: f64,
) -> sqlx::Result<Vec<RankedImage>> {
    sqlx::query_as!(
        RankedImage,
        r#"with cutoff as (
               select percentile_cont(1 - $2::float8) within group (order by score) as score
               from images where species = $1 and score is not null
           )
           select id, score, image_url, source_url, hidden,
                  (not hidden and coalesce(
                      case when exists (select 1 from model)
                           then score >= (select score from cutoff)
                           else true end,
                      false)) as "eligible!"
           from images
           where species = $1
           order by hidden, score desc nulls last, id"#,
        species,
        keep_fraction,
    )
    .fetch_all(pool)
    .await
}

/// Record that one image beat another.
pub async fn insert_pair(
    pool: &PgPool,
    winner: i64,
    loser: i64,
    validation: bool,
) -> sqlx::Result<()> {
    sqlx::query!(
        "insert into pairs (winner_id, loser_id, validation) values ($1, $2, $3)",
        winner,
        loser,
        validation,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Current scores of two images, `None` where unscored.
pub async fn pair_scores(
    pool: &PgPool,
    winner: i64,
    loser: i64,
) -> sqlx::Result<(Option<f32>, Option<f32>)> {
    let row = sqlx::query!(
        "select (select score from images where id = $1) as winner,
                (select score from images where id = $2) as loser",
        winner,
        loser,
    )
    .fetch_one(pool)
    .await?;
    Ok((row.winner, row.loser))
}

/// How many validation pairs, or training pairs when `validation` is false,
/// exist and how many the current scores agree with.
pub async fn agreement(pool: &PgPool, validation: bool) -> sqlx::Result<(i64, i64)> {
    let row = sqlx::query!(
        r#"select count(*) as "total!",
                  count(*) filter (where w.score > l.score) as "agreed!"
           from pairs p
           join images w on w.id = p.winner_id
           join images l on l.id = p.loser_id
           where p.validation = $1"#,
        validation,
    )
    .fetch_one(pool)
    .await?;
    Ok((row.total, row.agreed))
}

/// One recorded retrain.
#[derive(Debug, Serialize)]
pub struct TrainingRun {
    pub trained_at: DateTime<Utc>,
    pub pairs: i32,
    pub training_agreed: i32,
    pub validation_total: i32,
    pub validation_agreed: i32,
}

/// Record a retrain and the accuracy of the model it produced.
pub async fn insert_training_run(pool: &PgPool, run: &TrainingRun) -> sqlx::Result<()> {
    sqlx::query!(
        "insert into training_runs (trained_at, pairs, training_agreed, validation_total, validation_agreed)
         values ($1, $2, $3, $4, $5)",
        run.trained_at,
        run.pairs,
        run.training_agreed,
        run.validation_total,
        run.validation_agreed,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Every recorded retrain, newest first.
pub async fn training_runs(pool: &PgPool) -> sqlx::Result<Vec<TrainingRun>> {
    sqlx::query_as!(
        TrainingRun,
        "select trained_at, pairs, training_agreed, validation_total, validation_agreed
         from training_runs order by trained_at desc",
    )
    .fetch_all(pool)
    .await
}

/// Turn every validation pair into a training pair. Returns how many moved.
pub async fn promote_validation(pool: &PgPool) -> sqlx::Result<u64> {
    let result =
        sqlx::query!("update pairs set validation = false where validation").execute(pool).await?;
    Ok(result.rows_affected())
}

/// Total labelled pairs.
pub async fn count_pairs(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"select count(*) as "count!" from pairs"#).fetch_one(pool).await
}

/// Embeddings for every training pair where both images are embedded, as
/// `(winner, loser)`. Validation pairs are left out.
pub async fn pair_embeddings(pool: &PgPool) -> sqlx::Result<Vec<(Vec<f32>, Vec<f32>)>> {
    sqlx::query!(
        r#"select w.embedding as "winner!", l.embedding as "loser!"
           from pairs p
           join images w on w.id = p.winner_id
           join images l on l.id = p.loser_id
           where w.embedding is not null and l.embedding is not null and not p.validation"#
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

/// Everything learned from labelling, as pretty-printed JSON. Includes the
/// model, every pair, and the images those pairs and hides refer to. Images
/// are keyed by `(source, source_id)`, so the file can be imported into a
/// database where ids differ.
pub async fn export(pool: &PgPool) -> sqlx::Result<String> {
    sqlx::query_scalar!(
        r#"select jsonb_pretty(jsonb_build_object(
               'weights', (select to_jsonb(weights) from model),
               'images', coalesce((
                   select jsonb_agg(jsonb_build_object(
                       'species', species, 'source', source, 'source_id', source_id,
                       'image_url', image_url, 'source_url', source_url,
                       'author', author, 'author_url', author_url,
                       'license', license, 'license_url', license_url,
                       'width', width, 'height', height, 'hidden', hidden) order by id)
                   from images i
                   where hidden or exists (select 1 from pairs p where i.id in (p.winner_id, p.loser_id))
               ), '[]'),
               'pairs', coalesce((
                   select jsonb_agg(jsonb_build_object(
                       'winner_source', w.source, 'winner_source_id', w.source_id,
                       'loser_source', l.source, 'loser_source_id', l.source_id,
                       'validation', p.validation, 'created_at', p.created_at) order by p.id)
                   from pairs p
                   join images w on w.id = p.winner_id
                   join images l on l.id = p.loser_id
               ), '[]')
           )) as "export!""#
    )
    .fetch_one(pool)
    .await
}

/// Load a file written by [`export`]. Images are inserted if missing, and
/// marked hidden if hidden in either place. Refuses a database that already
/// has pairs, since importing twice would count every verdict twice. Returns
/// `(new images, pairs)`.
pub async fn import(pool: &PgPool, json: &str) -> anyhow::Result<(u64, u64)> {
    let mut tx = pool.begin().await?;
    let existing = sqlx::query_scalar!(r#"select count(*) as "count!" from pairs"#)
        .fetch_one(&mut *tx)
        .await?;
    anyhow::ensure!(existing == 0, "database already has {existing} pairs");

    // `xmax = 0` is only true for freshly inserted rows, not updated ones.
    let images = sqlx::query_scalar!(
        r#"insert into images as i (species, source, source_id, image_url, source_url, author,
                                    author_url, license, license_url, width, height, hidden)
           select * from jsonb_to_recordset($1::text::jsonb -> 'images') as t(
               species text, source text, source_id text, image_url text, source_url text,
               author text, author_url text, license text, license_url text,
               width int, height int, hidden bool)
           on conflict (source, source_id) do update set hidden = i.hidden or excluded.hidden
           returning (xmax = 0) as "inserted!""#,
        json,
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .filter(|inserted| *inserted)
    .count() as u64;

    let pairs = sqlx::query!(
        "insert into pairs (winner_id, loser_id, validation, created_at)
         select w.id, l.id, t.validation, t.created_at
         from jsonb_to_recordset($1::text::jsonb -> 'pairs') as t(
             winner_source text, winner_source_id text, loser_source text,
             loser_source_id text, validation bool, created_at timestamptz)
         join images w on (w.source, w.source_id) = (t.winner_source, t.winner_source_id)
         join images l on (l.source, l.source_id) = (t.loser_source, t.loser_source_id)",
        json,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // The weights only depend on CLIP, so they score new images as they are.
    sqlx::query!(
        "insert into model (id, weights)
         select 1, array(select jsonb_array_elements_text($1::text::jsonb -> 'weights')::real)
         where jsonb_typeof($1::text::jsonb -> 'weights') = 'array'
         on conflict (id) do update set weights = excluded.weights, trained_at = now()",
        json,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((images, pairs))
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
