//! Slash commands. Discord delivers them as signed HTTP requests to
//! `POST /interactions`, so no gateway connection is needed.

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use chrono::{NaiveTime, Utc};
use chrono_tz::Tz;
use critter_types::Kind;
use serde_json::{Value, json};
use serenity::{
    all::{
        ChannelId, Command, CommandInteraction, CommandOptionType, GuildId, Interaction,
        InteractionContext, Permissions, ResolvedOption, ResolvedValue,
    },
    builder::{CreateCommand, CreateCommandOption, CreateWebhook},
    http::HttpError,
};

use crate::{
    App,
    db::{self, Cadence},
    push,
};

pub fn router() -> Router<App> {
    Router::new().route("/interactions", post(interaction))
}

/// Replace the application's global commands with ours.
pub async fn register(app: &App) -> serenity::Result<()> {
    Command::set_global_commands(&*app.discord, commands()).await?;
    Ok(())
}

/// `/now` for everyone, and `/subscribe`, `/configure`, `/unsubscribe` and
/// `/list` for members who can manage webhooks. Discord enforces the
/// permission per command. All work in servers only. Each channel has at
/// most one subscription per species.
fn commands() -> Vec<CreateCommand> {
    let cadence = CreateCommandOption::new(CommandOptionType::String, "cadence", "How often")
        .add_string_choice("hourly", "hourly")
        .add_string_choice("daily", "daily");
    let time =
        CreateCommandOption::new(CommandOptionType::String, "time", "Daily post time as HH:MM");
    let timezone =
        CreateCommandOption::new(CommandOptionType::String, "timezone", "Time zone for the time")
            .set_autocomplete(true);
    let command = |name: &str, description: &str| {
        CreateCommand::new(name).description(description).contexts(vec![InteractionContext::Guild])
    };
    let manage = |name: &str, description: &str| {
        command(name, description).default_member_permissions(Permissions::MANAGE_WEBHOOKS)
    };
    vec![
        command("now", "Show this hour's critter photo, only to you").add_option(species_option()),
        manage("subscribe", "Post a critter here on a schedule")
            .add_option(species_option())
            .add_option(cadence.clone().required(true))
            .add_option(time.clone().description("Daily post time as HH:MM. Default 00:00"))
            .add_option(timezone.clone().description("Time zone for the time. Default UTC")),
        manage("configure", "Change a critter subscription here. Unset options are kept")
            .add_option(species_option())
            .add_option(cadence)
            .add_option(time)
            .add_option(timezone),
        manage("unsubscribe", "Stop posting a critter here").add_option(species_option()),
        manage("list", "List this server's critter subscriptions"),
        manage("trigger", "Post a subscription's current photo here now, for testing")
            .add_option(species_option()),
    ]
}

fn species_option() -> CreateCommandOption {
    critter_types::SPECIES.iter().fold(
        CreateCommandOption::new(CommandOptionType::String, "species", "Which critter")
            .required(true),
        |option, slug| option.add_string_choice(*slug, *slug),
    )
}

