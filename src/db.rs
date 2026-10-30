//! Postgres queries. Everything that touches the schema lives here.

use sqlx::PgPool;

use crate::sources::Candidate;

/// Insert a candidate image. Returns `true` if it was new, `false` if the same
/// source id was already present.
pub async fn insert_image(pool: &PgPool, candidate: &Candidate) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "insert into images (species, source, source_id, image_url, source_url, author, author_url,
                             license, license_url, width, height)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         on conflict (source, source_id) do nothing",
    )
    .bind(candidate.species)
    .bind(candidate.source)
    .bind(&candidate.source_id)
    .bind(&candidate.image_url)
    .bind(&candidate.source_url)
    .bind(&candidate.author)
    .bind(&candidate.author_url)
    .bind(&candidate.license)
    .bind(&candidate.license_url)
    .bind(candidate.width)
    .bind(candidate.height)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Total number of images across all species.
pub async fn count_images(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar("select count(*) from images").fetch_one(pool).await
}
