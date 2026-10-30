//! The labelling page and its small JSON API. You are shown two photos of a
//! species and click the better one. Pairs feed the ranker in `rank`.

use std::collections::BTreeMap;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    middleware,
    response::Html,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::{App, db, ml::rank, require_admin, sources};

/// Query string for `GET /label/pair`.
#[derive(Deserialize)]
struct PairQuery {
    species: String,
}

/// Two candidates to compare, plus how many pairs are labelled so far.
#[derive(Serialize)]
struct Pair {
    a: db::LabelImage,
    b: db::LabelImage,
    pairs: i64,
}

/// Body of `POST /label`.
#[derive(Deserialize)]
struct Verdict {
    winner: i64,
    loser: i64,
}

/// Body of `POST /label/hide`.
#[derive(Deserialize)]
struct Hide {
    id: i64,
    /// The image shown beside it, recorded as a win over the hidden one.
    opponent: Option<i64>,
}

/// Result of `POST /label/train`.
#[derive(Serialize)]
struct Trained {
    pairs: usize,
    /// Images per species that now qualify for slots.
    eligible: BTreeMap<&'static str, usize>,
}

/// The page itself is public. Everything under `/label/` and the `POST`
/// require the admin token, which the page asks for and keeps in local
/// storage.
pub fn router(app: App) -> Router<App> {
    let api = Router::new()
        .route("/label", post(record))
        .route("/label/pair", get(pair))
        .route("/label/hide", post(hide))
        .route("/label/train", post(train))
        .route_layer(middleware::from_fn_with_state(app, require_admin));
    Router::new().route("/label", get(page)).merge(api)
}

/// `GET /label` - the labelling page.
async fn page() -> Html<&'static str> {
    Html(include_str!("../static/label.html"))
}

/// `GET /label/pair?species=fox` - two random embedded photos to compare.
/// 404 for an unknown species, 503 while fewer than two photos are embedded.
async fn pair(
    State(app): State<App>,
    Query(query): Query<PairQuery>,
) -> Result<Json<Pair>, StatusCode> {
    let species = sources::species(&query.species).ok_or(StatusCode::NOT_FOUND)?;
    let (a, b) = db::random_pair(&app.pool, species.slug)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let pairs = db::count_pairs(&app.pool).await.map_err(internal)?;
    Ok(Json(Pair { a, b, pairs }))
}

/// `POST /label` - record that `winner` beat `loser`. 422 if they are the
/// same image or either does not exist.
async fn record(State(app): State<App>, Json(verdict): Json<Verdict>) -> StatusCode {
    if verdict.winner == verdict.loser {
        return StatusCode::UNPROCESSABLE_ENTITY;
    }
    match db::insert_pair(&app.pool, verdict.winner, verdict.loser).await {
        Ok(()) => StatusCode::NO_CONTENT,
        // A foreign key failure means an id does not exist.
        Err(sqlx::Error::Database(error)) if error.is_foreign_key_violation() => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        Err(error) => internal(error),
    }
}

/// `POST /label/hide` - never serve this image again, and record it as a
/// loss for the ranker. 404 if it does not exist.
async fn hide(State(app): State<App>, Json(hide): Json<Hide>) -> StatusCode {
    match db::hide_image(&app.pool, hide.id, hide.opponent).await {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(error) => internal(error),
    }
}

/// `POST /label/train` - retrain the ranker and rescore every image.
async fn train(State(app): State<App>) -> Result<Json<Trained>, StatusCode> {
    let pairs = rank::retrain(&app).await.map_err(|error| {
        tracing::error!(%error, "training failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let mut eligible = BTreeMap::new();
    for species in sources::SPECIES {
        let ids = db::eligible_ids(&app.pool, species.slug, app.config.keep_fraction)
            .await
            .map_err(internal)?;
        eligible.insert(species.slug, ids.len());
    }
    Ok(Json(Trained { pairs, eligible }))
}

fn internal(error: sqlx::Error) -> StatusCode {
    tracing::error!(%error, "label query failed");
    StatusCode::INTERNAL_SERVER_ERROR
}