/// `POST /interactions` - every slash command and Discord's endpoint PING.
/// 401 unless signed with the application's key.
async fn interaction(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, StatusCode> {
    let header = |name: &str| {
        headers.get(name).and_then(|value| value.to_str().ok()).ok_or(StatusCode::UNAUTHORIZED)
    };
    app.verifier
        .verify(header("X-Signature-Ed25519")?, header("X-Signature-Timestamp")?, &body)
        .map_err(|()| StatusCode::UNAUTHORIZED)?;

    let interaction: Interaction =
        serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    // Responses are plain JSON, since serenity cannot build Components V2.
    // Type 1 is PONG and type 4 is a message.
    let response = match interaction {
        Interaction::Ping(_) => json!({ "type": 1 }),
        Interaction::Command(command) => {
            let message = run(&app, &command).await.unwrap_or_else(|failure| {
                tracing::error!(error = %failure, "command failed");
                error("Something went wrong. Try again later.")
            });
            json!({ "type": 4, "data": message })
        }
        // Type 8 offers autocomplete choices. Only `timezone` autocompletes.
        Interaction::Autocomplete(command) => {
            let query = command.data.autocomplete().map(|option| option.value).unwrap_or_default();
            json!({ "type": 8, "data": { "choices": timezones(query) } })
        }
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    Ok(Json(response))
}

/// Only the person who ran the command sees the reply.
const EPHEMERAL: u64 = 1 << 6;

/// Run a command and return the reply.
async fn run(app: &App, command: &CommandInteraction) -> anyhow::Result<Value> {
    let Some(guild) = command.guild_id else {
        return Ok(error("Use this in a server channel."));
    };
    let channel = command.channel_id;
    let args = &command.data.options();
    let species = option(args, "species").filter(|slug| critter_types::SPECIES.contains(slug));
    let cadence = option(args, "cadence").and_then(Cadence::parse);
    let Ok(post_at) =
        option(args, "time").map(|time| NaiveTime::parse_from_str(time, "%H:%M")).transpose()
    else {
        return Ok(error("Give the time as HH:MM, for example 09:30."));
    };
    let Ok(timezone) = option(args, "timezone").map(str::parse::<Tz>).transpose() else {
        return Ok(error("Unknown time zone. Pick one from the list."));
    };

    match (command.data.name.as_str(), species) {
        ("list", _) => list(app, guild).await,
        (_, None) => Ok(error("Pick a species.")),
        ("now", Some(species)) => now(app, species).await,
        ("subscribe", Some(species)) => {
            let Some(cadence) = cadence else { return Ok(error("Pick a cadence.")) };
            let subscription = db::NewSubscription {
                guild: guild.get() as i64,
                channel: channel.get() as i64,
                url: "",
                species,
                cadence,
                post_at: post_at.unwrap_or(NaiveTime::MIN),
                timezone: timezone.unwrap_or(Tz::UTC).name(),
            };
            subscribe(app, channel, subscription).await
        }
        ("configure", Some(species)) => {
            if cadence.is_none() && post_at.is_none() && timezone.is_none() {
                return Ok(error("Nothing to change. Give a cadence, time or time zone."));
            }
            let channel = channel.get() as i64;
            let timezone = timezone.map(|timezone| timezone.name());
            Ok(
                match db::configure(&app.pool, channel, species, cadence, post_at, timezone).await?
                {
                    Some(row) => success(format!(
                        "Updated {species}. It now posts {}.",
                        describe(&row.cadence, row.post_at, &row.timezone)
                    )),
                    None => error(format!("This channel does not get {species}.")),
                },
            )
        }
        ("unsubscribe", Some(species)) => unsubscribe(app, channel, species).await,
        ("trigger", Some(species)) => trigger(app, channel, species).await,
        _ => Ok(error("Unknown command.")),
    }
}

/// A string option's value, if present.
fn option<'a>(args: &[ResolvedOption<'a>], name: &str) -> Option<&'a str> {
    args.iter().find(|option| option.name == name).and_then(|option| match option.value {
        ResolvedValue::String(value) => Some(value),
        _ => None,
    })
}

/// A reply holding one container with one text display.
fn text(content: impl Into<String>) -> Value {
    json!({
        "flags": push::IS_COMPONENTS_V2 | EPHEMERAL,
        "components": [{
            "type": 17, // Container
            "components": [{ "type": 10, "content": content.into() }], // Text display
        }],
    })
}

fn success(message: impl Into<String>) -> Value {
    text(format!(":white_check_mark: {}", message.into()))
}

fn error(message: impl Into<String>) -> Value {
    text(format!(":x: {}", message.into()))
}

/// This hour's photo, rendered the same way the scheduler posts it.
async fn now(app: &App, species: &str) -> anyhow::Result<Value> {
    let slot = Kind::Hour.slot_containing(Utc::now());
    Ok(match push::fetch_photo(app, species, Kind::Hour, slot).await {
        Ok(photo) => push::message(&photo, EPHEMERAL),
        Err(failure) => {
            tracing::warn!(error = %failure, "could not fetch photo");
            error("No photo is available right now. Try again later.")
        }
    })
}

/// Subscribe the channel. Every subscription in a channel shares one webhook,
/// since Discord caps webhooks per channel.
/// Its `url` is ignored and filled in here.
async fn subscribe(
    app: &App,
    channel: ChannelId,
    subscription: db::NewSubscription<'_>,
) -> anyhow::Result<Value> {
    let url = match db::channel_webhook(&app.pool, channel.get() as i64).await? {
        Some(url) => url,
        None => {
            match channel.create_webhook(&*app.discord, CreateWebhook::new("Daily Critters")).await
            {
                Ok(webhook) => webhook.url()?,
                Err(failure) if status(&failure) == Some(403) => {
                    return Ok(error("I need the Manage Webhooks permission in this channel."));
                }
                Err(failure) => return Err(failure.into()),
            }
        }
    };
    let subscription = db::NewSubscription { url: &url, ..subscription };
    let species = subscription.species;
    Ok(if db::insert_webhook(&app.pool, &subscription).await? {
        let schedule =
            describe(subscription.cadence.as_str(), subscription.post_at, subscription.timezone);
        success(format!("Subscribed to {species}, {schedule}."))
    } else {
        error(format!("This channel already gets {species}. Use `/configure` to change it."))
    })
}

/// `hourly`, or `daily at 09:00 Europe/Stockholm`.
fn describe(cadence: &str, post_at: NaiveTime, timezone: &str) -> String {
    match cadence {
        "daily" => format!("daily at {} {timezone}", post_at.format("%H:%M")),
        other => other.into(),
    }
}

