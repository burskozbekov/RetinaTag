// face.rs — Face detection (SCRFD-500M) + embedding (ResNet50/MobileFaceNet) via ort (ONNX Runtime)
//
// Models expected in <app_local_data>/models/:
//   det_500m.onnx   — InsightFace SCRFD-500M face detector
//   w600k_r50.onnx  — InsightFace ResNet50 embedder (preferred, MR-ALL 91.25)
//   w600k_mbf.onnx  — InsightFace MobileFaceNet embedder (fallback, MR-ALL 71.87)
//
// Uses the same ort backend as clip.rs (DirectML GPU on Windows, CPU fallback).

use anyhow::{bail, Result};
use image::{DynamicImage, GenericImageView};
use ndarray::Array4;
use ort::session::{builder::GraphOptimizationLevel, Session};
use std::path::Path;

// ── Constants ─────────────────────────────────────────────────────────────────

const DET_SIZE: usize = 640;   // SCRFD input
const EMB_SIZE: usize = 112;   // Face recognition input (same for MobileFaceNet and ResNet50)
// Score threshold: 0.45 is the InsightFace default for SCRFD. 0.55 was too
// strict and was silently dropping real-world phone/DSLR faces. We still
// filter noise downstream via the skin/texture check in commands.rs and the
// embedding-norm gate in `get_embedding`.
const SCORE_THRESH: f32 = 0.45;
const NMS_THRESH: f32 = 0.4;
pub const RECOGNITION_THRESH: f32 = 0.60;

// ── Model types ───────────────────────────────────────────────────────────────

pub struct FaceModels {
    detector: std::sync::Mutex<Session>,
    embedder: std::sync::Mutex<Session>,
}

// ── Detected face ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DetectedFace {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub score: f32,
    /// 5-point facial landmarks: [left_eye, right_eye, nose, left_mouth, right_mouth]
    /// Each is (x, y) in original image coordinates. Used for face alignment.
    pub landmarks: Option<[(f32, f32); 5]>,
}

// ── Load models ───────────────────────────────────────────────────────────────

pub fn load_models(models_dir: &Path) -> Result<FaceModels> {
    let det_path = models_dir.join("det_500m.onnx");
    // Prefer ResNet50 (MR-ALL 91.25) over MobileFaceNet (MR-ALL 71.87)
    let emb_path = if models_dir.join("w600k_r50.onnx").exists() {
        eprintln!("[face] Using ResNet50 recognition model (high accuracy)");
        models_dir.join("w600k_r50.onnx")
    } else if models_dir.join("w600k_mbf.onnx").exists() {
        eprintln!("[face] Using MobileFaceNet recognition model (fallback)");
        models_dir.join("w600k_mbf.onnx")
    } else {
        bail!("Face embedding model not found (need w600k_r50.onnx or w600k_mbf.onnx)");
    };

    if !det_path.exists() {
        bail!("Face detection model not found: {:?}", det_path);
    }

    let detector = Session::builder()
        .map_err(|e| anyhow::anyhow!("ort session builder: {}", e))?
.with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow::anyhow!("ort opt level: {}", e))?
        .commit_from_file(&det_path)
        .map_err(|e| anyhow::anyhow!("Failed to load detection model: {}", e))?;

    let embedder = Session::builder()
        .map_err(|e| anyhow::anyhow!("ort session builder: {}", e))?
.with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow::anyhow!("ort opt level: {}", e))?
        .commit_from_file(&emb_path)
        .map_err(|e| anyhow::anyhow!("Failed to load embedding model: {}", e))?;

    Ok(FaceModels {
        detector: std::sync::Mutex::new(detector),
        embedder: std::sync::Mutex::new(embedder),
    })
}

// ── Face Detection (SCRFD-500M) ───────────────────────────────────────────────

