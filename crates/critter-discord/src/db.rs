//! Postgres queries for the pusher's own database. Queries are checked at
//! compile time against `DATABASE_URL`, or against `.sqlx/` when
//! `SQLX_OFFLINE=true`. Run `cargo sqlx prepare` from this crate's directory
//! after changing one.

use chrono::{DateTime, Utc};
use critter_types::Kind;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// How often a webhook fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cadence {
    Hourly,
    Daily,
}

impl Cadence {
    /// The value stored in `webhooks.cadence`.
    pub fn as_str(self) -> &'static str {
        match self {
            Cadence::Hourly => "hourly",
            Cadence::Daily => "daily",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "hourly" => Some(Cadence::Hourly),
            "daily" => Some(Cadence::Daily),
            _ => None,
        }
    }

    /// The slot granularity this cadence posts.
    pub fn kind(self) -> Kind {
        match self {
            Cadence::Hourly => Kind::Hour,
            Cadence::Daily => Kind::Day,
        }
    }
}

/// A webhook registration. `cadence` is stored as text and parsed with
/// [`Cadence::parse`].
#[derive(Debug, Serialize)]
pub struct Webhook {
    pub id: Uuid,
    pub url: String,
    pub species: String,
    pub cadence: String,
    pub last_fired_slot: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Register a webhook and return the stored row.
pub async fn insert_webhook(
    pool: &PgPool,
    url: &str,
    species: &str,
    cadence: Cadence,
) -> sqlx::Result<Webhook> {
    sqlx::query_as!(
        Webhook,
        "insert into webhooks (id, url, species, cadence) values ($1, $2, $3, $4)
         returning id, url, species, cadence, last_fired_slot, created_at",
        Uuid::new_v4(),
        url,
        species,
        cadence.as_str(),
    )
    .fetch_one(pool)
    .await
}

/// Fetch one webhook by id.
pub async fn get_webhook(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Webhook>> {
    sqlx::query_as!(
        Webhook,
        "select id, url, species, cadence, last_fired_slot, created_at from webhooks where id = $1",
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Delete a webhook. Returns `false` if there was nothing to delete.
pub async fn delete_webhook(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let result = sqlx::query!("delete from webhooks where id = $1", id).execute(pool).await?;
    Ok(result.rows_affected() == 1)
}

/// Webhooks that have not yet fired for the current slot of their cadence.
pub async fn due_webhooks(
    pool: &PgPool,
    hour_slot: DateTime<Utc>,
    day_slot: DateTime<Utc>,
) -> sqlx::Result<Vec<Webhook>> {
    sqlx::query_as!(
        Webhook,
        "select id, url, species, cadence, last_fired_slot, created_at
         from webhooks
         where (cadence = 'hourly' and (last_fired_slot is null or last_fired_slot < $1))
            or (cadence = 'daily' and (last_fired_slot is null or last_fired_slot < $2))
         order by created_at",
        hour_slot,
        day_slot,
    )
    .fetch_all(pool)
    .await
}

/// Record that a webhook has handled a slot.
pub async fn mark_fired(pool: &PgPool, id: Uuid, slot: DateTime<Utc>) -> sqlx::Result<()> {
    sqlx::query!("update webhooks set last_fired_slot = $2 where id = $1", id, slot)
        .execute(pool)
        .await?;
    Ok(())
}
