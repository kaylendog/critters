# critter-server

The photo API, the ingest job, the labelling UI and the preference ranker.

## Commands

| Command | What it does |
| --- | --- |
| `serve` | Run the API and ingest on a timer. The default. |
| `seed [--per-species N] [--species a,b]` | Ingest once and exit. |
| `train` | Retrain the ranker and rescore every image. |
| `export <file>` | Write the model, pairs and hides to JSON. |
| `import <file>` | Load an export into a database with no pairs. |

## API

| Route | Auth | Returns |
| --- | --- | --- |
| `GET /species` | None | Every species slug. |
| `GET /{species}/{YYYY}/{MM}/{DD}` | None | The day's photo as JSON. |
| `GET /{species}/{YYYY}/{MM}/{DD}/{HH}` | None | The hour's photo as JSON. |
| `.../image` on either | None | A redirect to the image itself. |
| `GET /label` | Page asks | The labelling page. |
| `GET /label/ranking` | Page asks | Every image of a species, best first. |

Times are UTC. Slots in the future or before `MIN_SLOT` are 404. A 503 means no
photo is eligible yet.

```json
{
  "species": "coyote",
  "slot": "2026-10-30T15:00:00Z",
  "kind": "hour",
  "image_url": "https://upload.wikimedia.org/...",
  "source_url": "https://commons.wikimedia.org/wiki/File:...",
  "source": "commons",
  "author": "Jane Doe",
  "author_url": "https://commons.wikimedia.org/wiki/User:...",
  "license": "CC-BY-SA-4.0",
  "license_url": "https://creativecommons.org/licenses/by-sa/4.0/",
  "width": 1280,
  "height": 853
}
```

The label pages themselves are public. They ask for the admin token and send it
as a bearer token on every call.

## Labelling

- Click the better photo, or press `A`/`←` and `B`/`→`. `S` skips.
- `1`, `2` and `3` hide the left, both, or the right photo for good.
- Training pairs are chosen where the ranker is least sure.
- Tick Validate to label held-out pairs. These measure accuracy and never
  reach training. Promote moves them into training.
- The history table shows training and validation accuracy for every run.

## Configuration

Every flag is also an environment variable. `.env` in this directory is loaded
for local development.

| Variable | Default | Meaning |
| --- | --- | --- |
| `DATABASE_URL` | Required | Postgres connection string. |
| `ADMIN_TOKEN` | Random, logged | Bearer token for `/label`. |
| `BIND_ADDR` | `0.0.0.0:8080` | Listen address. |
| `DATA_DIR` | `./data` | Where the CLIP weights are cached, about 600 MB. |
| `KEEP_FRACTION` | `0.5` | Share of each species, by score, that can be picked. |
| `INGEST_INTERVAL_HOURS` | `6` | How often to look for new photos. |
| `SEED_PER_SPECIES` | `300` | Photos fetched per species and source each ingest. |
| `MIN_SLOT` | `2026-01-01` | Earliest date served. |
| `LICENSES` | All non-commercial families | Comma list, e.g. `CC0,CC-BY`. |
| `USER_AGENT` | Project URL | Sent upstream. Wikimedia asks for contact details. |

## Metal

On Apple silicon, `cargo run -p critter-server --features metal` embeds on the
GPU, roughly 5 images a second. It falls back to the CPU when Metal is missing.

## Queries

Queries are checked at compile time. After changing one, run
`cargo sqlx prepare` in this directory and commit `.sqlx/`.