pub fn detect_faces(models: &FaceModels, img: &DynamicImage) -> Result<Vec<DetectedFace>> {
    let (orig_w, orig_h) = img.dimensions();

    // ── Letterbox resize (aspect-ratio preserving) ───────────────────────
    // CRITICAL: anisotropic resize_exact stretches faces, which makes SCRFD
    // predict landmarks in distorted positions. Misplaced landmarks → bad
    // similarity transform → garbage embedding. See InsightFace reference:
    //   insightface/python-package/insightface/app/face_analysis.py
    //   uses uniform scale + zero-padded canvas + single det_scale.
    let det_scale = (DET_SIZE as f32 / orig_w as f32).min(DET_SIZE as f32 / orig_h as f32);
    let new_w = ((orig_w as f32) * det_scale) as u32;
    let new_h = ((orig_h as f32) * det_scale) as u32;
    let resized = img
        .resize_exact(new_w, new_h, image::imageops::FilterType::Triangle)
        .to_rgb8();

    // Build input [1, 3, 640, 640] pre-filled with normalised black
    // (raw 0 → (0 - 127.5)/128.0 = -0.99609375)
    let fill_val: f32 = -127.5 / 128.0;
    let mut input = Array4::<f32>::from_elem((1, 3, DET_SIZE, DET_SIZE), fill_val);
    for y in 0..(new_h as usize).min(DET_SIZE) {
        for x in 0..(new_w as usize).min(DET_SIZE) {
            let p = resized.get_pixel(x as u32, y as u32);
            input[[0, 0, y, x]] = (p[0] as f32 - 127.5) / 128.0;
            input[[0, 1, y, x]] = (p[1] as f32 - 127.5) / 128.0;
            input[[0, 2, y, x]] = (p[2] as f32 - 127.5) / 128.0;
        }
    }

    let input_tensor = ort::value::Tensor::from_array(input)
        .map_err(|e| anyhow::anyhow!("det tensor: {}", e))?;

    // Run detector and copy output data (so we can release the session lock)
    // SCRFD outputs: [0-2] scores, [3-5] bboxes, [6-8] keypoints (5 landmarks × 2 coords)
    let output_data: Vec<(Vec<f32>, Vec<f32>, Vec<f32>)> = {
        let mut det = models.detector.lock().unwrap();
        let outputs = det.run(ort::inputs!["input.1" => input_tensor])
            .map_err(|e| anyhow::anyhow!("det run: {}", e))?;
        let strides_tmp = [8usize, 16, 32];
        let fmc_tmp = strides_tmp.len();
        let mut data = Vec::new();
        for idx in 0..fmc_tmp {
            let (_, sd): (_, &[f32]) = outputs[idx].try_extract_tensor()
                .map_err(|e| anyhow::anyhow!("scores[{}] extract: {}", idx, e))?;
            let (_, bd): (_, &[f32]) = outputs[idx + fmc_tmp].try_extract_tensor()
                .map_err(|e| anyhow::anyhow!("bboxes[{}] extract: {}", idx, e))?;
            // Extract keypoints (5 landmarks per face) — may not exist in all models
            let kd: Vec<f32> = if outputs.len() > idx + 2 * fmc_tmp {
                let (_, kp): (_, &[f32]) = outputs[idx + 2 * fmc_tmp].try_extract_tensor()
                    .map_err(|e| anyhow::anyhow!("kpts[{}] extract: {}", idx, e))?;
                kp.to_vec()
            } else {
                vec![]
            };
            data.push((sd.to_vec(), bd.to_vec(), kd));
        }
        data
    }; // det lock released here

    // Single inverse scale to go from 640-space back to original image space
    // (matches InsightFace's `det_scale` reciprocal usage).
    let inv_scale = 1.0 / det_scale;
    let scale_x = inv_scale;
    let scale_y = inv_scale;
    let strides = [8usize, 16, 32];

    let mut faces = Vec::new();

    for (idx, &stride) in strides.iter().enumerate() {
        let fh = DET_SIZE / stride;
        let fw = DET_SIZE / stride;
        let n = fh * fw * 2;

        let scores_data = &output_data[idx].0;
        let bboxes_data = &output_data[idx].1;
        let kpts_data = &output_data[idx].2;

        let mut centers: Vec<(f32, f32)> = Vec::with_capacity(n);
        for row in 0..fh {
            for col in 0..fw {
                // InsightFace SCRFD anchor centres: (col*stride, row*stride),
                // NO half-pixel offset. See:
                //   insightface/python-package/insightface/model_zoo/scrfd.py
                //   anchor_centers = (np.mgrid[:H,:W][::-1] * stride).reshape(-1, 2)
                // Adding +0.5 shifts landmarks by half-a-stride which at stride=32
                // is a 16-px error on the input → bad alignment → bad embeddings.
                let cx = col as f32 * stride as f32;
                let cy = row as f32 * stride as f32;
                centers.push((cx, cy));
                centers.push((cx, cy));
            }
        }

        for i in 0..n.min(centers.len()) {
            // Flat array: scores_data is [n*1], bboxes_data is [n*4]
            if i >= scores_data.len() { break; }
            let score = scores_data[i];
            if score < SCORE_THRESH { continue; }

            let bi = i * 4;
            if bi + 3 >= bboxes_data.len() { break; }
            let (d0, d1, d2, d3) = (bboxes_data[bi], bboxes_data[bi+1], bboxes_data[bi+2], bboxes_data[bi+3]);

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

            let fw = x2 - x1;
            let fh = y2 - y1;
            // Filter out tiny faces (background people, paintings in frames, crowds)
            // - Absolute minimum: 40px (below this, recognition quality is poor)
            // - Relative minimum: 2.5% of shortest image dimension
            //   e.g. 4000x3000 image → min 75px (keeps main subjects, filters background)
            //   Tuned DOWN from 60/5% because the previous values were silently
            //   dropping half-body group shots and wider landscape portraits
            //   — especially on 12MP phone photos where a single subject's
            //   face is often 80-140px, not 150+.
            let min_abs = 40;
            let min_rel = (orig_w.min(orig_h) as f32 * 0.025) as i32;
            let min_size = min_abs.max(min_rel);
            if fw >= min_size && fh >= min_size {
                // Extract 5-point landmarks if available
                let landmarks = if !kpts_data.is_empty() {
                    let ki = i * 10; // 5 landmarks × 2 coords
                    if ki + 10 <= kpts_data.len() {
                        let (cx_f, cy_f) = centers[i];
                        let s_f = stride as f32;
                        Some([
                            ((cx_f + kpts_data[ki]   * s_f) * scale_x, (cy_f + kpts_data[ki+1] * s_f) * scale_y),
                            ((cx_f + kpts_data[ki+2] * s_f) * scale_x, (cy_f + kpts_data[ki+3] * s_f) * scale_y),
                            ((cx_f + kpts_data[ki+4] * s_f) * scale_x, (cy_f + kpts_data[ki+5] * s_f) * scale_y),
                            ((cx_f + kpts_data[ki+6] * s_f) * scale_x, (cy_f + kpts_data[ki+7] * s_f) * scale_y),
                            ((cx_f + kpts_data[ki+8] * s_f) * scale_x, (cy_f + kpts_data[ki+9] * s_f) * scale_y),
                        ])
                    } else { None }
                } else { None };

                faces.push(DetectedFace { x1, y1, x2, y2, score, landmarks });
            }
        }
    }

    Ok(nms(faces, NMS_THRESH))
}

