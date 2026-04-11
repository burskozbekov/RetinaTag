use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::HashSet,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tauri::Emitter;

use crate::{db, scanner};

/// Manages file system watchers for auto-scanning folders
pub struct FolderWatcher {
    _watcher: RecommendedWatcher,
}

impl FolderWatcher {
    pub fn new(
        folders: Vec<String>,
        db_conn: Arc<Mutex<rusqlite::Connection>>,
        thumbnails_dir: std::path::PathBuf,
        app_handle: tauri::AppHandle,
    ) -> anyhow::Result<Self> {
        let pending_files: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let pending_clone = pending_files.clone();
        let db_clone = db_conn.clone();
        let thumbs_clone = thumbnails_dir.clone();
        let ah_clone = app_handle.clone();

        // Debounce: process new files every 5 seconds
        let debounce_running = Arc::new(AtomicBool::new(false));
        let debounce_running2 = debounce_running.clone();

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) => {
                        for path in &event.paths {
                            if scanner::is_image_file(path) {
                                if let Some(s) = path.to_str() {
                                    let mut pending = pending_clone.lock().unwrap();
                                    pending.insert(s.to_string());
                                }
                            }
                        }

                        // Start debounce processor if not already running
                        if !debounce_running2.swap(true, Ordering::SeqCst) {
                            let pending_ref = pending_files.clone();
                            let db_ref = db_clone.clone();
                            let thumbs_ref = thumbs_clone.clone();
                            let ah_ref = ah_clone.clone();
                            let running_ref = debounce_running2.clone();

                            std::thread::spawn(move || {
                                // Keep looping until the pending set stays empty after a full
                                // debounce window — this closes the race where files arrive
                                // between the drain and the running=false store.
                                loop {
                                    std::thread::sleep(Duration::from_secs(5));

                                    let files: Vec<String> = {
                                        let mut pending = pending_ref.lock().unwrap_or_else(|e| e.into_inner());
                                        pending.drain().collect()
                                    };

                                    if files.is_empty() {
                                        // Nothing arrived during the sleep window — safe to exit.
                                        running_ref.store(false, Ordering::SeqCst);
                                        break;
                                    }

                                    process_new_files(files, &db_ref, &thumbs_ref, &ah_ref);
                                    // Loop: sleep again to catch any files that arrived while
                                    // process_new_files was running.
                                }
                            });
                        }
                    }
                    _ => {}
                }
            }
        })?;

        for folder in &folders {
            let path = Path::new(folder);
            if path.exists() {
                watcher.watch(path, RecursiveMode::Recursive)?;
            }
        }

        Ok(FolderWatcher { _watcher: watcher })
    }
}

fn process_new_files(
    files: Vec<String>,
    db_conn: &Arc<Mutex<rusqlite::Connection>>,
    thumbnails_dir: &std::path::Path,
    app_handle: &tauri::AppHandle,
) {
    let mut new_count = 0;

    for file_path in &files {
        let path = std::path::Path::new(file_path);
        if !path.exists() {
            continue;
        }

        let hash = match scanner::compute_hash(file_path) {
            Ok(h) => h,
            Err(_) => continue,
        };

        let conn = match db_conn.lock() {
            Ok(c) => c,
            Err(_) => continue,
        };

        if db::photo_exists_by_hash(&conn, &hash).unwrap_or(true) {
            continue;
        }

        let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let folder = path.parent().unwrap_or(path).to_string_lossy().to_string();

        let (width, height) = {
            let fp = file_path.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let result = image::image_dimensions(&fp)
                    .map(|(w, h)| (Some(w as i32), Some(h as i32)))
                    .unwrap_or((None, None));
                let _ = tx.send(result);
            });
            rx.recv_timeout(std::time::Duration::from_secs(10))
                .unwrap_or((None, None))
        };

        let size = std::fs::metadata(file_path).map(|m| m.len() as i64).unwrap_or(0);

        let new_photo = db::NewPhoto {
            path: file_path,
            filename: &filename,
            folder: &folder,
            hash: &hash,
            size,
            width,
            height,
        };

        if let Ok(photo_id) = db::insert_photo(&conn, &new_photo) {
            if photo_id > 0 {
                if let Ok(_) = crate::thumbnail::get_or_create_thumbnail(file_path, &hash, thumbnails_dir, 200) {
                    let cache_name = format!("{}.jpg", &hash[..hash.len().min(24)]);
                    let thumb_path = thumbnails_dir.join(&cache_name);
                    db::update_thumbnail_path(&conn, photo_id, &thumb_path.to_string_lossy()).ok();
                }
                new_count += 1;
            }
        }
    }

    if new_count > 0 {
        app_handle
            .emit("watch-new-files", serde_json::json!({
                "count": new_count,
                "files": files.len(),
            }))
            .ok();
    }
}
