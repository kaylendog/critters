# critters

One critter photo per hour and per day, picked by a ranker trained on your own
taste. Photos come only from sources with licences that allow non-commercial
redistribution. Bots post each photo to chat services.

## How it works

- `critter-server` pulls candidate photos from Wikimedia Commons, iNaturalist
  and Openverse. It embeds each one with CLIP.
- You compare photos two at a time on a local web page. A Bradley-Terry ranker
  learns your preferences from those verdicts.
- Only the top half of each species, by score, can be picked. You can also
  hide any photo for good.
- Each slot is picked deterministically on first request and then pinned, so
  every caller sees the same photo.
- `critter-discord` is a Discord bot. Servers subscribe a channel with a slash
  command, and it posts each new slot there.

## Crates

| Crate | What it is |
| --- | --- |
| [`critter-server`](crates/critter-server) | Photo API, ingest, labelling UI and ranker. |
| [`critter-discord`](crates/critter-discord) | Discord bot. Pulls from the server over HTTP. |
| [`critter-types`](crates/critter-types) | Types shared by the two. |

Each binary has its own Postgres database.

## Species

`wolf`, `coyote`, `fox`, `bernese`, `hyena`, `wild-dog`, `deer`,
`bearded-dragon`, `leopard-gecko`, `crested-gecko`, `snow-leopard`.

## Running it

Compose files live in `docker/`. `common.yml` holds what both setups share.

| File | Use |
| --- | --- |
| `compose.prod.yml` | Published images, restart on failure, ports bound to localhost only. |
| `compose.dev.yml` | Postgres on 5432, plus the binaries built from source behind the `app` profile. |

In production:

```sh
cp docker/.env.example docker/.env   # then fill it in
docker compose -f docker/compose.prod.yml up -d
docker compose -f docker/compose.prod.yml run --rm server seed
```

The server prints a random admin token on start unless `SERVER_ADMIN_TOKEN` is
set. Open <http://localhost:8080/label> on the host, or through an SSH tunnel,
paste the token, and start comparing. Press Train after a hundred or so pairs.
Discord must reach the bot on 8081 over public HTTPS, so put a reverse proxy in
front of it. The bot's own setup is in its
[README](crates/critter-discord/README.md).

For development, run only Postgres in Docker and the binaries with Cargo:

```sh
docker compose -f docker/compose.dev.yml up -d
cp crates/critter-server/.env.example crates/critter-server/.env
cargo run -p critter-server -- seed
cargo run -p critter-server
```

## Moving your model

Labels and weights live in the server's database. To take them to a new
deployment:

```sh
critter-server export critters.json   # on the old one
critter-server import critters.json   # on the new one, before any labelling
```

The import brings in every labelled image, so it works on an unseeded
database.

## Licence

MIT. Photos keep their own licences, which the API returns with each photo.