// ── Face Embedding with Alignment (ArcFace standard pipeline) ────────────────
//
// Critical: InsightFace models expect ALIGNED face crops, not raw bounding box crops.
// Alignment uses 5 facial landmarks → similarity transform → canonical position.
// Without alignment, same person at different angles produces very different embeddings.

/// ArcFace canonical landmark positions for 112×112 input
const ARCFACE_DST: [(f32, f32); 5] = [
    (38.2946, 51.6963), // left eye
    (73.5318, 51.5014), // right eye
    (56.0252, 71.7366), // nose tip
    (41.5493, 92.3655), // left mouth corner
    (70.7299, 92.2041), // right mouth corner
];

/// Estimate 2×3 similarity transform matrix from source landmarks to canonical positions.
/// Uses least-squares fitting of: dst = M * src (affine with uniform scale + rotation).
fn estimate_similarity_transform(src: &[(f32, f32); 5]) -> [[f32; 3]; 2] {
    let dst = &ARCFACE_DST;
    // Compute means
    let (mut sx, mut sy, mut dx, mut dy) = (0.0f32, 0.0, 0.0, 0.0);
    for i in 0..5 {
        sx += src[i].0; sy += src[i].1;
        dx += dst[i].0; dy += dst[i].1;
    }
    sx /= 5.0; sy /= 5.0; dx /= 5.0; dy /= 5.0;

    // Center the points
    let mut src_c = [(0.0f32, 0.0f32); 5];
    let mut dst_c = [(0.0f32, 0.0f32); 5];
    for i in 0..5 {
        src_c[i] = (src[i].0 - sx, src[i].1 - sy);
        dst_c[i] = (dst[i].0 - dx, dst[i].1 - dy);
    }

    // Compute the 2×2 part of similarity transform using Procrustes
    // M = [a -b tx; b a ty] where (a,b) encodes scale+rotation
    let mut num_a = 0.0f32;
    let mut num_b = 0.0f32;
    let mut denom = 0.0f32;
    for i in 0..5 {
        num_a += src_c[i].0 * dst_c[i].0 + src_c[i].1 * dst_c[i].1;
        num_b += src_c[i].0 * dst_c[i].1 - src_c[i].1 * dst_c[i].0;
        denom += src_c[i].0 * src_c[i].0 + src_c[i].1 * src_c[i].1;
    }

    if denom < 1e-8 {
        // Degenerate case — return identity-ish crop
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    }

    let a = num_a / denom;
    let b = num_b / denom;
    // Forward transform: dst_x = a*src_x - b*src_y + tx
    //                    dst_y = b*src_x + a*src_y + ty
    // At the mean: tx = dx - (a*sx - b*sy), ty = dy - (b*sx + a*sy)
    let tx = dx - a * sx + b * sy;
    let ty = dy - b * sx - a * sy;

    // INVERSE transform: we need to map from dst (112×112) back to source image
    // Forward: dst = [a -b; b a] * src + [tx; ty]
    // Inverse: src = [a b; -b a] / (a²+b²) * (dst - [tx; ty])
    let det = a * a + b * b;
    let ai = a / det;
    let bi = b / det;
    let txi = -(ai * tx + bi * ty);
    let tyi = -((-bi) * tx + ai * ty);

    // Return INVERSE matrix: maps 112×112 pixel → source image pixel
    [[ai, bi, txi], [-bi, ai, tyi]]
}

