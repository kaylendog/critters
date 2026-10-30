# critter-discord

A Discord bot that posts critter photos on a schedule. It receives slash
commands as HTTP requests, so it needs no gateway connection. Photos come
from a `critter-server` over HTTP.

## Commands

All replies are visible only to the person who ran the command. Anyone can
use `/now`. The rest need Manage Webhooks, which Discord enforces. Server
admins can change this under Server Settings, Integrations.

| Command | What it does |
| --- | --- |
| `/now species` | This hour's photo. |
| `/subscribe species cadence [time] [timezone]` | Post that species here hourly or daily. Daily posts go out at `time` (HH:MM, default 00:00) in `timezone` (default UTC). |
| `/configure species [cadence] [time] [timezone]` | Change a subscription. Options you leave out are kept. |
| `/unsubscribe species` | Stop. |
| `/list` | Every subscription in this server. |

A channel can have one subscription per species. They share one webhook, which is
removed with the last subscription. If the webhook is deleted or the bot is
removed, its subscriptions are dropped on the next post.

## Setup

1. Create an application in the
   [Developer Portal](https://discord.com/developers/applications). Copy its
   Application ID, Public Key and a bot token into `.env`.
2. Start the bot. It registers its commands on every start.
3. Expose port 8081 over public HTTPS. For a quick test, use
   `ngrok http 8081`.
4. Set the Interactions Endpoint URL to `https://<host>/interactions`.
   Discord checks it when you save.
5. Invite the bot. It only needs Manage Webhooks:
   `https://discord.com/oauth2/authorize?client_id=<APP_ID>&scope=bot+applications.commands&permissions=536870912`

## Configuration

Every flag is also an environment variable. `.env` in this directory is loaded
for local development.

| Variable | Default | Meaning |
| --- | --- | --- |
| `DATABASE_URL` | Required | The bot's own Postgres database. |
| `DISCORD_APPLICATION_ID` | Required | From the Developer Portal. |
| `DISCORD_PUBLIC_KEY` | Required | Hex. Verifies that requests come from Discord. |
| `DISCORD_TOKEN` | Required | Bot token. |
| `SERVER_URL` | Required | The `critter-server` to pull photos from. |
| `BIND_ADDR` | `0.0.0.0:8081` | Listen address. |
| `POLL_INTERVAL_SECS` | `60` | How often to look for subscriptions that are due. |
| `USER_AGENT` | Project URL | Sent to the server. |
