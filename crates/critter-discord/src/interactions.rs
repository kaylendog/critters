//! The `/critters` slash command. Discord delivers it as a signed HTTP request
//! to `POST /interactions`, so no gateway connection is needed.

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use chrono::Utc;
use critter_types::Kind;
use serenity::{
    all::{
        ChannelId, Command, CommandInteraction, CommandOptionType, GuildId, Interaction,
        InteractionContext, Permissions, ResolvedOption, ResolvedValue,
    },
    builder::{
        CreateCommand, CreateCommandOption, CreateInteractionResponse,
        CreateInteractionResponseMessage, CreateWebhook,
    },
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

/// Replace the application's global commands with `/critters`.
pub async fn register(app: &App) -> serenity::Result<()> {
    Command::set_global_commands(&*app.discord, vec![command()]).await?;
    Ok(())
}

/// `/critters subscribe|unsubscribe|list`. Only members who can manage
/// webhooks see it, and only in servers.
fn command() -> CreateCommand {
    let species = species_option();
    let cadence = CreateCommandOption::new(CommandOptionType::String, "cadence", "How often")
        .required(true)
        .add_string_choice("hourly", "hourly")
        .add_string_choice("daily", "daily");
    let subcommand = |name: &str, description: &str| {
        CreateCommandOption::new(CommandOptionType::SubCommand, name, description)
    };
    CreateCommand::new("critters")
        .description("Critter photos in this channel")
        .default_member_permissions(Permissions::MANAGE_WEBHOOKS)
        .contexts(vec![InteractionContext::Guild])
        .add_option(
            subcommand("subscribe", "Post a critter here on a schedule")
                .add_sub_option(species.clone())
                .add_sub_option(cadence.clone()),
        )
        .add_option(
            subcommand("unsubscribe", "Stop posting a critter here")
                .add_sub_option(species)
                .add_sub_option(cadence),
        )
        .add_option(subcommand("list", "List this server's subscriptions"))
        .add_option(
            subcommand("now", "Show this hour's photo, only to you")
                .add_sub_option(species_option()),
        )
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
) -> Result<Json<CreateInteractionResponse>, StatusCode> {
    let header = |name: &str| {
        headers.get(name).and_then(|value| value.to_str().ok()).ok_or(StatusCode::UNAUTHORIZED)
    };
    app.verifier
        .verify(header("X-Signature-Ed25519")?, header("X-Signature-Timestamp")?, &body)
        .map_err(|()| StatusCode::UNAUTHORIZED)?;

    let interaction: Interaction =
        serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let response = match interaction {
        Interaction::Ping(_) => CreateInteractionResponse::Pong,
        Interaction::Command(command) => {
            let message = run(&app, &command).await.unwrap_or_else(|error| {
                tracing::error!(%error, "command failed");
                text("Something went wrong. Try again later.")
            });
            CreateInteractionResponse::Message(message.ephemeral(true))
        }
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    Ok(Json(response))
}

/// Run a command and return the reply.
async fn run(
    app: &App,
    command: &CommandInteraction,
) -> anyhow::Result<CreateInteractionResponseMessage> {
    let Some(guild) = command.guild_id else {
        return Ok(text("Use this in a server channel."));
    };
    let channel = command.channel_id;
    let options = command.data.options();
    let Some(ResolvedOption { name, value: ResolvedValue::SubCommand(args), .. }) = options.first()
    else {
        return Ok(text("Unknown command."));
    };
    let species = option(args, "species").filter(|slug| critter_types::SPECIES.contains(slug));
    let cadence = option(args, "cadence").and_then(Cadence::parse);
    let reply = match (*name, species, cadence) {
        ("subscribe", Some(species), Some(cadence)) => {
            subscribe(app, guild, channel, species, cadence).await?
        }
        ("unsubscribe", Some(species), Some(cadence)) => {
            unsubscribe(app, channel, species, cadence).await?
        }
        ("list", _, _) => list(app, guild).await?,
        ("now", Some(species), _) => return now(app, species).await,
        _ => "Pick a species and a cadence.".into(),
    };
    Ok(text(reply))
}

/// A string option's value, if present.
fn option<'a>(args: &[ResolvedOption<'a>], name: &str) -> Option<&'a str> {
    args.iter().find(|option| option.name == name).and_then(|option| match option.value {
        ResolvedValue::String(value) => Some(value),
        _ => None,
    })
}