pub fn get_embedding(
    models: &FaceModels,
    img: &DynamicImage,
    face: &DetectedFace,
) -> Result<Vec<f32>> {
    let rgb_img = img.to_rgb8();
    let (iw, ih) = (rgb_img.width(), rgb_img.height());

    // Create aligned 112×112 face crop
    let crop = if let Some(ref lmk) = face.landmarks {
        // Use similarity transform alignment (the proper InsightFace way)
        let inv_m = estimate_similarity_transform(lmk);
        let mut aligned = image::RgbImage::new(EMB_SIZE as u32, EMB_SIZE as u32);

        // BILINEAR sampling — InsightFace uses cv2.warpAffine with INTER_LINEAR.
        // Nearest-neighbour costs 0.05–0.10 cosine sim on faces that are large
        // in the source image (heavy downsampling to 112×112 → aliasing).
        for dst_y in 0..EMB_SIZE as u32 {
            for dst_x in 0..EMB_SIZE as u32 {
                let src_x = inv_m[0][0] * dst_x as f32 + inv_m[0][1] * dst_y as f32 + inv_m[0][2];
                let src_y = inv_m[1][0] * dst_x as f32 + inv_m[1][1] * dst_y as f32 + inv_m[1][2];

                let sx0 = src_x.floor() as i32;
                let sy0 = src_y.floor() as i32;
                let fx = src_x - sx0 as f32;
                let fy = src_y - sy0 as f32;
                let sx1 = sx0 + 1;
                let sy1 = sy0 + 1;

                if sx0 >= 0 && sy0 >= 0 && sx1 < iw as i32 && sy1 < ih as i32 {
                    let p00 = rgb_img.get_pixel(sx0 as u32, sy0 as u32);
                    let p10 = rgb_img.get_pixel(sx1 as u32, sy0 as u32);
                    let p01 = rgb_img.get_pixel(sx0 as u32, sy1 as u32);
                    let p11 = rgb_img.get_pixel(sx1 as u32, sy1 as u32);
                    let w00 = (1.0 - fx) * (1.0 - fy);
                    let w10 = fx * (1.0 - fy);
                    let w01 = (1.0 - fx) * fy;
                    let w11 = fx * fy;
                    let r = (p00[0] as f32 * w00 + p10[0] as f32 * w10
                          + p01[0] as f32 * w01 + p11[0] as f32 * w11)
                          .round().clamp(0.0, 255.0) as u8;
                    let g = (p00[1] as f32 * w00 + p10[1] as f32 * w10
                          + p01[1] as f32 * w01 + p11[1] as f32 * w11)
                          .round().clamp(0.0, 255.0) as u8;
                    let b = (p00[2] as f32 * w00 + p10[2] as f32 * w10
                          + p01[2] as f32 * w01 + p11[2] as f32 * w11)
                          .round().clamp(0.0, 255.0) as u8;
                    aligned.put_pixel(dst_x, dst_y, image::Rgb([r, g, b]));
                } else if src_x >= 0.0 && src_y >= 0.0
                    && (src_x as u32) < iw && (src_y as u32) < ih
                {
                    // Fallback for edge pixels
                    aligned.put_pixel(dst_x, dst_y, *rgb_img.get_pixel(src_x as u32, src_y as u32));
                }
            }
        }
        aligned
    } else {
        // Fallback: simple bounding box crop (no alignment)
        let pw = ((face.x2 - face.x1) / 5).max(8);
        let ph = ((face.y2 - face.y1) / 5).max(8);
        let x1 = (face.x1 - pw).max(0) as u32;
        let y1 = (face.y1 - ph).max(0) as u32;
        let x2 = (face.x2 + pw).min(iw as i32) as u32;
        let y2 = (face.y2 + ph).min(ih as i32) as u32;
        img.crop_imm(x1, y1, x2 - x1, y2 - y1)
            .resize_exact(EMB_SIZE as u32, EMB_SIZE as u32, image::imageops::FilterType::Triangle)
            .to_rgb8()
    };

    let mut input = Array4::<f32>::zeros((1, 3, EMB_SIZE, EMB_SIZE));
    for y in 0..EMB_SIZE {
        for x in 0..EMB_SIZE {
            let p = crop.get_pixel(x as u32, y as u32);
            input[[0, 0, y, x]] = (p[0] as f32 - 127.5) / 128.0;
            input[[0, 1, y, x]] = (p[1] as f32 - 127.5) / 128.0;
            input[[0, 2, y, x]] = (p[2] as f32 - 127.5) / 128.0;
        }
    }

    let input_tensor = ort::value::Tensor::from_array(input)
        .map_err(|e| anyhow::anyhow!("emb tensor: {}", e))?;
    let mut emb: Vec<f32> = {
        let mut emb_session = models.embedder.lock().unwrap();
        let outputs = emb_session.run(ort::inputs!["input.1" => input_tensor])
            .map_err(|e| anyhow::anyhow!("emb run: {}", e))?;
        let (_, raw_data): (_, &[f32]) = outputs[0].try_extract_tensor()
            .map_err(|e| anyhow::anyhow!("emb extract: {}", e))?;
        raw_data.to_vec()
    };

    // Quality check: raw embedding norm indicates face quality (MagFace, CVPR 2021)
    // Low norm = bad crop (profile view, heavy shadow, motion blur, painting).
    // Typical good face: norm > 15. Bad/ambiguous: norm < ~9-10.
    // Gate lowered back to 9.0 because the previous 12.0 was rejecting valid
    // side-profile faces and under-exposed indoor shots, contributing to the
    // "0 faces from N photos" symptom. Centroid poisoning is still limited
    // because (a) DBSCAN already rejects noisy singletons, (b) the photo-
    // realism check in commands.rs catches most cartoons/prints.
    let raw_norm: f32 = emb.iter().map(|v| v * v).sum::<f32>().sqrt();
    if raw_norm < 9.0 {
        // Return empty embedding for low-quality faces — they'll be skipped
        return Ok(vec![]);
    }

    // L2-normalise
    if raw_norm > 1e-8 {
        emb.iter_mut().for_each(|v| *v /= raw_norm);
    }
    Ok(emb)
}

