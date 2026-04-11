// clip.rs — CLIP semantic search engine
//
// Two ONNX models (image encoder + text encoder) loaded via ONNX Runtime (ort).
// DirectML backend on Windows → uses any GPU (NVIDIA/AMD/Intel) automatically.
// Falls back to CPU if no GPU is available.
//
// Three model tiers (downloaded by user choice):
//   fast     → CLIP ViT-B/32 quantized  (~155 MB)  — any machine
//   balanced → CLIP ViT-B/32 full       (~600 MB)  — mid-range GPU
//   best     → CLIP ViT-L/14            (~1.7 GB)  — RTX 3090 class

use anyhow::{bail, Context, Result};
use image::DynamicImage;
use ndarray::{Array2, Array4};
use ort::session::{builder::GraphOptimizationLevel, Session};
use std::path::Path;

use crate::clip_tokenizer::ClipTokenizer;

// CLIP image normalisation (same for all variants)
const CLIP_MEAN: [f32; 3] = [0.48145466, 0.4578275,  0.40821073];
const CLIP_STD:  [f32; 3] = [0.26862954, 0.26130258, 0.27577711];
const CLIP_SIZE: u32 = 224;

// ── Tier definitions ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipTier {
    Fast,
    Balanced,
    Best,
}

impl ClipTier {
    pub fn dir_name(self) -> &'static str {
        match self {
            ClipTier::Fast     => "clip_fast",
            ClipTier::Balanced => "clip_balanced",
            ClipTier::Best     => "clip_best",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ClipTier::Fast     => "Fast (ViT-B/32 quantized)",
            ClipTier::Balanced => "Dengeli (ViT-B/32 full)",
            ClipTier::Best     => "Best (ViT-L/14)",
        }
    }

    pub fn size_mb(self) -> u32 {
        match self {
            ClipTier::Fast     => 155,
            ClipTier::Balanced => 600,
            ClipTier::Best     => 1700,
        }
    }

    pub fn embed_dim(self) -> usize {
        match self {
            ClipTier::Fast | ClipTier::Balanced => 512,
            ClipTier::Best                      => 768,
        }
    }

    /// Download URLs for (visual.onnx, textual.onnx, vocab.json, merges.txt)
    pub fn urls(self) -> (String, String, String, String) {
        let base_b32 = "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/main/onnx";
        let base_l14 = "https://huggingface.co/Xenova/clip-vit-large-patch14/resolve/main/onnx";
        let tok_b32  = "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/main";
        let tok_l14  = "https://huggingface.co/Xenova/clip-vit-large-patch14/resolve/main";

        match self {
            ClipTier::Fast => (
                format!("{}/visual_quantized.onnx",  base_b32),
                format!("{}/textual_quantized.onnx", base_b32),
                format!("{}/vocab.json",              tok_b32),
                format!("{}/merges.txt",              tok_b32),
            ),
            ClipTier::Balanced => (
                format!("{}/visual.onnx",  base_b32),
                format!("{}/textual.onnx", base_b32),
                format!("{}/vocab.json",   tok_b32),
                format!("{}/merges.txt",   tok_b32),
            ),
            ClipTier::Best => (
                format!("{}/visual.onnx",  base_l14),
                format!("{}/textual.onnx", base_l14),
                format!("{}/vocab.json",   tok_l14),
                format!("{}/merges.txt",   tok_l14),
            ),
        }
    }
}

// ── Engine ────────────────────────────────────────────────────────────────────

pub struct ClipEngine {
    visual:    Session,
    textual:   Session,
    tokenizer: ClipTokenizer,
    pub tier:  ClipTier,
}

// ort::Session is Send+Sync in practice (models loaded read-only)
unsafe impl Send for ClipEngine {}
unsafe impl Sync for ClipEngine {}

