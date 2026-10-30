//! Bradley-Terry preference ranker over CLIP embeddings. Each labelled pair
//! says one image beat another. We fit a direction `w` so that
//! `P(a beats b) = sigmoid(w · a - w · b)`, and an image's score is `w · e`.

use crate::{App, db};

/// Full-batch gradient descent steps. Milliseconds for a few hundred pairs.
const ITERATIONS: usize = 300;
const LEARNING_RATE: f32 = 1.0;
/// L2 penalty. Keeps `w` small when pairs are few and dimensions are many.
const L2: f32 = 0.01;

/// Fit `w` from (winner, loser) embedding pairs. Minimises the mean logistic
/// loss on the embedding differences plus an L2 penalty. There is no bias
/// term, since swapping a pair must flip the prediction exactly.
pub fn train(pairs: &[(Vec<f32>, Vec<f32>)]) -> Vec<f32> {
    let dim = pairs.first().map(|(winner, _)| winner.len()).unwrap_or_default();
    let diffs: Vec<Vec<f32>> = pairs
        .iter()
        .map(|(winner, loser)| winner.iter().zip(loser).map(|(a, b)| a - b).collect())
        .collect();

    let mut weights = vec![0f32; dim];
    for _ in 0..ITERATIONS {
        let mut gradient: Vec<f32> = weights.iter().map(|w| 2.0 * L2 * w).collect();
        for diff in &diffs {
            // d/dw of -log sigmoid(w · diff) is (sigmoid(w · diff) - 1) * diff.
            let probability = 1.0 / (1.0 + (-dot(&weights, diff)).exp());
            let coefficient = (probability - 1.0) / diffs.len() as f32;
            for (g, x) in gradient.iter_mut().zip(diff) {
                *g += coefficient * x;
            }
        }
        for (w, g) in weights.iter_mut().zip(&gradient) {
            *w -= LEARNING_RATE * g;
        }
    }
    weights
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Retrain from every stored pair, save the model, and rescore every embedded
/// image. Returns the number of pairs used.
pub async fn retrain(app: &App) -> anyhow::Result<usize> {
    let pairs = db::pair_embeddings(&app.pool).await?;
    anyhow::ensure!(!pairs.is_empty(), "no labelled pairs to train on");
    let weights = train(&pairs);
    db::save_model(&app.pool, &weights).await?;
    db::rescore(&app.pool).await?;

    // Score the new model on both sets and keep the result, so runs can be
    // compared over time.
    let (_, training_agreed) = db::agreement(&app.pool, false).await?;
    let (validation_total, validation_agreed) = db::agreement(&app.pool, true).await?;
    db::insert_training_run(
        &app.pool,
        &db::TrainingRun {
            trained_at: chrono::Utc::now(),
            pairs: pairs.len() as i32,
            training_agreed: training_agreed as i32,
            validation_total: validation_total as i32,
            validation_agreed: validation_agreed as i32,
        },
    )
    .await?;
    tracing::info!(
        pairs = pairs.len(),
        training_agreed,
        validation_total,
        validation_agreed,
        "ranker retrained"
    );
    Ok(pairs.len())
}

#[cfg(test)]
mod tests {
    use rand::{RngExt, SeedableRng, rngs::StdRng};

    use super::*;

    fn unit(rng: &mut StdRng, dim: usize) -> Vec<f32> {
        let v: Vec<f32> = (0..dim).map(|_| rng.random_range(-1.0f32..1.0)).collect();
        let norm = dot(&v, &v).sqrt();
        v.into_iter().map(|x| x / norm).collect()
    }

    #[test]
    fn recovers_hidden_preference_direction() {
        // Preferences follow a hidden direction. Training on 400 pairs should
        // point `w` the same way. 64 dimensions keeps the pairs-per-dimension
        // ratio realistic without needing thousands of synthetic pairs.
        let mut rng = StdRng::seed_from_u64(7);
        let hidden = unit(&mut rng, 64);
        let pairs: Vec<_> = (0..400)
            .map(|_| {
                let a = unit(&mut rng, 64);
                let b = unit(&mut rng, 64);
                if dot(&hidden, &a) > dot(&hidden, &b) { (a, b) } else { (b, a) }
            })
            .collect();

        let weights = train(&pairs);
        let cosine = dot(&weights, &hidden) / dot(&weights, &weights).sqrt();
        assert!(cosine > 0.9, "cosine was {cosine}");
    }
}
