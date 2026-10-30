//! Postgres queries for the pusher's own database. Queries are checked at
//! compile time against `DATABASE_URL`, or against `.sqlx/` when
//! `SQLX_OFFLINE=true`. Run `cargo sqlx prepare` from this crate's directory
//! after changing one.

use chrono::{DateTime, NaiveTime, Utc};
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
    /// Local time a daily subscription posts at, in `timezone`.
    pub post_at: NaiveTime,
    /// IANA time zone name, e.g. `Europe/Stockholm`.
    pub timezone: String,
    pub last_fired_slot: Option<DateTime<Utc>>,
}

/// Where and how a channel subscribes.
pub struct NewSubscription<'a> {
    pub guild: i64,
    pub channel: i64,
    pub url: &'a str,
    pub species: &'a str,
    pub cadence: Cadence,
    pub post_at: NaiveTime,
    pub timezone: &'a str,
}

/// Subscribe a channel. Returns `false` if it already gets this species.
pub async fn insert_webhook(pool: &PgPool, new: &NewSubscription<'_>) -> sqlx::Result<bool> {
    let result = sqlx::query!(
        "insert into webhooks (id, guild_id, channel_id, url, species, cadence, post_at, timezone)
         values ($1, $2, $3, $4, $5, $6, $7, $8)
         on conflict (channel_id, species) do nothing",
        Uuid::new_v4(),
        new.guild,
        new.channel,
        new.url,
        new.species,
        new.cadence.as_str(),
        new.post_at,
        new.timezone,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Change the settings that are `Some` and keep the rest. Returns the
/// subscription as it now stands, or `None` if the channel does not get this
/// species.
pub async fn configure(
    pool: &PgPool,
    channel: i64,
    species: &str,
    cadence: Option<Cadence>,
    post_at: Option<NaiveTime>,
    timezone: Option<&str>,
) -> sqlx::Result<Option<Subscription>> {
    sqlx::query_as!(
        Subscription,
        "update webhooks
         set cadence = coalesce($3, cadence),
             post_at = coalesce($4, post_at),
             timezone = coalesce($5, timezone)
         where channel_id = $1 and species = $2
         returning channel_id, species, cadence, post_at, timezone",
        channel,
        species,
        cadence.map(Cadence::as_str),
        post_at,
        timezone,
    )
    .fetch_optional(pool)
    .await
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
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar!(
        "delete from webhooks where channel_id = $1 and species = $2 returning url",
        channel,
        species,
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

/// A channel's subscription to a species, if it has one.
pub async fn subscription(
    pool: &PgPool,
    channel: i64,
    species: &str,
) -> sqlx::Result<Option<Webhook>> {
    sqlx::query_as!(
        Webhook,
        "select id, url, species, cadence, post_at, timezone, last_fired_slot
         from webhooks where channel_id = $1 and species = $2",
        channel,
        species,
    )
    .fetch_optional(pool)
    .await
}

/// Drop every subscription that posts through a webhook.
pub async fn delete_webhook_url(pool: &PgPool, url: &str) -> sqlx::Result<()> {
    sqlx::query!("delete from webhooks where url = $1", url).execute(pool).await?;
    Ok(())
}

/// One line of `/critters list`.
pub struct Subscription {
    pub channel_id: i64,
    pub species: String,
    pub cadence: String,
    pub post_at: NaiveTime,
    pub timezone: String,
}

/// A server's subscriptions, by channel.
pub async fn guild_subscriptions(pool: &PgPool, guild: i64) -> sqlx::Result<Vec<Subscription>> {
    sqlx::query_as!(
        Subscription,
        "select channel_id, species, cadence, post_at, timezone from webhooks
         where guild_id = $1 order by channel_id, species, cadence",
        guild,
    )
    .fetch_all(pool)
    .await
}

/// Hourly webhooks that have not fired for `hour_slot`, and every daily
/// webhook. Whether a daily one is due depends on its time zone, which the
/// scheduler works out.
pub async fn due_webhooks(pool: &PgPool, hour_slot: DateTime<Utc>) -> sqlx::Result<Vec<Webhook>> {
    sqlx::query_as!(
        Webhook,
        "select id, url, species, cadence, post_at, timezone, last_fired_slot
         from webhooks
         where cadence = 'daily' or last_fired_slot is null or last_fired_slot < $1
         order by created_at",
        hour_slot,
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