fn text(content: impl Into<String>) -> CreateInteractionResponseMessage {
    CreateInteractionResponseMessage::new().content(content)
}

/// This hour's photo, rendered the same way the scheduler posts it.
async fn now(app: &App, species: &str) -> anyhow::Result<CreateInteractionResponseMessage> {
    let slot = Kind::Hour.slot_containing(Utc::now());
    Ok(match push::fetch_photo(app, species, Kind::Hour, slot).await {
        Ok(photo) => CreateInteractionResponseMessage::new().embed(push::embed(&photo)),
        Err(error) => {
            tracing::warn!(%error, "could not fetch photo");
            text("No photo is available right now. Try again later.")
        }
    })
}

/// Subscribe the channel. Every subscription in a channel shares one webhook,
/// since Discord caps webhooks per channel.
async fn subscribe(
    app: &App,
    guild: GuildId,
    channel: ChannelId,
    species: &str,
    cadence: Cadence,
) -> anyhow::Result<String> {
    let url = match db::channel_webhook(&app.pool, channel.get() as i64).await? {
        Some(url) => url,
        None => {
            match channel.create_webhook(&*app.discord, CreateWebhook::new("Daily Critters")).await
            {
                Ok(webhook) => webhook.url()?,
                Err(error) if status(&error) == Some(403) => {
                    return Ok("I need the Manage Webhooks permission in this channel.".into());
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    let inserted = db::insert_webhook(
        &app.pool,
        guild.get() as i64,
        channel.get() as i64,
        &url,
        species,
        cadence,
    )
    .await?;
    Ok(if inserted {
        format!(
            "Subscribed to {} {species}. The first photo arrives within a minute.",
            cadence.as_str()
        )
    } else {
        format!("This channel already gets {} {species}.", cadence.as_str())
    })
}

/// Unsubscribe the channel, and delete its webhook once nothing uses it.
async fn unsubscribe(
    app: &App,
    channel: ChannelId,
    species: &str,
    cadence: Cadence,
) -> anyhow::Result<String> {
    let Some(url) =
        db::delete_subscription(&app.pool, channel.get() as i64, species, cadence).await?
    else {
        return Ok(format!("This channel does not get {} {species}.", cadence.as_str()));
    };
    // Someone may have deleted the webhook by hand already. That is fine.
    let parsed = reqwest::Url::parse(&url).ok();
    if !db::webhook_in_use(&app.pool, &url).await?
        && let Some((id, token)) = parsed.as_ref().and_then(serenity::utils::parse_webhook)
        && let Err(error) = app.discord.delete_webhook_with_token(id, token, None).await
    {
        tracing::warn!(%error, "could not delete webhook");
    }
    Ok(format!("Unsubscribed from {} {species}.", cadence.as_str()))
}

/// Every subscription in the server, by channel.
async fn list(app: &App, guild: GuildId) -> anyhow::Result<String> {
    let subscriptions = db::guild_subscriptions(&app.pool, guild.get() as i64).await?;
    if subscriptions.is_empty() {
        return Ok("No subscriptions yet. Try `/critters subscribe`.".into());
    }
    Ok(subscriptions
        .iter()
        .map(|(channel, species, cadence)| format!("<#{channel}> {cadence} {species}"))
        .collect::<Vec<_>>()
        .join("\n"))
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
    fn command_fits_discord_limits() {
        let json = serde_json::to_value(command()).unwrap();
        assert_eq!(json["default_member_permissions"], "536870912");
        // Discord allows at most 25 choices per option.
        let species = &json["options"][0]["options"][0]["choices"];
        assert_eq!(species.as_array().unwrap().len(), critter_types::SPECIES.len());
        assert!(critter_types::SPECIES.len() <= 25);
    }
}
