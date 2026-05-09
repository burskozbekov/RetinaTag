use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use image::{codecs::jpeg::JpegEncoder, DynamicImage};
use std::path::Path;

/// JPEG quality for cached thumbnails. 80 is visually indistinguishable from
/// the image-crate default of 90 at 200-256 px but ~30% smaller on disk —
/// less IPC, less base64 overhead, faster grid loads.
const THUMB_JPEG_QUALITY: u8 = 80;

/// Derive the cache filename for a given photo hash. Handles both legacy
/// SHA-256 hex (pure 64-char hex) and the current `xxh3:<128-bit-hex>` form,
/// stripping the `xxh3:` prefix before slicing — colons are illegal in
/// Windows filenames, so leaving the prefix in would break everything.
/// Single source of truth to keep scanner/watcher/commands in sync.
pub fn thumb_cache_name(photo_hash: &str) -> String {
    let stripped = photo_hash.split(':').next_back().unwrap_or(photo_hash);
    format!("{}.jpg", &stripped[..stripped.len().min(24)])
}

use crate::scanner::{RAW_EXTENSIONS, VIDEO_EXTENSIONS};

/// Try to open an image/RAW/video. Applies EXIF orientation for images.
pub fn open_image(photo_path: &str) -> Result<DynamicImage> {
    let ext = Path::new(photo_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Route by format
    let img = if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        extract_video_frame(photo_path)?
    } else if RAW_EXTENSIONS.contains(&ext.as_str()) {
        // RAW: try Windows WIC codec first, then image::open as fallback for DNG
        convert_raw_to_image(photo_path)
            .or_else(|_| image::open(photo_path).map_err(|e| anyhow::anyhow!("{}", e)))?
    } else if ext == "heic" || ext == "heif" {
        // HEIC/HEIF: always use WPF which applies HEIF rotation internally.
        // Do NOT try image::open — it can't decode HEIC.
        convert_heic_to_image(photo_path)?
    } else {
        image::open(photo_path)?
    };

    // Apply EXIF orientation — but NOT for video (ffmpeg handles it)
    // and NOT for HEIC/HEIF (WPF already applied HEIF rotation, applying
    // EXIF orientation again would cause DOUBLE ROTATION).
    let skip_orientation = VIDEO_EXTENSIONS.contains(&ext.as_str())
        || ext == "heic" || ext == "heif";
    if !skip_orientation {
        let orientation = read_exif_orientation(photo_path).unwrap_or(1);
        Ok(apply_orientation(img, orientation))
    } else {
        Ok(img)
    }
}

/// Read the EXIF Orientation tag (1–8). Returns 1 (normal) on any failure.
/// Public variant so other modules (commands) can inspect orientation.
pub fn get_exif_orientation(path: &str) -> u32 {
    read_exif_orientation(path).unwrap_or(1)
}

fn read_exif_orientation(path: &str) -> Option<u32> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    let field = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    field.value.get_uint(0)
}

/// Apply EXIF orientation transform.
fn apply_orientation(img: DynamicImage, orientation: u32) -> DynamicImage {
    match orientation {
        1 => img,
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.fliph().rotate270(),
        6 => img.rotate90(),
        7 => img.fliph().rotate90(),
        8 => img.rotate270(),
        _ => img,
    }
}

// ── RAW / HEIC conversion via Windows WIC / WPF codecs ───────────────────────
//
// Both RAW and HEIC use the same PowerShell → BitmapDecoder pathway; only the
// codec behind the scenes differs (Raw Image Extension vs HEIF Image Extension
// from the Microsoft Store). Consolidated into one helper that encodes the
// converted frame to an in-memory temp file, reads it back, and unlinks — no
// per-call code duplication and a single place to tune.

#[cfg(target_os = "windows")]
fn wpf_decode_to_jpeg(photo_path: &str, label: &str) -> Result<DynamicImage> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    // Use a per-call unique temp name so concurrent decode tasks (rayon in
    // scanner, background tagger) don't clobber each other's files.
    let temp_dir = std::env::temp_dir();
    let temp_jpg = temp_dir.join(format!(
        "retinatag_{}_{}_{}.jpg",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let temp_jpg_str = temp_jpg.to_string_lossy().to_string();

    let ps_script = format!(
        r#"Add-Type -AssemblyName PresentationCore;
$src = New-Object System.Uri('file:///{}');
$dec = [System.Windows.Media.Imaging.BitmapDecoder]::Create($src, [System.Windows.Media.Imaging.BitmapCreateOptions]::PreservePixelFormat, [System.Windows.Media.Imaging.BitmapCacheOption]::OnLoad);
$frame = $dec.Frames[0];
$enc = New-Object System.Windows.Media.Imaging.JpegBitmapEncoder;
$enc.QualityLevel = 88;
$enc.Frames.Add([System.Windows.Media.Imaging.BitmapFrame]::Create($frame));
$fs = [System.IO.File]::Create('{}');
$enc.Save($fs);
$fs.Close();"#,
        photo_path.replace('\\', "/"),
        temp_jpg_str.replace('\\', "/")
    );

    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps_script])
        .stderr(Stdio::null()) // prevents the child from blocking if stderr fills
        .creation_flags(0x08000000)
        .output()
        .context("Failed to run PowerShell for codec decode")?;

    if !output.status.success() {
        std::fs::remove_file(&temp_jpg).ok();
        return Err(anyhow::anyhow!("{} decode failed (exit {:?})", label, output.status.code()));
    }

    let img = image::open(&temp_jpg).with_context(|| format!("open converted {} JPEG", label))?;
    std::fs::remove_file(&temp_jpg).ok();
    Ok(img)
}

