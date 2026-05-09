// Image quality analysis — blur detection and "keeper" scoring.
//
// Blur detection uses **Laplacian variance** (OpenCV's standard technique):
//   1. Convert image to grayscale
//   2. Apply 3×3 Laplacian kernel [[0,1,0],[1,-4,1],[0,1,0]]
//   3. Take the variance of the result
//
// Sharp image → strong edges → high variance. Blurry → smooth → low variance.
//
// ⚠️ CRITICAL safety consideration:
//   Naive Laplacian variance will mis-flag intentionally-shallow-DoF photos
//   (portraits with bokeh, macro, telephoto) as "blurry" because most of the
//   frame is soft. We fight this by also measuring blur in the CENTER 40%
//   of the image. If the center is sharp but the whole frame is soft, it's
//   almost certainly bokeh — NOT a throw-away blurry shot.
//
// Typical ranges on a 256×256 grayscale thumbnail (u8 0-255):
//   - Crystal sharp photo:    2000 – 8000
//   - Normal sharp photo:      500 – 2000
//   - Slight blur / soft:      100 – 500
//   - Clearly blurry:           20 – 100
//   - Severely blurry:           0 – 20
//
// Default "too blurry" threshold exposed to UI: 100.

use anyhow::Result;
use image::DynamicImage;

/// Detailed blur score: overall Laplacian variance + center-ROI variance
/// + per-patch maximum.
///
/// Why three numbers instead of one: a single global variance mis-flags
/// every photo with legitimate soft regions (bokeh portraits, foggy
/// landscapes, night-time phone shots with dark sky). The only truly
/// unusable photos are the ones where *no region anywhere* has usable
/// detail — so we also scan a 4×4 grid of patches and track the sharpest
/// one. The caller picks an `effective` score as max of the three, which
/// means "sharpest evidence of detail anywhere in the frame".
pub struct BlurScoreDetail {
    /// Laplacian variance over the whole 256×256 grayscale thumbnail.
    pub overall: f32,
    /// Laplacian variance computed ONLY over the center 40% ROI.
    pub center: f32,
    /// center / overall ratio — >1.5 suggests bokeh (center much sharper).
    pub center_ratio: f32,
    /// Maximum Laplacian variance across a 4×4 grid of 64×64 patches.
    /// Captures "the sharpest region of the photo" — a foggy scene with
    /// one visible tree, or a night shot with visible street lights, will
    /// have a high patch_max even if `overall` is low.
    pub patch_max: f32,
}

/// Compute the simple Laplacian-variance blur score (backwards-compatible).
/// Higher = sharper. Use `compute_blur_score_detailed` for bokeh safety.
pub fn compute_blur_score(img: &DynamicImage) -> f32 {
    compute_blur_score_detailed(img).overall
}