// ── Similarity & serialisation ────────────────────────────────────────────────

/// Cosine similarity with full normalization (safe for any vectors)
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() { return 0.0; }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a < 1e-8 || norm_b < 1e-8 { return 0.0; }
    (dot / (norm_a * norm_b)).clamp(-1.0, 1.0)
}

/// Fast dot product for L2-normalized vectors (skips sqrt — much faster in tight loops)
#[inline]
pub fn dot_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Mean of `embeddings`, L2-normalised. Used to summarise a list of faces
/// into one canonical vector — per-person centroid for known persons,
/// per-skip-cluster centroid for the "don't ask again" set.
/// Compute a robust centroid of L2-normalized embeddings with outlier rejection.
/// For N >= 4: compute mean, drop embeddings with cos-sim < (mean - 1.5σ), recompute.
/// This prevents a single bad face (bad lighting, partial occlusion, wrong identity
/// slipped in by user) from permanently poisoning the person centroid.
pub fn compute_centroid(embeddings: &[Vec<f32>]) -> Vec<f32> {
    if embeddings.is_empty() {
        return Vec::new();
    }
    if embeddings.len() == 1 {
        return embeddings[0].clone();
    }
    let dim = embeddings[0].len();

    // Pass 1: naive mean
    let mean_of = |idxs: &[usize]| -> Vec<f32> {
        let mut sum = vec![0.0f32; dim];
        let mut count = 0usize;
        for &i in idxs {
            let e = &embeddings[i];
            if e.len() != dim { continue; }
            for (s, v) in sum.iter_mut().zip(e.iter()) { *s += v; }
            count += 1;
        }
        if count == 0 { return Vec::new(); }
        let inv = 1.0 / count as f32;
        for s in sum.iter_mut() { *s *= inv; }
        let norm: f32 = sum.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 1e-8 {
            for s in sum.iter_mut() { *s /= norm; }
        }
        sum
    };

    let all_idxs: Vec<usize> = (0..embeddings.len()).collect();
    let c1 = mean_of(&all_idxs);
    if embeddings.len() < 4 {
        // Too few to estimate std reliably — skip outlier rejection.
        return c1;
    }

    // Compute cos sims to the naive centroid
    let sims: Vec<f32> = embeddings.iter()
        .map(|e| if e.len() == dim { cosine_similarity(e, &c1) } else { -1.0 })
        .collect();
    let n = sims.len() as f32;
    let mean_sim: f32 = sims.iter().sum::<f32>() / n;
    let var: f32 = sims.iter().map(|s| (s - mean_sim).powi(2)).sum::<f32>() / n;
    let std_sim = var.sqrt();
    let floor_sim = mean_sim - 1.5 * std_sim;

    let kept: Vec<usize> = (0..embeddings.len())
        .filter(|&i| sims[i] >= floor_sim)
        .collect();

    // If rejection leaves too few (< half), keep all — better to include than lose data.
    if kept.len() < (embeddings.len() / 2).max(2) {
        return c1;
    }
    mean_of(&kept)
}

