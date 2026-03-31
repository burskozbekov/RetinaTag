use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::path::Path;

/// Returns base64-encoded JPEG thumbnail. Reads from cache file if it exists,
/// otherwise resizes the source image, saves to cache, and returns the base64.
pub fn get_or_create_thumbnail(
    photo_path: &str,
    photo_hash: &str,
    thumbnails_dir: &Path,
    size: u32,
) -> Result<String> {
    let cache_name = format!("{}.jpg", &photo_hash[..photo_hash.len().min(24)]);
    let cache_path = thumbnails_dir.join(&cache_name);

    if cache_path.exists() {
        let data = std::fs::read(&cache_path).context("read cached thumbnail")?;
        return Ok(STANDARD.encode(&data));
    }

    let img = image::open(photo_path).context("open image for thumbnail")?;
    let thumb = img.resize_to_fill(size, size, image::imageops::FilterType::Lanczos3);

    let mut bytes: Vec<u8> = Vec::new();
    thumb
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Jpeg,
        )
        .context("encode thumbnail jpeg")?;

    std::fs::write(&cache_path, &bytes).context("save thumbnail cache")?;
    Ok(STANDARD.encode(&bytes))
}

/// Resize image to max `max_px` on longest side and return base64 JPEG.
/// Used before sending to AI APIs to reduce costs and VRAM usage.
pub fn prepare_for_api(photo_path: &str) -> Result<String> {
    prepare_for_api_sized(photo_path, 512)
}

/// Prepare a smaller image specifically for local Ollama inference.
/// 384px is enough for tagging and uses significantly less VRAM.
pub fn prepare_for_api_local(photo_path: &str) -> Result<String> {
    prepare_for_api_sized(photo_path, 384)
}

fn prepare_for_api_sized(photo_path: &str, max_px: u32) -> Result<String> {
    let img = image::open(photo_path).context("open image for API")?;
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
