//! CLIP image embeddings on the CPU via candle. Only the vision tower and its
//! projection are loaded, giving a unit-length 512-d vector per image.

use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{Linear, VarBuilder};
use candle_transformers::models::clip::{
    div_l2_norm,
    vision_model::{ClipVisionConfig, ClipVisionTransformer},
};
use image::{GenericImageView, imageops::FilterType};
use tokio::sync::OnceCell;

use crate::{App, Config, db};

/// Safetensors for `openai/clip-vit-base-patch32`. The upstream repo only
/// ships PyTorch weights on `main`, so this points at the safetensors PR.
const WEIGHTS_URL: &str =
    "https://huggingface.co/openai/clip-vit-base-patch32/resolve/refs%2Fpr%2F15/model.safetensors";
const WEIGHTS_FILE: &str = "clip-vit-base-patch32.safetensors";
/// Input resolution the model was trained at.
const SIZE: u32 = 224;
/// CLIP's per-channel normalisation constants.
const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const STD: [f32; 3] = [0.268_629_54, 0.261_302_6, 0.275_777_1];
/// How many unembedded images one pass handles.
const BATCH: i64 = 10_000;

/// The loaded vision tower.
pub struct Clip {
    vision: ClipVisionTransformer,
    projection: Linear,
}

impl Clip {
    /// Memory-map the weights and build the vision tower.
    fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        // SAFETY: the file is only written by `download` and is never modified
        // while mapped.
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[path], DType::F32, &Device::Cpu)? };
        let config = ClipVisionConfig::vit_base_patch32();
        let vision = ClipVisionTransformer::new(vb.pp("vision_model"), &config)?;
        let projection = candle_nn::linear_no_bias(
            config.embed_dim,
            config.projection_dim,
            vb.pp("visual_projection"),
        )?;
        Ok(Clip { vision, projection })
    }

    /// Embed one encoded image. Blocking and CPU-bound.
    pub fn embed(&self, bytes: &[u8]) -> anyhow::Result<Vec<f32>> {
        let pixels = preprocess(bytes)?;
        let features = self.vision.forward(&pixels)?.apply(&self.projection)?;
        Ok(div_l2_norm(&features)?.squeeze(0)?.to_vec1()?)
    }
}

/// Decode, resize the shorter side to 224, centre crop, and normalise into a
/// `(1, 3, 224, 224)` tensor.
fn preprocess(bytes: &[u8]) -> anyhow::Result<Tensor> {
    let decoded = image::load_from_memory(bytes).context("decoding image")?;
    let (width, height) = decoded.dimensions();
    let scale = SIZE as f32 / width.min(height) as f32;
    let resized = decoded.resize_exact(
        (width as f32 * scale).round().max(SIZE as f32) as u32,
        (height as f32 * scale).round().max(SIZE as f32) as u32,
        FilterType::Triangle,
    );
    let (width, height) = resized.dimensions();
    let cropped = resized.crop_imm((width - SIZE) / 2, (height - SIZE) / 2, SIZE, SIZE).to_rgb8();

    // Channel-major layout, normalised per channel.
    let pixels = (SIZE * SIZE) as usize;
    let mut data = vec![0f32; 3 * pixels];
    for (index, pixel) in cropped.pixels().enumerate() {
        for channel in 0..3 {
            data[channel * pixels + index] =
                (pixel.0[channel] as f32 / 255.0 - MEAN[channel]) / STD[channel];
        }
    }
    Ok(Tensor::from_vec(data, (3, SIZE as usize, SIZE as usize), &Device::Cpu)?.unsqueeze(0)?)
}

/// Where the weights live on disk.
fn weights_path(config: &Config) -> PathBuf {
    config.data_dir.join(WEIGHTS_FILE)
}

/// Download the weights if they are not already present. Writes to a temp
/// file and renames, so a partial download is never mistaken for a model.
async fn download(http: &reqwest::Client, path: &std::path::Path) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    tracing::info!(url = WEIGHTS_URL, "downloading CLIP weights, about 600 MB");
    tokio::fs::create_dir_all(path.parent().context("weights path has no parent")?).await?;
    let partial = path.with_extension("partial");
    let mut file = tokio::fs::File::create(&partial).await?;
    let mut response = http.get(WEIGHTS_URL).send().await?.error_for_status()?;
    while let Some(chunk) = response.chunk().await? {
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
    }
    file.sync_all().await?;
    tokio::fs::rename(partial, path).await?;
    Ok(())
}

/// The model, loaded once per process on first use.
async fn model(app: &App) -> anyhow::Result<Arc<Clip>> {
    static MODEL: OnceCell<Arc<Clip>> = OnceCell::const_new();
    MODEL
        .get_or_try_init(|| async {
            let path = weights_path(&app.config);
            download(&app.http, &path).await?;
            let clip = tokio::task::spawn_blocking(move || Clip::load(&path)).await??;
            tracing::info!("CLIP weights loaded");
            Ok(Arc::new(clip))
        })
        .await
        .cloned()
}

/// Embed every image that does not have an embedding yet. Images that cannot
/// be fetched or decoded are logged and left for the next pass.
pub async fn embed_pending(app: &App) -> anyhow::Result<usize> {
    let pending = db::unembedded_images(&app.pool, BATCH).await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let clip = model(app).await?;
    tracing::info!(count = pending.len(), "embedding images");

    // ponytail: sequential. Each image is a download plus roughly 100 ms of
    // CPU. Parallelise the downloads if seeding ever feels slow.
    let mut done = 0;
    for (id, url) in pending {
        let bytes = match app.http.get(&url).send().await.and_then(|r| r.error_for_status()) {
            Ok(response) => response.bytes().await?,
            Err(error) => {
                tracing::warn!(id, %error, "could not fetch image");
                continue;
            }
        };
        let clip = clip.clone();
        match tokio::task::spawn_blocking(move || clip.embed(&bytes)).await? {
            Ok(embedding) => db::set_embedding(&app.pool, id, &embedding).await?,
            Err(error) => {
                tracing::warn!(id, %error, "could not embed image");
                continue;
            }
        }
        done += 1;
        if done % 50 == 0 {
            tracing::info!(done, "embedding images");
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use image::{ImageFormat, RgbImage};

    use super::*;

    #[test]
    fn preprocess_crops_to_model_input() {
        // A 300x200 image: shorter side scales to 224, then a centre crop.
        let mut png = std::io::Cursor::new(Vec::new());
        RgbImage::from_pixel(300, 200, image::Rgb([255, 0, 0]))
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let tensor = preprocess(png.get_ref()).unwrap();
        assert_eq!(tensor.dims(), &[1, 3, 224, 224]);

        // Pure red normalises to a known value per channel.
        let flat: Vec<f32> = tensor.flatten_all().unwrap().to_vec1().unwrap();
        let red = (1.0 - MEAN[0]) / STD[0];
        let green = (0.0 - MEAN[1]) / STD[1];
        assert!((flat[0] - red).abs() < 1e-5);
        assert!((flat[224 * 224] - green).abs() < 1e-5);
    }
}