pub fn embedding_to_bytes(emb: &[f32]) -> Vec<u8> {
    emb.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn bytes_to_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

// ── NMS ───────────────────────────────────────────────────────────────────────

fn iou(a: &DetectedFace, b: &DetectedFace) -> f32 {
    let x1 = a.x1.max(b.x1) as f32;
    let y1 = a.y1.max(b.y1) as f32;
    let x2 = a.x2.min(b.x2) as f32;
    let y2 = a.y2.min(b.y2) as f32;
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let area_a = (a.x2 - a.x1) as f32 * (a.y2 - a.y1) as f32;
    let area_b = (b.x2 - b.x1) as f32 * (b.y2 - b.y1) as f32;
    inter / (area_a + area_b - inter + 1e-6)
}

fn nms(mut faces: Vec<DetectedFace>, thresh: f32) -> Vec<DetectedFace> {
    faces.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let mut suppressed = vec![false; faces.len()];
    let mut keep = Vec::new();
    for i in 0..faces.len() {
        if suppressed[i] { continue; }
        keep.push(faces[i].clone());
        for j in (i + 1)..faces.len() {
            if !suppressed[j] && iou(&faces[i], &faces[j]) > thresh {
                suppressed[j] = true;
            }
        }
    }
    keep
}

// ── Face Clustering: DBSCAN (used by PhotoPrism, Immich) ─────────────────────
//
// DBSCAN is the production standard for face clustering:
// - Used by PhotoPrism (eps=0.64), Immich (max_dist=0.6)
// - Density-based: finds core points with enough neighbors, expands clusters
// - Handles noise: isolated faces get label -1 (not forced into wrong cluster)
// - No predefined cluster count needed
//
// For L2-normalized embeddings: euclidean_distance = sqrt(2 * (1 - cosine_similarity))
// PhotoPrism eps=0.64 ≈ cosine similarity >= 0.795
// We use eps=0.65 ≈ cosine similarity >= 0.789

/// DBSCAN epsilon: maximum Euclidean distance between neighbors (on L2-normalized vectors)
/// For normalized 512-dim embeddings: euclidean = sqrt(2 * (1 - cosine_sim))
/// eps=0.87 → cosine_sim >= ~0.62
/// Tested: 0.80 splits same person, 0.95 merges different people. 0.87 is the sweet spot.
const DBSCAN_EPS: f32 = 0.87;

/// Minimum number of faces to form a cluster core
/// PhotoPrism uses 4, but for small personal libraries 2 is better
const DBSCAN_MIN_SAMPLES: usize = 2;

/// Threshold for assigning noise points to nearest cluster (more lenient)
/// eps=1.0 → cosine_sim >= ~0.50
const DBSCAN_ASSIGN_EPS: f32 = 1.0;

pub struct FaceCluster {
    pub face_ids: Vec<i64>,
    pub centroid: Vec<f32>,
    pub representative: i64,
}

/// Euclidean distance between two L2-normalized vectors
fn euclidean_dist(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>().sqrt()
}

/// Maximum faces for full DBSCAN (O(n²)). Above this, use scalable 2-phase approach.
const DBSCAN_MAX_FULL: usize = 2000;

pub fn cluster_embeddings(faces: &[(i64, Vec<f32>)]) -> Vec<FaceCluster> {
    if faces.is_empty() { return vec![]; }

    let n = faces.len();

    // For large datasets: 2-phase approach
    // Phase 1: DBSCAN on a sample (first DBSCAN_MAX_FULL faces)
    // Phase 2: Assign remaining faces to nearest centroid (O(n*k))
    if n > DBSCAN_MAX_FULL {
        return cluster_large(faces);
    }

    // Full DBSCAN for smaller datasets
    let mut labels: Vec<i32> = vec![-1; n];
    let mut cluster_id: i32 = 0;

    // Precompute neighbor lists
    let mut neighbors: Vec<Vec<usize>> = vec![vec![]; n];
    for i in 0..n {
        for j in (i+1)..n {
            let dist = euclidean_dist(&faces[i].1, &faces[j].1);
            if dist <= DBSCAN_EPS {
                neighbors[i].push(j);
                neighbors[j].push(i);
            }
        }
    }

    // DBSCAN core loop
    for i in 0..n {
        if labels[i] != -1 { continue; } // already assigned

        // Check if core point (enough neighbors)
        if neighbors[i].len() < DBSCAN_MIN_SAMPLES { continue; } // not core → stays noise for now

        // Start new cluster — expand from this core point
        labels[i] = cluster_id;
        let mut queue: Vec<usize> = neighbors[i].clone();
        let mut qi = 0;
        while qi < queue.len() {
            let j = queue[qi];
            qi += 1;
            if labels[j] == cluster_id { continue; } // already in this cluster
            labels[j] = cluster_id; // add to cluster (even if was noise)

            // If j is also a core point, expand its neighbors too
            if neighbors[j].len() >= DBSCAN_MIN_SAMPLES {
                for &nb in &neighbors[j] {
                    if labels[nb] == -1 {
                        queue.push(nb);
                    }
                }
            }
        }
        cluster_id += 1;
    }

    // Post-processing: assign noise points (-1) to nearest cluster centroid
    // (more lenient threshold — these are likely real faces that were just slightly outside eps)
    if cluster_id > 0 {
        // Compute cluster centroids
        let mut centroids: Vec<Vec<f32>> = vec![vec![0.0; faces[0].1.len()]; cluster_id as usize];
        let mut counts: Vec<usize> = vec![0; cluster_id as usize];
        for i in 0..n {
            if labels[i] >= 0 {
                let cid = labels[i] as usize;
                counts[cid] += 1;
                for (c, v) in centroids[cid].iter_mut().zip(&faces[i].1) { *c += v; }
            }
        }
        for cid in 0..cluster_id as usize {
            if counts[cid] > 0 {
                let cnt = counts[cid] as f32;
                let norm: f32 = centroids[cid].iter().map(|v| (v/cnt)*(v/cnt)).sum::<f32>().sqrt();
                if norm > 1e-8 {
                    centroids[cid].iter_mut().for_each(|v| *v = *v / cnt / norm);
                }
            }
        }

        // Assign noise to nearest cluster if within ASSIGN_EPS
        for i in 0..n {
            if labels[i] != -1 { continue; }
            let mut best_dist = f32::MAX;
            let mut best_cid: i32 = -1;
            for cid in 0..cluster_id as usize {
                let dist = euclidean_dist(&faces[i].1, &centroids[cid]);
                if dist < best_dist { best_dist = dist; best_cid = cid as i32; }
            }
            if best_dist <= DBSCAN_ASSIGN_EPS && best_cid >= 0 {
                labels[i] = best_cid;
            }
        }
    }

    // Build clusters from labels
    let mut cluster_map: std::collections::HashMap<i32, Vec<usize>> = std::collections::HashMap::new();
    for i in 0..n {
        if labels[i] >= 0 {
            cluster_map.entry(labels[i]).or_default().push(i);
        } else {
            // Remaining noise: each becomes its own single-face cluster
            cluster_map.entry(-(i as i32) - 2).or_default().push(i);
        }
    }

    let mut clusters: Vec<FaceCluster> = Vec::new();
    for (_label, members) in cluster_map.iter() {
        let face_ids: Vec<i64> = members.iter().map(|i| faces[*i].0).collect();

        // Compute centroid
        let dim = faces[0].1.len();
        let mut centroid = vec![0.0f32; dim];
        for i in members.iter() {
            for (c, v) in centroid.iter_mut().zip(&faces[*i].1) { *c += v; }
        }
        let nm = members.len() as f32;
        let norm: f32 = centroid.iter().map(|v| (v/nm)*(v/nm)).sum::<f32>().sqrt();
        if norm > 1e-8 {
            centroid.iter_mut().for_each(|v| *v = *v / nm / norm);
        }

        // Pick representative (closest to centroid)
        let mut best_rep = members[0];
        let mut best_sim = -1.0f32;
        for i in members.iter() {
            let sim = cosine_similarity(&faces[*i].1, &centroid);
            if sim > best_sim { best_sim = sim; best_rep = *i; }
        }

        clusters.push(FaceCluster {
            face_ids,
            centroid,
            representative: faces[best_rep].0,
        });
    }

    // NOTE: Cluster-merge pass REMOVED (was root cause of cross-identity wrong-tagging).
    // The iterative centroid-merge at ~1.2*eps would bridge two different people through
    // a single noisy "half-profile" face, then grow iteratively, permanently merging
    // identities. If DBSCAN over-splits, we accept that — the user reviews unknowns.
    // See: immich/photoprism do NOT post-merge DBSCAN clusters for this exact reason.

    // Sort by size descending
    clusters.sort_by(|a, b| b.face_ids.len().cmp(&a.face_ids.len()));
    clusters
}

/// Scalable clustering for large libraries (>2000 faces).
/// Phase 1: DBSCAN on first DBSCAN_MAX_FULL faces to discover cluster centroids.
/// Phase 2: Assign remaining faces to nearest centroid in O(n*k).
/// Total: O(DBSCAN_MAX_FULL² + n*k) instead of O(n²).
fn cluster_large(faces: &[(i64, Vec<f32>)]) -> Vec<FaceCluster> {
    let n = faces.len();
    eprintln!("[face] Large dataset ({} faces) — using 2-phase clustering", n);

    // Phase 1: Full DBSCAN on sample
    let sample_size = DBSCAN_MAX_FULL.min(n);
    let sample = &faces[..sample_size];
    let mut seed_clusters = cluster_embeddings(sample); // recursive call, sample < DBSCAN_MAX_FULL

    if seed_clusters.is_empty() {
        // No clusters found in sample — treat each face as its own cluster
        return faces.iter().map(|(fid, emb)| FaceCluster {
            face_ids: vec![*fid],
            centroid: emb.clone(),
            representative: *fid,
        }).collect();
    }

    // Phase 2: Assign remaining faces to nearest centroid
    for i in sample_size..n {
        let (fid, emb) = &faces[i];
        let mut best_dist = f32::MAX;
        let mut best_ci = 0usize;

        for (ci, c) in seed_clusters.iter().enumerate() {
            let dist = euclidean_dist(emb, &c.centroid);
            if dist < best_dist { best_dist = dist; best_ci = ci; }
        }

        if best_dist <= DBSCAN_ASSIGN_EPS {
            // Assign to existing cluster + update centroid incrementally
            let c = &mut seed_clusters[best_ci];
            c.face_ids.push(*fid);
            let nn = c.face_ids.len() as f32;
            for (cv, v) in c.centroid.iter_mut().zip(emb.iter()) {
                *cv = (*cv * (nn - 1.0) + v) / nn;
            }
            let norm: f32 = c.centroid.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 1e-8 { c.centroid.iter_mut().for_each(|v| *v /= norm); }
        } else {
            // New cluster for this face (too far from all existing)
            seed_clusters.push(FaceCluster {
                face_ids: vec![*fid],
                centroid: emb.clone(),
                representative: *fid,
            });
        }
    }

    // Update representatives for modified clusters
    for c in &mut seed_clusters {
        if c.face_ids.len() > 1 {
            // Pick a face closest to centroid from the faces we have access to
            let mut best_sim = -1.0f32;
            let mut best_fid = c.face_ids[0];
            for fid in &c.face_ids {
                if let Some((_, emb)) = faces.iter().find(|(id, _)| id == fid) {
                    let sim = cosine_similarity(emb, &c.centroid);
                    if sim > best_sim { best_sim = sim; best_fid = *fid; }
                }
            }
            c.representative = best_fid;
        }
    }

    seed_clusters.sort_by(|a, b| b.face_ids.len().cmp(&a.face_ids.len()));
    seed_clusters
}