#[cfg(target_os = "windows")]
fn convert_raw_to_image(photo_path: &str) -> Result<DynamicImage> {
    wpf_decode_to_jpeg(photo_path, "raw")
}
#[cfg(target_os = "windows")]
fn convert_heic_to_image(photo_path: &str) -> Result<DynamicImage> {
    wpf_decode_to_jpeg(photo_path, "heic")
}

#[cfg(not(target_os = "windows"))]
fn convert_raw_to_image(_photo_path: &str) -> Result<DynamicImage> {
    Err(anyhow::anyhow!("RAW conversion not supported on this platform"))
}
#[cfg(not(target_os = "windows"))]
fn convert_heic_to_image(_photo_path: &str) -> Result<DynamicImage> {
    Err(anyhow::anyhow!("HEIC conversion not supported on this platform"))
}

// ── Video frame extraction via ffmpeg ────────────────────────────────────────

/// Run ffmpeg with the given args, capture stdout as bytes. Swallow stderr
/// (progress noise / "non-monotonic dts" warnings would fill the parent's
/// buffer and block the child on Windows).
fn run_ffmpeg_capture(args: &[&str]) -> Result<Vec<u8>> {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "windows")]
    use std::os::windows::process::CommandExt;

    let mut cmd = Command::new("ffmpeg");
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(0x08000000);
    }
    let out = cmd.output().context("ffmpeg spawn failed — is it on PATH?")?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(anyhow::anyhow!("ffmpeg returned no frame"));
    }
    Ok(out.stdout)
}

/// Extract one frame from a video and decode it. Uses a single ffmpeg call
/// that seeks to ~1s, pipes an MJPEG frame to stdout, and skips the
/// round-trip through a temp file. Avoids the separate ffprobe call by
/// picking a fixed safe seek (most videos are > 2s; a fallback at 0s handles
/// shorter clips).
fn extract_video_frame(video_path: &str) -> Result<DynamicImage> {
    // `-ss` BEFORE `-i` is the "fast seek": ffmpeg jumps to the nearest
    // keyframe without decoding everything before it. Much faster on long
    // videos. `-frames:v 1 -f mjpeg pipe:1` is the in-memory equivalent of
    // "write one JPEG to stdout".
    let primary = run_ffmpeg_capture(&[
        "-loglevel", "error",
        "-ss", "1",
        "-i", video_path,
        "-frames:v", "1",
        "-q:v", "3",
        "-f", "mjpeg",
        "pipe:1",
    ]);
    let bytes = match primary {
        Ok(b) => b,
        Err(_) => {
            // Fallback for very short videos: seek from 0.
            run_ffmpeg_capture(&[
                "-loglevel", "error",
                "-i", video_path,
                "-frames:v", "1",
                "-q:v", "3",
                "-f", "mjpeg",
                "pipe:1",
            ])?
        }
    };
    image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
        .context("decode video frame from memory")
}

// ── Thumbnail generation ─────────────────────────────────────────────────────
//
// Note on JPEG DCT-scaled decode: skipped for now. `image` 0.25 wraps
// zune-jpeg internally but doesn't surface the scale API, and pulling zune
// 0.4 in directly doesn't buy us DCT scale either (moved behind a private
// flag in that release). Not worth a third JPEG backend for a 20-30% win
// on one format when the rayon-parallelized scanner is already giving
// multi-x wins. Worth revisiting when image 0.26 lands with its promised
// ergonomic scale support, or if we pick up mozjpeg for other reasons.

