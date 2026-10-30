//! The labelling page and its small JSON API. You are shown two photos of a
//! species and click the better one. Pairs feed the ranker in `rank`.

use std::collections::BTreeMap;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::{App, db, ml::rank, require_admin, sources};

/// Query string for `GET /label/pair` and `GET /label/ranking.json`.
#[derive(Deserialize)]
struct PairQuery {
    species: String,
    /// Only offer images that appear in no pair yet.
    #[serde(default)]
    validation: bool,
}

/// Two candidates to compare, plus how many pairs are labelled so far.
#[derive(Serialize)]
struct Pair {
    a: db::LabelImage,
    b: db::LabelImage,
    pairs: i64,
    validation: ValidationStats,
}

/// How the ranker is doing on pairs it has not trained on.
#[derive(Serialize)]
struct ValidationStats {
    total: i64,
    agreed: i64,
}

/// Body of `POST /label`.
#[derive(Deserialize)]
struct Verdict {
    winner: i64,
    loser: i64,
    /// Record as a validation pair, kept out of training, and report what the
    /// model would have picked.
    #[serde(default)]
    validation: bool,
}

/// Response to a validation verdict.
#[derive(Serialize)]
struct Checked {
    /// The image the model preferred, or `None` when either is unscored.
    model_winner: Option<i64>,
    /// `P(model's pick beats the other)` under the Bradley-Terry model.
    confidence: Option<f32>,
    agreed: Option<bool>,
    validation: ValidationStats,
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
        .route("/label/promote", post(promote))
        .route("/label/ranking.json", get(ranking))
        .route_layer(middleware::from_fn_with_state(app, require_admin));
    Router::new().route("/label", get(page)).route("/label/ranking", get(ranking_page)).merge(api)
}

/// `GET /label` - the labelling page.
async fn page() -> Html<&'static str> {
    Html(include_str!("../static/label.html"))
}

/// `GET /label/ranking` - the ranking page.
async fn ranking_page() -> Html<&'static str> {
    Html(include_str!("../static/ranking.html"))
}

/// `GET /label/ranking.json?species=fox` - every image of a species, best
/// first, with its score and whether it currently qualifies for slots.
async fn ranking(
    State(app): State<App>,
    Query(query): Query<PairQuery>,
) -> Result<Json<Vec<db::RankedImage>>, StatusCode> {
    let species = sources::species(&query.species).ok_or(StatusCode::NOT_FOUND)?;
    let ranked =
        db::ranking(&app.pool, species.slug, app.config.keep_fraction).await.map_err(internal)?;
    Ok(Json(ranked))
}

/// `GET /label/pair?species=fox[&validation=true]` - two random embedded
/// photos to compare. With `validation`, only photos that appear in no pair.
/// 404 for an unknown species, 503 when fewer than two qualify.
async fn pair(
    State(app): State<App>,
    Query(query): Query<PairQuery>,
) -> Result<Json<Pair>, StatusCode> {
    let species = sources::species(&query.species).ok_or(StatusCode::NOT_FOUND)?;
    let (a, b) = db::random_pair(&app.pool, species.slug, query.validation)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let pairs = db::count_pairs(&app.pool).await.map_err(internal)?;
    let validation = validation_stats(&app).await?;
    Ok(Json(Pair { a, b, pairs, validation }))
}

/// `POST /label` - record that `winner` beat `loser`. 204 for a training
/// pair. A validation pair instead returns what the model thought. 422 if
/// they are the same image or either does not exist.
async fn record(
    State(app): State<App>,
    Json(verdict): Json<Verdict>,
) -> Result<Response, StatusCode> {
    if verdict.winner == verdict.loser {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    match db::insert_pair(&app.pool, verdict.winner, verdict.loser, verdict.validation).await {
        Ok(()) => {}
        // A foreign key failure means an id does not exist.
        Err(sqlx::Error::Database(error)) if error.is_foreign_key_violation() => {
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
        Err(error) => return Err(internal(error)),
    }
    if !verdict.validation {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // Compare the model's view with the verdict. Confidence is the logistic
    // of the score gap, the same quantity the ranker was trained on.
    let (winner, loser) =
        db::pair_scores(&app.pool, verdict.winner, verdict.loser).await.map_err(internal)?;
    let judged = winner.zip(loser).map(|(winner, loser)| {
        let agreed = winner > loser;
        let gap = (winner - loser).abs();
        let model_winner = if agreed { verdict.winner } else { verdict.loser };
        (model_winner, 1.0 / (1.0 + (-gap).exp()), agreed)
    });
    Ok(Json(Checked {
        model_winner: judged.map(|(id, _, _)| id),
        confidence: judged.map(|(_, confidence, _)| confidence),
        agreed: judged.map(|(_, _, agreed)| agreed),
        validation: validation_stats(&app).await?,
    })
    .into_response())
}

/// `POST /label/promote` - turn every validation pair into a training pair.
async fn promote(State(app): State<App>) -> Result<Json<serde_json::Value>, StatusCode> {
    let promoted = db::promote_validation(&app.pool).await.map_err(internal)?;
    Ok(Json(serde_json::json!({ "promoted": promoted })))
}

async fn validation_stats(app: &App) -> Result<ValidationStats, StatusCode> {
    let (total, agreed) = db::validation_stats(&app.pool).await.map_err(internal)?;
    Ok(ValidationStats { total, agreed })
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
