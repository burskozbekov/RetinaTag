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
        auto_tag_folders: HashSet<String>,
        tag_running: Arc<AtomicBool>,
        tag_stop: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        let pending_files: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let pending_clone = pending_files.clone();
        let db_clone = db_conn.clone();
        let thumbs_clone = thumbnails_dir.clone();
        let ah_clone = app_handle.clone();
        let auto_tag_clone = Arc::new(auto_tag_folders);

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
                                    // v1.5.75 — poison-tolerant lock. Was
                                    // `.unwrap()` which crashed the watcher
                                    // callback (and silently killed
                                    // watch-folders for the session) if any
                                    // sibling thread had panicked while
                                    // holding this mutex. into_inner just
                                    // takes the data anyway — duplicates in
                                    // the pending set are harmless.
                                    let mut pending = pending_clone
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner());
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
                            let auto_tag_ref = auto_tag_clone.clone();
                            let tag_running_ref = tag_running.clone();
                            let tag_stop_ref = tag_stop.clone();

                            std::thread::spawn(move || {
                                loop {
                                    std::thread::sleep(Duration::from_secs(5));

                                    let files: Vec<String> = {
                                        let mut pending = pending_ref.lock().unwrap_or_else(|e| e.into_inner());
                                        pending.drain().collect()
                                    };

                                    if files.is_empty() {
                                        running_ref.store(false, Ordering::SeqCst);
                                        break;
                                    }

                                    let new_count = process_new_files(&files, &db_ref, &thumbs_ref, &ah_ref);

                                    // Keep the "Last check" timestamp in the
                                    // Watch Folders UI fresh. The watcher
                                    // does see activity here — skipping the
                                    // touch would leave the UI lying about
                                    // when we last looked. Dedupe parents so
                                    // we only issue one UPDATE per folder.
                                    {
                                        let parents: HashSet<String> = files
                                            .iter()
                                            .filter_map(|f| {
                                                std::path::Path::new(f)
                                                    .parent()
                                                    .map(|p| p.to_string_lossy().into_owned())
                                            })
                                            .collect();
                                        if !parents.is_empty() {
                                            if let Ok(conn) = db_ref.lock() {
                                                for p in &parents {
                                                    let _ = db::update_watch_folder_checked_by_path(&conn, p);
                                                }
                                            }
                                        }
                                    }

                                    // Auto-tag: if any new file is in an auto_tag folder, trigger tagging
                                    if new_count > 0 {
                                        let should_auto_tag = files.iter().any(|f| {
                                            auto_tag_ref.iter().any(|folder| {
                                                let norm_f = f.replace('\\', "/");
                                                let norm_folder = folder.replace('\\', "/");
                                                norm_f.starts_with(&norm_folder)
                                            })
                                        });

                                        if should_auto_tag && !tag_running_ref.swap(true, Ordering::SeqCst) {
                                            let db_tag = db_ref.clone();
                                            let stop_tag = tag_stop_ref.clone();
                                            let ah_tag = ah_ref.clone();
                                            let running_tag = tag_running_ref.clone();

                                            ah_ref.emit("auto-tag-started", serde_json::json!({
                                                "count": new_count,
                                                "source": "watch-folder"
                                            })).ok();

                                            tauri::async_runtime::spawn(async move {
                                                stop_tag.store(false, Ordering::SeqCst);
                                                let result = crate::tagger::run_tagging(
                                                    db_tag, stop_tag, ah_tag.clone(),
                                                ).await;
                                                running_tag.store(false, Ordering::SeqCst);
                                                ah_tag.emit("tag-complete", result).ok();
                                            });
                                        }
                                    }
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

/// Process new files — returns count of successfully imported photos
fn process_new_files(
    files: &[String],
    db_conn: &Arc<Mutex<rusqlite::Connection>>,
    thumbnails_dir: &std::path::Path,
    app_handle: &tauri::AppHandle,
) -> usize {
    // v1.5.74 — Was a P1 freeze: this loop used to acquire `db_conn.lock()`
    // once at the top of each iteration and hold it through image_dimensions,
    // EXIF read, video duration (ffprobe subprocess), and thumbnail
    // generation — easily 1-3 seconds per file. Every Tauri command that
    // touched the DB blocked for the duration of one watched-file import.
    // Now we only hold the lock around the actual DB queries, and run all
    // the slow I/O outside the critical section.
    let mut new_count = 0;

    for file_path in files {
        let path = std::path::Path::new(file_path);
        if !path.exists() {
            continue;
        }

        let hash = match scanner::compute_hash(file_path) {
            Ok(h) => h,
            Err(_) => continue,
        };

        // Short lock — dedup check only.
        let already_in_library = match db_conn.lock() {
            Ok(c) => db::photo_exists_by_hash(&c, &hash).unwrap_or(true),
            Err(_) => continue,
        };
        if already_in_library {
            continue;
        }

        let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let folder = path.parent().unwrap_or(path).to_string_lossy().to_string();

        // SLOW I/O — no lock held. image_dimensions decodes the image
        // header (~30 ms typical, 500 ms+ for HEIC); EXIF reads a few KB
        // off disk; video duration may spawn ffprobe.
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

        let mtype = crate::scanner::media_type_for_path(std::path::Path::new(file_path));
        let date_taken = crate::exif_reader::read_exif(file_path)
            .ok().and_then(|e| e.date_taken)
            .or_else(|| {
                std::fs::metadata(file_path).ok().and_then(|m| {
                    m.created().or_else(|_| m.modified()).ok().map(|t| {
                        let dt: chrono::DateTime<chrono::Local> = t.into();
                        dt.format("%Y-%m-%d %H:%M:%S").to_string()
                    })
                })
            });
        let duration_secs = if mtype == "video" {
            crate::scanner::extract_video_duration_pub(file_path)
        } else {
            None
        };

        // Thumbnail also slow — generate before re-locking.
        let thumb_path = crate::thumbnail::get_or_create_thumbnail(
            file_path, &hash, thumbnails_dir, 256,
        )
        .ok()
        .map(|_| {
            let cache_name = crate::thumbnail::thumb_cache_name(&hash);
            thumbnails_dir.join(&cache_name)
        });

        let new_photo = db::NewPhoto {
            path: file_path,
            filename: &filename,
            folder: &folder,
            hash: &hash,
            size,
            width,
            height,
            media_type: mtype,
            date_taken,
            duration_secs,
        };

        // Short lock — insert + thumb-path update.
        if let Ok(conn) = db_conn.lock() {
            if let Ok(photo_id) = db::insert_photo(&conn, &new_photo) {
                if photo_id > 0 {
                    if let Some(tp) = thumb_path {
                        db::update_thumbnail_path(&conn, photo_id, &tp.to_string_lossy()).ok();
                    }
                    new_count += 1;
                }
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

    new_count
}