/// Compute detailed blur metrics: overall + center-weighted variance.
///
/// The image is first downscaled to 256×256 for consistent, fast scoring
/// (otherwise two copies of the same photo at different resolutions would
/// get very different scores).
pub fn compute_blur_score_detailed(img: &DynamicImage) -> BlurScoreDetail {
    // Resize to a fixed 256×256 for scale-invariant scoring
    let small = img.resize_exact(256, 256, image::imageops::FilterType::Triangle);
    let gray = small.to_luma8();
    let (w, h) = (gray.width() as i32, gray.height() as i32);
    let buf = gray.as_raw();

    // Center ROI: 40% square centered in the frame.
    // For 256×256 that's x,y ∈ [77, 179) — a 102×102 window.
    let margin_x = (w as f32 * 0.30) as i32;
    let margin_y = (h as f32 * 0.30) as i32;
    let cx_lo = margin_x;
    let cx_hi = w - margin_x;
    let cy_lo = margin_y;
    let cy_hi = h - margin_y;

    // 4×4 grid of 64×64 patches (covers the full 256×256). For each patch
    // we accumulate sum + sum-of-squares of the Laplacian so we can compute
    // its variance at the end and keep the max. A single sharp subject
    // against a soft background (bokeh, fog, night sky) still produces
    // at least one patch with high variance.
    const GRID: usize = 4;
    const PATCH: i32 = 256 / GRID as i32; // 64
    let mut patch_sum = [[0.0f64; GRID]; GRID];
    let mut patch_sq = [[0.0f64; GRID]; GRID];
    let mut patch_n  = [[0u64; GRID]; GRID];

    // Apply 3x3 Laplacian kernel.
    //   [ 0  1  0 ]
    //   [ 1 -4  1 ]
    //   [ 0  1  0 ]
    let mut o_sum = 0.0f64;
    let mut o_sq = 0.0f64;
    let mut o_n = 0u64;
    let mut c_sum = 0.0f64;
    let mut c_sq = 0.0f64;
    let mut c_n = 0u64;

    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            let c_ = buf[(y * w + x) as usize] as i32;
            let u = buf[((y - 1) * w + x) as usize] as i32;
            let d = buf[((y + 1) * w + x) as usize] as i32;
            let l = buf[(y * w + x - 1) as usize] as i32;
            let r = buf[(y * w + x + 1) as usize] as i32;
            let lap = (u + d + l + r - 4 * c_) as f64;
            let lap2 = lap * lap;

            o_sum += lap;
            o_sq += lap2;
            o_n += 1;

            if x >= cx_lo && x < cx_hi && y >= cy_lo && y < cy_hi {
                c_sum += lap;
                c_sq += lap2;
                c_n += 1;
            }

            // Patch-grid bucketing. Integer division locks each pixel into
            // exactly one patch cell.
            let px = (x / PATCH).min(GRID as i32 - 1) as usize;
            let py = (y / PATCH).min(GRID as i32 - 1) as usize;
            patch_sum[py][px] += lap;
            patch_sq[py][px] += lap2;
            patch_n[py][px] += 1;
        }
    }

    let var = |sum: f64, sq: f64, n: u64| -> f32 {
        if n == 0 { return 0.0; }
        let nf = n as f64;
        let mean = sum / nf;
        let v = (sq / nf) - (mean * mean);
        v.max(0.0) as f32
    };
    let overall = var(o_sum, o_sq, o_n);
    let center = var(c_sum, c_sq, c_n);
    let center_ratio = if overall > 1.0 { center / overall } else { 0.0 };

    // Find the sharpest patch in the grid.
    let mut patch_max = 0.0f32;
    for py in 0..GRID {
        for px in 0..GRID {
            let v = var(patch_sum[py][px], patch_sq[py][px], patch_n[py][px]);
            if v > patch_max { patch_max = v; }
        }
    }

    BlurScoreDetail { overall, center, center_ratio, patch_max }
}

/// "Sharpest evidence of detail anywhere in the frame." This is what we
/// actually want to threshold on — a photo is unusable only when *no region*
/// has detail. Bokeh (center sharp), fog (one object in focus), night shots
/// (highlights) all have at least one sharp patch and survive this score.
pub fn effective_sharpness(detail: &BlurScoreDetail) -> f32 {
    detail.overall.max(detail.center).max(detail.patch_max)
}

/// Convenience: load a thumbnail JPEG from disk and score it.
pub fn score_thumbnail_file(thumb_path: &std::path::Path) -> Result<f32> {
    let img = image::open(thumb_path)?;
    Ok(compute_blur_score(&img))
}

/// Convenience: detailed scoring from a thumbnail JPEG on disk.
pub fn score_thumbnail_file_detailed(thumb_path: &std::path::Path) -> Result<BlurScoreDetail> {
    let img = image::open(thumb_path)?;
    Ok(compute_blur_score_detailed(&img))
}