/// Up to 25 IANA zone names containing `query`, Discord's limit. Spaces match
/// underscores, so `new york` finds `America/New_York`.
fn timezones(query: &str) -> Vec<Value> {
    let query = query.to_lowercase().replace(' ', "_");
    chrono_tz::TZ_VARIANTS
        .iter()
        .map(|timezone| timezone.name())
        .filter(|name| name.to_lowercase().contains(&query))
        .take(25)
        .map(|name| json!({ "name": name, "value": name }))
        .collect()
}

/// Unsubscribe the channel, and delete its webhook once nothing uses it.
async fn unsubscribe(app: &App, channel: ChannelId, species: &str) -> anyhow::Result<Value> {
    let Some(url) = db::delete_subscription(&app.pool, channel.get() as i64, species).await? else {
        return Ok(error(format!("This channel does not get {species}.")));
    };
    // Someone may have deleted the webhook by hand already. That is fine.
    let parsed = reqwest::Url::parse(&url).ok();
    if !db::webhook_in_use(&app.pool, &url).await?
        && let Some((id, token)) = parsed.as_ref().and_then(serenity::utils::parse_webhook)
        && let Err(failure) = app.discord.delete_webhook_with_token(id, token, None).await
    {
        tracing::warn!(error = %failure, "could not delete webhook");
    }
    Ok(success(format!("Unsubscribed from {species}.")))
}

/// Post the current slot of a subscription's cadence through its webhook now.
/// The post is not recorded, so the scheduled one still goes out.
async fn trigger(app: &App, channel: ChannelId, species: &str) -> anyhow::Result<Value> {
    let Some(webhook) = db::subscription(&app.pool, channel.get() as i64, species).await? else {
        return Ok(error(format!("This channel does not get {species}.")));
    };
    let Some(cadence) = Cadence::parse(&webhook.cadence) else {
        anyhow::bail!("subscription {} has cadence {:?}", webhook.id, webhook.cadence);
    };
    let kind = cadence.kind();
    let photo = match push::fetch_photo(app, species, kind, kind.slot_containing(Utc::now())).await
    {
        Ok(photo) => photo,
        Err(failure) => {
            tracing::warn!(error = %failure, "could not fetch photo");
            return Ok(error("No photo is available right now. Try again later."));
        }
    };
    Ok(match push::post(app, &webhook.url, &photo).await {
        Ok(()) => success(format!("Posted the current {species} photo.")),
        Err(failure) => {
            tracing::warn!(error = %failure, "triggered post failed");
            error(format!("Discord rejected the post: {failure}"))
        }
    })
}

/// Every subscription in the server, by channel.
async fn list(app: &App, guild: GuildId) -> anyhow::Result<Value> {
    let subscriptions = db::guild_subscriptions(&app.pool, guild.get() as i64).await?;
    if subscriptions.is_empty() {
        return Ok(text("No subscriptions yet. Try `/subscribe`."));
    }
    let lines: Vec<_> = subscriptions
        .iter()
        .map(|row| {
            let schedule = describe(&row.cadence, row.post_at, &row.timezone);
            format!("<#{}> {}, {schedule}", row.channel_id, row.species)
        })
        .collect();
    Ok(text(lines.join("\n")))
}

/// The HTTP status of a failed Discord request, if it got that far.
pub fn status(error: &serenity::Error) -> Option<u16> {
    match error {
        serenity::Error::Http(HttpError::UnsuccessfulRequest(response)) => {
            Some(response.status_code.as_u16())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_fit_discord_limits() {
        let json = serde_json::to_value(commands()).unwrap();
        // Discord allows at most 25 choices per option.
        let species = &json[0]["options"][0]["choices"];
        assert_eq!(species.as_array().unwrap().len(), critter_types::SPECIES.len());
        assert!(critter_types::SPECIES.len() <= 25);
    }

    #[test]
    fn only_now_is_public() {
        let json = serde_json::to_value(commands()).unwrap();
        for command in json.as_array().unwrap() {
            let permissions = &command["default_member_permissions"];
            match command["name"].as_str().unwrap() {
                "now" => assert!(permissions.is_null()),
                _ => assert_eq!(permissions, "536870912"),
            }
        }
    }

    #[test]
    fn success_is_a_container() {
        let reply = success("Unsubscribed from fox.");
        assert_eq!(
            reply["components"][0]["components"][0]["content"],
            ":white_check_mark: Unsubscribed from fox."
        );
    }

    #[test]
    fn error_is_a_container() {
        let reply = error("Pick a species.");
        assert_eq!(reply["flags"], 32768 | 64);
        assert_eq!(reply["components"][0]["type"], 17);
        assert_eq!(reply["components"][0]["components"][0]["content"], ":x: Pick a species.");
    }

    #[test]
    fn timezones_autocomplete() {
        let names =
            |query| timezones(query).iter().map(|c| c["value"].to_string()).collect::<Vec<_>>();
        assert!(names("new york").contains(&r#""America/New_York""#.to_string()));
        assert!(names("STOCKHOLM").contains(&r#""Europe/Stockholm""#.to_string()));
        assert_eq!(timezones("").len(), 25);
    }
}