pub fn load_engine(models_dir: &Path, tier: ClipTier) -> Result<ClipEngine> {
    let dir = models_dir.join(tier.dir_name());

    // All tiers stored as visual.onnx / textual.onnx in their respective subdirs
    let visual_path  = dir.join("visual.onnx");
    let textual_path = dir.join("textual.onnx");

    if !visual_path.exists() || !textual_path.exists() {
        bail!(
            "CLIP models not found: {:?}\n\
             Please download them from Settings > Semantic Search.",
            dir
        );
    }

    let visual = Session::builder()
        .map_err(|e| anyhow::anyhow!("ONNX builder (visual): {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow::anyhow!("opt level (visual): {e}"))?
        .commit_from_file(&visual_path)
        .map_err(|e| anyhow::anyhow!("visual model loading: {e}"))?;

    let textual = Session::builder()
        .map_err(|e| anyhow::anyhow!("ONNX builder (textual): {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow::anyhow!("opt level (textual): {e}"))?
        .commit_from_file(&textual_path)
        .map_err(|e| anyhow::anyhow!("textual model loading: {e}"))?;

    let tokenizer = ClipTokenizer::load(&dir).context("failed to load tokenizer")?;

    Ok(ClipEngine { visual, textual, tokenizer, tier })
}

// ── Image encoding ────────────────────────────────────────────────────────────

/// Encode one image → L2-normalised embedding vector.
pub fn encode_image(engine: &mut ClipEngine, img: &DynamicImage) -> Result<Vec<f32>> {
    let rgb = img
        .resize_exact(CLIP_SIZE, CLIP_SIZE, image::imageops::FilterType::Triangle)
        .to_rgb8();

    let mut arr = Array4::<f32>::zeros((1, 3, CLIP_SIZE as usize, CLIP_SIZE as usize));
    for y in 0..CLIP_SIZE as usize {
        for x in 0..CLIP_SIZE as usize {
            let p = rgb.get_pixel(x as u32, y as u32);
            for c in 0..3 {
                arr[[0, c, y, x]] = (p[c] as f32 / 255.0 - CLIP_MEAN[c]) / CLIP_STD[c];
            }
        }
    }

    // Create ort Tensor from owned ndarray
    let vis_tensor = ort::value::Tensor::<f32>::from_array(arr)
        .map_err(|e| anyhow::anyhow!("image tensor: {e}"))?;

    let outputs = engine
        .visual
        .run(ort::inputs!["pixel_values" => vis_tensor])
        .map_err(|e| anyhow::anyhow!("visual run: {e}"))?;

    extract_emb(&outputs, engine.tier.embed_dim())
}

// ── Text encoding ─────────────────────────────────────────────────────────────

/// Encode a text query → L2-normalised embedding vector.
pub fn encode_text(engine: &mut ClipEngine, text: &str) -> Result<Vec<f32>> {
    let (ids, mask) = engine.tokenizer.encode(text);

    let ids_arr  = Array2::from_shape_vec((1, 77), ids )?;
    let mask_arr = Array2::from_shape_vec((1, 77), mask)?;

    let ids_tensor  = ort::value::Tensor::<i64>::from_array(ids_arr)
        .map_err(|e| anyhow::anyhow!("ids tensor: {e}"))?;
    let mask_tensor = ort::value::Tensor::<i64>::from_array(mask_arr)
        .map_err(|e| anyhow::anyhow!("mask tensor: {e}"))?;

    let outputs = engine.textual
        .run(ort::inputs!["input_ids" => ids_tensor, "attention_mask" => mask_tensor])
        .map_err(|e| anyhow::anyhow!("textual run: {e}"))?;

    extract_emb(&outputs, engine.tier.embed_dim())
}

fn extract_emb(outputs: &ort::session::SessionOutputs, dim: usize) -> Result<Vec<f32>> {
    // try_extract_tensor returns (&Shape, &[T]) in ort rc.12
    let raw: Vec<f32> = if let Some(v) = outputs.get("image_embeds") {
        let (_, data): (_, &[f32]) = v.try_extract_tensor()
            .map_err(|e| anyhow::anyhow!("image_embeds extract: {e}"))?;
        data.to_vec()
    } else if let Some(v) = outputs.get("text_embeds") {
        let (_, data): (_, &[f32]) = v.try_extract_tensor()
            .map_err(|e| anyhow::anyhow!("text_embeds extract: {e}"))?;
        data.to_vec()
    } else if let Some(v) = outputs.get("last_hidden_state") {
        // Full hidden states → take CLS token (first `dim` elements)
        let (_, data): (_, &[f32]) = v.try_extract_tensor()
            .map_err(|e| anyhow::anyhow!("last_hidden_state extract: {e}"))?;
        data.iter().take(dim).cloned().collect()
    } else {
        // Fall back: first output
        let (_, v) = outputs.iter().next().context("no outputs")?;
        let (_, data): (_, &[f32]) = v.try_extract_tensor()
            .map_err(|e| anyhow::anyhow!("fallback extract: {e}"))?;
        data.to_vec()
    };

    Ok(l2_normalise(raw))
}

// ── Cosine similarity ─────────────────────────────────────────────────────────

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn embedding_to_bytes(emb: &[f32]) -> Vec<u8> {
    emb.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn bytes_to_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

fn l2_normalise(mut v: Vec<f32>) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}
