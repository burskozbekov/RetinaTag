// face.rs — Face detection (SCRFD-500M) + embedding (MobileFaceNet) via tract-onnx
//
// Models expected in <resource_dir>/models/:
//   det_500m.onnx   — InsightFace SCRFD-500M face detector (~500 KB)
//   w600k_mbf.onnx  — InsightFace MobileFaceNet embedder  (~4 MB)
//
// Download with: ./download_models.ps1

use anyhow::{bail, Context, Result};
use image::{DynamicImage, GenericImageView};
use std::path::Path;
use tract_onnx::prelude::*;

// ── Constants ─────────────────────────────────────────────────────────────────

const DET_SIZE: usize = 640;   // SCRFD input
const EMB_SIZE: usize = 112;   // MobileFaceNet input
const SCORE_THRESH: f32 = 0.5;
const NMS_THRESH: f32 = 0.4;
pub const RECOGNITION_THRESH: f32 = 0.55; // cosine similarity threshold (higher = stricter)

// ── Model types ───────────────────────────────────────────────────────────────

type TractPlan = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

pub struct FaceModels {
    detector: TractPlan,
    embedder: TractPlan,
}

// tract's SimplePlan is Send+Sync (contains Arc internally)
unsafe impl Send for FaceModels {}
unsafe impl Sync for FaceModels {}

// ── Detected face ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DetectedFace {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub score: f32,
}

// ── Load models ───────────────────────────────────────────────────────────────

pub fn load_models(models_dir: &Path) -> Result<FaceModels> {
    let det_path = models_dir.join("det_500m.onnx");
    let emb_path = models_dir.join("w600k_mbf.onnx");

    if !det_path.exists() {
        bail!(
            "Face detection model not found: {:?}\n\
             Please run download_models.ps1 or use Settings > Face Recognition > Download.",
            det_path
        );
    }
    if !emb_path.exists() {
        bail!(
            "Face embedding model not found: {:?}\n\
             Please run download_models.ps1 or use Settings > Face Recognition > Download.",
            emb_path
        );
    }

    let detector = tract_onnx::onnx()
        .model_for_path(&det_path)
        .context("Failed to load detection model")?
        .into_optimized()
        .context("Failed to optimize detection model")?
        .into_runnable()
        .context("Failed to make detection model runnable")?;

    let embedder = tract_onnx::onnx()
        .model_for_path(&emb_path)
        .context("Failed to load embedding model")?
        .into_optimized()
        .context("Failed to optimize embedding model")?
        .into_runnable()
        .context("Failed to make embedding model runnable")?;

    Ok(FaceModels { detector, embedder })
}

// ── Face Detection (SCRFD-500M) ───────────────────────────────────────────────
//
// SCRFD output layout for 640×640 input (no keypoints variant):
//   outputs[0..2] — confidence scores for stride {8, 16, 32}
//   outputs[3..5] — bounding box deltas for stride {8, 16, 32}
//
// Each stride s has fh = 640/s, fw = 640/s, n = fh*fw*2 anchors.
// Anchor center: cx = (col + 0.5)*s,  cy = (row + 0.5)*s
// Box decode:    x1 = cx - d0*s,  y1 = cy - d1*s,  x2 = cx + d2*s,  y2 = cy + d3*s