/// Heuristic: a photo is "likely bokeh" (intentional shallow DoF) if the
/// center ROI is markedly sharper than the overall frame AND the center is
/// at least modestly sharp in absolute terms.
///
/// Thresholds tuned conservatively — we'd rather let a genuinely blurry
/// photo slip through than kill a deliberately-shallow portrait.
pub fn is_likely_bokeh(detail: &BlurScoreDetail) -> bool {
    // Center must be at least somewhat sharp, AND center should be
    // meaningfully sharper than the overall frame.
    detail.center >= 150.0 && detail.center_ratio >= 1.5
}

/// Signals passed to `keeper_score`. Collecting them in a struct keeps the
/// function signature readable as we add more criteria over time.
#[derive(Debug, Clone, Default)]
pub struct KeeperSignals {
    pub width: i32,
    pub height: i32,
    pub size_bytes: i64,
    pub blur: Option<f32>,
    /// -1 = rejected, 0 = unrated, 1-5 = stars
    pub rating: i32,
    pub favorite: bool,
    /// Number of user tags assigned to this photo.
    pub tag_count: i64,
    /// Number of faces in this photo that are assigned to a named person.
    pub person_count: i64,
    /// Number of collections this photo belongs to.
    pub collection_count: i64,
    /// True if the photo has an XMP sidecar written (user touched metadata).
    pub has_xmp: bool,
    /// Format-priority bonus derived from filename extension. Higher = prefer
    /// to keep. RAW/HEIC/AVIF carry more original information than JPEG, so
    /// they outrank re-encoded JPEG copies even when the JPEG is larger
    /// (see Immich issue #15013: keeper selection wrong when HEIC smaller
    /// but higher quality than JPEG).
    pub format_priority: i32,
}

/// Derive `format_priority` from a filename or path. Higher = prefer to keep.
///
/// Scoring rationale:
///   * RAW formats (.cr2 .cr3 .nef .arw .dng .orf .rw2 .raf) carry the
///     original sensor data — always the master copy.
///   * HEIC/HEIF/AVIF are modern high-quality encodings; a HEIC original
///     beats a re-exported JPEG even when smaller in bytes.
///   * PNG is penalized slightly — screenshots / graphics edits rather
///     than photos.
///   * BMP / TIFF without compression are penalized as unlikely originals
///     from a camera pipeline (usually intermediate dumps).
pub fn format_priority_from_filename(name: &str) -> i32 {
    let lower = name.to_ascii_lowercase();
    let ext = std::path::Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        // RAW originals — highest priority
        "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "raf"
        | "pef" | "srw" | "x3f" | "3fr" => 800,
        // Modern high-quality encodings
        "heic" | "heif" | "avif" => 500,
        // Standard camera output — neutral baseline
        "jpg" | "jpeg" | "webp" => 0,
        // Graphics / screenshots — slight penalty
        "png" => -50,
        // Uncompressed dumps — stronger penalty
        "bmp" | "tif" | "tiff" => -200,
        _ => 0,
    }
}

impl KeeperSignals {
    /// Has the user INVESTED any effort in this photo? (tags/person/collection/xmp)
    /// Invested photos get strong protection — they should rarely be auto-deleted.
    pub fn is_invested(&self) -> bool {
        self.tag_count > 0 || self.person_count > 0 || self.collection_count > 0 || self.has_xmp
    }
}