/// Returns base64-encoded JPEG thumbnail. Reads from cache or generates new.
pub fn get_or_create_thumbnail(
    photo_path: &str,
    photo_hash: &str,
    thumbnails_dir: &Path,
    size: u32,
) -> Result<String> {
    let cache_name = thumb_cache_name(photo_hash);
    let cache_path = thumbnails_dir.join(&cache_name);

    if cache_path.exists() {
        let data = std::fs::read(&cache_path).context("read cached thumbnail")?;
        return Ok(STANDARD.encode(&data));
    }

    let img = open_image(photo_path).context("open image for thumbnail")?;
    // Triangle (bilinear) is visually indistinguishable from Lanczos3 at
    // 200–256 px thumbnail sizes but 3–5× faster. Lanczos only pays off at
    // export-quality resolutions.
    let thumb = img.resize_to_fill(size, size, image::imageops::FilterType::Triangle);

    let mut bytes: Vec<u8> = Vec::new();
    let rgb = thumb.to_rgb8();
    JpegEncoder::new_with_quality(&mut bytes, THUMB_JPEG_QUALITY)
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .context("encode thumbnail jpeg")?;

    std::fs::write(&cache_path, &bytes).context("save thumbnail cache")?;
    Ok(STANDARD.encode(&bytes))
}

/// Resize image to max `max_px` on longest side and return base64 JPEG.
pub fn prepare_for_api(photo_path: &str) -> Result<String> {
    prepare_for_api_sized(photo_path, 512)
}

/// Prepare a smaller image specifically for local Ollama inference.
pub fn prepare_for_api_local(photo_path: &str) -> Result<String> {
    prepare_for_api_sized(photo_path, 384)
}

fn prepare_for_api_sized(photo_path: &str, max_px: u32) -> Result<String> {
    let img = open_image(photo_path).context("open image for API")?;
    let thumb = img.resize(max_px, max_px, image::imageops::FilterType::Triangle);

    let mut bytes: Vec<u8> = Vec::new();
    thumb
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Jpeg,
        )
        .context("encode API image jpeg")?;

    Ok(STANDARD.encode(&bytes))
}

// ── Dominant Color Extraction ───────────────────────────────────────────────

/// Extract `k` dominant colors from an image using simple k-means clustering.
/// Returns hex color strings sorted by cluster size (most dominant first).
///
/// Perf notes:
///   - Downsample to 48×48 (2.3k pixels) before clustering; visually identical
///     to 64×64 on the resulting palette but 1.8× less work per iteration.
///   - 4 iterations instead of 15 — k-means with evenly-spaced init converges
///     in 2–3 iters on natural images, 4 is a safe cap.
///   - Assignment step runs on rayon: each pixel → nearest centroid is
///     embarrassingly parallel.
pub fn extract_dominant_colors(img: &DynamicImage, k: usize) -> Vec<String> {
    use rayon::prelude::*;

    let small = img.resize_exact(48, 48, image::imageops::FilterType::Nearest);
    let rgb = small.to_rgb8();
    let pixels: Vec<[f32; 3]> = rgb.pixels()
        .map(|p| [p[0] as f32, p[1] as f32, p[2] as f32])
        .collect();

    if pixels.is_empty() || k == 0 {
        return vec![];
    }

    let step = pixels.len().max(1) / k.max(1);
    let mut centroids: Vec<[f32; 3]> = (0..k)
        .map(|i| pixels[(i * step).min(pixels.len() - 1)])
        .collect();

    let mut assignments = vec![0usize; pixels.len()];

    for _ in 0..4 {
        // Parallel nearest-centroid assignment.
        assignments
            .par_iter_mut()
            .zip(pixels.par_iter())
            .for_each(|(slot, px)| {
                let mut best = 0;
                let mut best_dist = f32::MAX;
                for (j, c) in centroids.iter().enumerate() {
                    let d = (px[0]-c[0]).powi(2) + (px[1]-c[1]).powi(2) + (px[2]-c[2]).powi(2);
                    if d < best_dist { best_dist = d; best = j; }
                }
                *slot = best;
            });

        // Centroid update — sequential is fine, k is tiny.
        let mut sums = vec![[0f32; 3]; k];
        let mut counts = vec![0usize; k];
        for (i, px) in pixels.iter().enumerate() {
            let c = assignments[i];
            sums[c][0] += px[0]; sums[c][1] += px[1]; sums[c][2] += px[2];
            counts[c] += 1;
        }
        for j in 0..k {
            if counts[j] > 0 {
                centroids[j] = [
                    sums[j][0] / counts[j] as f32,
                    sums[j][1] / counts[j] as f32,
                    sums[j][2] / counts[j] as f32,
                ];
            }
        }
    }

    let mut cluster_sizes: Vec<(usize, usize)> = (0..k).map(|i| (i, 0)).collect();
    for &a in &assignments { cluster_sizes[a].1 += 1; }
    cluster_sizes.sort_by(|a, b| b.1.cmp(&a.1));

    cluster_sizes.iter()
        .filter(|(_, count)| *count > 0)
        .map(|(idx, _)| {
            let c = &centroids[*idx];
            format!("#{:02X}{:02X}{:02X}", c[0] as u8, c[1] as u8, c[2] as u8)
        })
        .collect()
}