pub fn detect_faces(models: &FaceModels, img: &DynamicImage) -> Result<Vec<DetectedFace>> {
    let (orig_w, orig_h) = img.dimensions();

    // Resize to 640×640
    let resized = img
        .resize_exact(
            DET_SIZE as u32,
            DET_SIZE as u32,
            image::imageops::FilterType::Triangle,
        )
        .to_rgb8();

    // Build input [1, 3, 640, 640] normalised with mean=127.5, std=128
    let mut input =
        tract_ndarray::Array4::<f32>::zeros((1, 3, DET_SIZE, DET_SIZE));
    for y in 0..DET_SIZE {
        for x in 0..DET_SIZE {
            let p = resized.get_pixel(x as u32, y as u32);
            input[[0, 0, y, x]] = (p[0] as f32 - 127.5) / 128.0;
            input[[0, 1, y, x]] = (p[1] as f32 - 127.5) / 128.0;
            input[[0, 2, y, x]] = (p[2] as f32 - 127.5) / 128.0;
        }
    }

    let t: Tensor = input.into();
    let results = models.detector.run(tvec!(t.into()))?;

    let scale_x = orig_w as f32 / DET_SIZE as f32;
    let scale_y = orig_h as f32 / DET_SIZE as f32;
    let strides = [8usize, 16, 32];
    let fmc = strides.len(); // 3

    let mut faces = Vec::new();

    for (idx, &stride) in strides.iter().enumerate() {
        let fh = DET_SIZE / stride;
        let fw = DET_SIZE / stride;
        let n = fh * fw * 2; // 2 anchors per cell

        let scores = results[idx].to_array_view::<f32>()?;
        let bboxes = results[idx + fmc].to_array_view::<f32>()?;

        // Pre-compute anchor centres in 640×640 space
        // Order: rows outer, cols inner, 2 anchors per cell
        let mut centers: Vec<(f32, f32)> = Vec::with_capacity(n);
        for row in 0..fh {
            for col in 0..fw {
                let cx = (col as f32 + 0.5) * stride as f32;
                let cy = (row as f32 + 0.5) * stride as f32;
                centers.push((cx, cy));
                centers.push((cx, cy)); // two anchors share same centre
            }
        }

        for i in 0..n.min(centers.len()) {
            // Handle both [1, n, 1] and [n, 1] output shapes
            let score = match scores.ndim() {
                3 => scores[[0, i, 0]],
                2 => scores[[i, 0]],
                1 => scores[[i]],
                _ => continue,
            };
            if score < SCORE_THRESH {
                continue;
            }

            let (d0, d1, d2, d3) = match bboxes.ndim() {
                3 => (bboxes[[0, i, 0]], bboxes[[0, i, 1]], bboxes[[0, i, 2]], bboxes[[0, i, 3]]),
                2 => (bboxes[[i, 0]], bboxes[[i, 1]], bboxes[[i, 2]], bboxes[[i, 3]]),
                _ => continue,
            };

            let (cx, cy) = centers[i];
            let s = stride as f32;
            let x1 = ((cx - d0 * s) * scale_x) as i32;
            let y1 = ((cy - d1 * s) * scale_y) as i32;
            let x2 = ((cx + d2 * s) * scale_x) as i32;
            let y2 = ((cy + d3 * s) * scale_y) as i32;

            let x1 = x1.max(0);
            let y1 = y1.max(0);
            let x2 = x2.min(orig_w as i32);
            let y2 = y2.min(orig_h as i32);

            if x2 > x1 + 8 && y2 > y1 + 8 {
                faces.push(DetectedFace { x1, y1, x2, y2, score });
            }
        }
    }

    Ok(nms(faces, NMS_THRESH))
}

// ── Face Embedding (MobileFaceNet / ArcFace) ──────────────────────────────────
//
// Input:  [1, 3, 112, 112], normalised same as detector (mean=127.5, std=128)
// Output: [1, 512] L2-normalised embedding vector

pub fn get_embedding(
    models: &FaceModels,
    img: &DynamicImage,
    face: &DetectedFace,
) -> Result<Vec<f32>> {
    let (iw, ih) = (img.width() as i32, img.height() as i32);

    // Crop with ~20 % padding to include forehead / chin
    let pw = ((face.x2 - face.x1) / 5).max(8);
    let ph = ((face.y2 - face.y1) / 5).max(8);
    let x1 = (face.x1 - pw).max(0) as u32;
    let y1 = (face.y1 - ph).max(0) as u32;
    let x2 = (face.x2 + pw).min(iw) as u32;
    let y2 = (face.y2 + ph).min(ih) as u32;

    let crop = img
        .crop_imm(x1, y1, x2 - x1, y2 - y1)
        .resize_exact(
            EMB_SIZE as u32,
            EMB_SIZE as u32,
            image::imageops::FilterType::Triangle,
        )
        .to_rgb8();

    let mut input =
        tract_ndarray::Array4::<f32>::zeros((1, 3, EMB_SIZE, EMB_SIZE));
    for y in 0..EMB_SIZE {
        for x in 0..EMB_SIZE {
            let p = crop.get_pixel(x as u32, y as u32);
            input[[0, 0, y, x]] = (p[0] as f32 - 127.5) / 128.0;
            input[[0, 1, y, x]] = (p[1] as f32 - 127.5) / 128.0;
            input[[0, 2, y, x]] = (p[2] as f32 - 127.5) / 128.0;
        }
    }

    let t: Tensor = input.into();
    let results = models.embedder.run(tvec!(t.into()))?;
    let raw = results[0].to_array_view::<f32>()?;
    let mut emb: Vec<f32> = raw.iter().cloned().collect();

    // L2-normalise so dot-product == cosine similarity
    let norm: f32 = emb.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 1e-8 {
        emb.iter_mut().for_each(|v| *v /= norm);
    }
    Ok(emb)
}