/// Weighted keeper score. Higher = more likely to be the "original" the user
/// wants to keep. Lowest photos in a duplicate group get auto-marked for
/// deletion.
///
/// Tuning philosophy:
///   - Hard protection for favorites and invested photos (user cares)
///   - Rejected photos (rating = -1) are heavily penalized (user wants them gone)
///   - Resolution and sharpness dominate among neutrally-rated photos
///   - File size is a weak tiebreaker (bigger original usually better)
///
/// Weights (ascending impact):
///   - mb × 3               small tiebreak (big file usually better-quality)
///   - tag_count × 30       user effort bump per tag
///   - blur × 1.5           sharpness matters
///   - mp × 150             resolution matters a lot
///   - format_priority × 1  RAW +800 / HEIC +500 / JPEG 0 / PNG −50 / BMP −200
///   - collection × 500     in-a-collection is a clear user signal
///   - person × 800         linked to a named person — strong signal
///   - rating × 2_000       per-star boost
///   - xmp  +3_000          user hand-edited metadata
///   - invested  +20_000    any invested photo is never auto-demoted below
///                          a pristine untouched copy of the same scene
///   - favorite  +100_000   near-veto: favorites almost never get deleted
///   - rejected (rating<0) −50_000  flip side of favorite
pub fn keeper_score(sig: &KeeperSignals) -> f64 {
    let mp = (sig.width.max(0) as f64 * sig.height.max(0) as f64) / 1_000_000.0;
    let mb = sig.size_bytes.max(0) as f64 / (1024.0 * 1024.0);
    let b = sig.blur.unwrap_or(0.0).max(0.0) as f64;
    let rating_boost = if sig.rating < 0 { -50_000.0 } else { sig.rating as f64 * 2_000.0 };
    let fav_boost = if sig.favorite { 100_000.0 } else { 0.0 };
    let xmp_boost = if sig.has_xmp { 3_000.0 } else { 0.0 };
    let invested_boost = if sig.is_invested() { 20_000.0 } else { 0.0 };

    fav_boost
        + invested_boost
        + xmp_boost
        + rating_boost
        + (sig.person_count as f64) * 800.0
        + (sig.collection_count as f64) * 500.0
        + (sig.format_priority as f64)
        + mp * 150.0
        + b * 1.5
        + (sig.tag_count as f64) * 30.0
        + mb * 3.0
}

/// Explain why a photo is the keeper — human-readable reasoning shown in
/// the UI tooltip. Produces short bullet-style reasons (max ~6) ordered
/// by importance.
pub fn explain_keeper(sig: &KeeperSignals) -> Vec<String> {
    let mut reasons = Vec::new();
    if sig.favorite {
        reasons.push("❤ favorite".to_string());
    }
    if sig.rating > 0 {
        reasons.push(format!("{}★ rating", sig.rating));
    } else if sig.rating < 0 {
        reasons.push("rejected".to_string());
    }
    if sig.is_invested() {
        let mut parts = Vec::new();
        if sig.tag_count > 0 {
            parts.push(format!("{} tag{}", sig.tag_count, if sig.tag_count == 1 {""} else {"s"}));
        }
        if sig.person_count > 0 {
            parts.push(format!("{} person", sig.person_count));
        }
        if sig.collection_count > 0 {
            parts.push(format!("in {} collection{}", sig.collection_count, if sig.collection_count == 1 {""} else {"s"}));
        }
        if sig.has_xmp {
            parts.push("XMP edited".to_string());
        }
        if !parts.is_empty() {
            reasons.push(parts.join(", "));
        }
    }
    if sig.format_priority >= 800 {
        reasons.push("RAW original".to_string());
    } else if sig.format_priority >= 500 {
        reasons.push("HEIC/AVIF".to_string());
    } else if sig.format_priority <= -100 {
        reasons.push("uncompressed".to_string());
    }
    let mp = (sig.width.max(0) as f64 * sig.height.max(0) as f64) / 1_000_000.0;
    if mp > 0.0 {
        reasons.push(format!("{:.1} MP", mp));
    }
    if let Some(b) = sig.blur {
        if b >= 800.0 { reasons.push(format!("sharp ({:.0})", b)); }
        else if b >= 200.0 { reasons.push(format!("normal ({:.0})", b)); }
        else { reasons.push(format!("soft ({:.0})", b)); }
    }
    let mb = sig.size_bytes.max(0) as f64 / (1024.0 * 1024.0);
    if mb >= 0.1 {
        reasons.push(format!("{:.1} MB", mb));
    }
    reasons
}
