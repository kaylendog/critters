//! Postgres queries for the pusher's own database. Queries are checked at
//! compile time against `DATABASE_URL`, or against `.sqlx/` when
//! `SQLX_OFFLINE=true`. Run `cargo sqlx prepare` from this crate's directory
//! after changing one.

use chrono::{DateTime, Utc};
use critter_types::Kind;
use sqlx::PgPool;
use uuid::Uuid;

/// How often a webhook fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// A subscription that is due. `cadence` is stored as text and parsed with
/// [`Cadence::parse`].
#[derive(Debug)]
pub struct Webhook {
    pub id: Uuid,
    pub url: String,
    pub species: String,
    pub cadence: String,
}

/// Subscribe a channel. Returns `false` if it already had this subscription.
pub async fn insert_webhook(
    pool: &PgPool,
    guild: i64,
    channel: i64,
    url: &str,
    species: &str,
    cadence: Cadence,
) -> sqlx::Result<bool> {
    let result = sqlx::query!(
        "insert into webhooks (id, guild_id, channel_id, url, species, cadence)
         values ($1, $2, $3, $4, $5, $6)
         on conflict (channel_id, species, cadence) do nothing",
        Uuid::new_v4(),
        guild,
        channel,
        url,
        species,
        cadence.as_str(),
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// The webhook a channel's subscriptions already use, if any.
pub async fn channel_webhook(pool: &PgPool, channel: i64) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar!("select url from webhooks where channel_id = $1 limit 1", channel)
        .fetch_optional(pool)
        .await
}

/// Remove one subscription and return its webhook URL, or `None` if the
/// channel did not have it.
pub async fn delete_subscription(
    pool: &PgPool,
    channel: i64,
    species: &str,
    cadence: Cadence,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar!(
        "delete from webhooks where channel_id = $1 and species = $2 and cadence = $3
         returning url",
        channel,
        species,
        cadence.as_str(),
    )
    .fetch_optional(pool)
    .await
}

/// Whether any subscription still posts through a webhook.
pub async fn webhook_in_use(pool: &PgPool, url: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"select exists (select 1 from webhooks where url = $1) as "exists!""#,
        url
    )
    .fetch_one(pool)
    .await
}

/// Drop every subscription that posts through a webhook.
pub async fn delete_webhook_url(pool: &PgPool, url: &str) -> sqlx::Result<()> {
    sqlx::query!("delete from webhooks where url = $1", url).execute(pool).await?;
    Ok(())
}

/// A server's subscriptions as `(channel, species, cadence)`.
pub async fn guild_subscriptions(
    pool: &PgPool,
    guild: i64,
) -> sqlx::Result<Vec<(i64, String, String)>> {
    sqlx::query!(
        "select channel_id, species, cadence from webhooks where guild_id = $1
         order by channel_id, species, cadence",
        guild,
    )
    .map(|row| (row.channel_id, row.species, row.cadence))
    .fetch_all(pool)
    .await
}

/// Webhooks that have not yet fired for the current slot of their cadence.
pub async fn due_webhooks(
    pool: &PgPool,
    hour_slot: DateTime<Utc>,
    day_slot: DateTime<Utc>,
) -> sqlx::Result<Vec<Webhook>> {
    sqlx::query_as!(
        Webhook,
        "select id, url, species, cadence
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