// ── Similarity & serialisation ────────────────────────────────────────────────

/// Cosine similarity for L2-normalised vectors (= dot product).
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

// ── NMS ───────────────────────────────────────────────────────────────────────

fn nms(mut faces: Vec<DetectedFace>, thresh: f32) -> Vec<DetectedFace> {
    faces.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut suppressed = vec![false; faces.len()];
    let mut keep = Vec::new();
    for i in 0..faces.len() {
        if suppressed[i] {
            continue;
        }
        keep.push(faces[i].clone());
        for j in (i + 1)..faces.len() {
            if !suppressed[j] && iou(&faces[i], &faces[j]) > thresh {
                suppressed[j] = true;
            }
        }
    }
    keep
}

// ── Face Clustering (Greedy agglomerative) ────────────────────────────────────
//
// Input:  list of (face_id, embedding)
// Output: list of clusters, each cluster = Vec<face_id>
//
// Algorithm: greedy single-linkage
//   For each face (sorted by id), find the closest existing cluster.
//   If max similarity to any member of that cluster >= CLUSTER_THRESH → merge.
//   Otherwise → start new cluster.
//
// This is O(n²) — fine for typical libraries (<50k faces per folder).
// CLUSTER_THRESH is slightly lower than recognition thresh to be inclusive.

const CLUSTER_THRESH: f32 = 0.30;

pub struct FaceCluster {
    pub face_ids: Vec<i64>,
    /// Mean embedding of all members (normalised), used as the cluster centre.
    pub centroid: Vec<f32>,
    /// face_id chosen as the visual representative (highest average similarity)
    pub representative: i64,
}

pub fn cluster_embeddings(faces: &[(i64, Vec<f32>)]) -> Vec<FaceCluster> {
    if faces.is_empty() {
        return vec![];
    }

    let mut clusters: Vec<FaceCluster> = Vec::new();

    for (face_id, emb) in faces {
        // Find best matching cluster (by similarity to centroid)
        let best = clusters
            .iter()
            .enumerate()
            .map(|(ci, c)| (ci, cosine_similarity(emb, &c.centroid)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        if let Some((ci, sim)) = best {
            if sim >= CLUSTER_THRESH {
                // Merge into existing cluster, update centroid (running mean)
                let c = &mut clusters[ci];
                c.face_ids.push(*face_id);
                let n = c.face_ids.len() as f32;
                for (a, b) in c.centroid.iter_mut().zip(emb.iter()) {
                    *a = (*a * (n - 1.0) + b) / n;
                }
                // Re-normalise centroid
                let norm: f32 = c.centroid.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm > 1e-8 {
                    c.centroid.iter_mut().for_each(|v| *v /= norm);
                }
                continue;
            }
        }

        // Start new cluster
        clusters.push(FaceCluster {
            face_ids: vec![*face_id],
            centroid: emb.clone(),
            representative: *face_id,
        });
    }

    // Choose representative: face whose embedding is closest to the centroid
    for c in &mut clusters {
        if c.face_ids.len() == 1 {
            c.representative = c.face_ids[0];
            continue;
        }
        let best = faces
            .iter()
            .filter(|(id, _)| c.face_ids.contains(id))
            .map(|(id, emb)| (*id, cosine_similarity(emb, &c.centroid)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((id, _)) = best {
            c.representative = id;
        }
    }

    // Sort clusters largest first
    clusters.sort_by(|a, b| b.face_ids.len().cmp(&a.face_ids.len()));
    clusters
}

fn iou(a: &DetectedFace, b: &DetectedFace) -> f32 {
    let ix1 = a.x1.max(b.x1);
    let iy1 = a.y1.max(b.y1);
    let ix2 = a.x2.min(b.x2);
    let iy2 = a.y2.min(b.y2);
    if ix2 <= ix1 || iy2 <= iy1 {
        return 0.0;
    }
    let inter = ((ix2 - ix1) * (iy2 - iy1)) as f32;
    let aa = ((a.x2 - a.x1) * (a.y2 - a.y1)) as f32;
    let ab = ((b.x2 - b.x1) * (b.y2 - b.y1)) as f32;
    inter / (aa + ab - inter)
}
