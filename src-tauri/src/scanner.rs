use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tauri::Emitter;
use walkdir::WalkDir;

use crate::{db, models::*, thumbnail};

const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "tiff", "tif", "heic", "heif", "avif",
];

pub fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn compute_hash(path: &str) -> Result<String> {
    let data = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(hex::encode(hasher.finalize()))
}

pub async fn scan_folder_impl(
    folder: String,
    db_conn: Arc<Mutex<rusqlite::Connection>>,
    thumbnails_dir: std::path::PathBuf,
    stop_flag: Arc<AtomicBool>,
    app_handle: tauri::AppHandle,
) -> Result<ScanComplete> {
    let folder_clone = folder.clone();

    // Collect all image paths first
    let all_paths: Vec<std::path::PathBuf> = tokio::task::spawn_blocking(move || {
        WalkDir::new(&folder_clone)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file() && is_image_file(e.path()))
            .map(|e| e.path().to_path_buf())
            .collect()
    })
    .await?;

    let total = all_paths.len();
    let mut new_files = 0usize;
    let mut skipped = 0usize;

    for (i, path) in all_paths.iter().enumerate() {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        let path_str = path.to_string_lossy().to_string();
        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let folder_str = path
            .parent()
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        // Emit progress periodically
        if i % 20 == 0 || i == total.saturating_sub(1) {
            app_handle
                .emit(
                    "scan-progress",
                    ScanProgress {
                        total,
                        scanned: i + 1,
                        new_files,
                        skipped,
                        current_file: filename.clone(),
                        is_running: true,
                    },
                )
                .ok();
        }

        let path_str_clone = path_str.clone();
        let folder_str_clone = folder_str.clone();
        let filename_clone = filename.clone();
        let thumbs_dir = thumbnails_dir.clone();
        let db_arc = db_conn.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<bool> {
            let hash = compute_hash(&path_str_clone)?;

            let conn = db_arc.lock().map_err(|_| anyhow::anyhow!("db lock"))?;

            if db::photo_exists_by_hash(&conn, &hash)? {
                return Ok(false);
            }

            let (width, height) = image::image_dimensions(&path_str_clone)
                .map(|(w, h)| (Some(w as i32), Some(h as i32)))
                .unwrap_or((None, None));

            let meta = std::fs::metadata(&path_str_clone)?;
            let size = meta.len() as i64;

            let new_photo = db::NewPhoto {
                path: &path_str_clone,
                filename: &filename_clone,
                folder: &folder_str_clone,
                hash: &hash,
                size,
                width,
                height,
            };

            let photo_id = db::insert_photo(&conn, &new_photo)?;

            if photo_id > 0 {
                if thumbnail::get_or_create_thumbnail(
                    &path_str_clone,
                    &hash,
                    &thumbs_dir,
                    200,
                )
                .is_ok()
                {
                    let cache_name = format!("{}.jpg", &hash[..hash.len().min(24)]);
                    let thumb_path = thumbs_dir.join(&cache_name);
                    db::update_thumbnail_path(
                        &conn,
                        photo_id,
                        &thumb_path.to_string_lossy(),
                    )
                    .ok();
                }
            }

            Ok(true)
        })
        .await;

        match result {
            Ok(Ok(true)) => new_files += 1,
            Ok(Ok(false)) => skipped += 1,
            _ => {}
        }
    }

    // Final event
    app_handle
        .emit(
            "scan-progress",
            ScanProgress {
                total,
                scanned: total,
                new_files,
                skipped,
                current_file: String::new(),
                is_running: false,
            },
        )
        .ok();

    Ok(ScanComplete {
        new_files,
        skipped,
        total,
        folder,
    })
}
