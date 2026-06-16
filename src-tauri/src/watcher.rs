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

                                    // v1.5.280 — Auto-tag removed entirely.
                                    //
                                    // The watcher used to kick off `run_tagging` whenever new
                                    // files landed in a folder marked `auto_tag=1`. That
                                    // surprised the user repeatedly: opening the app caused
                                    // background tagging they didn't ask for, burning API
                                    // credits / local-model time without consent. Tagging
                                    // must always be an explicit "Start tagging" click.
                                    //
                                    // The auto_tag column + UI toggle still exist for now
                                    // (no DB migration headaches), but nothing reads them —
                                    // they're effectively dead settings.
                                    let _ = (new_count, &auto_tag_ref, &tag_running_ref, &tag_stop_ref);
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
/// v1.5.388 — English month folder name (matches the library + MTP layout).
fn month_name_en(m: u32) -> &'static str {
    match m {
        1 => "January", 2 => "February", 3 => "March", 4 => "April",
        5 => "May", 6 => "June", 7 => "July", 8 => "August",
        9 => "September", 10 => "October", 11 => "November", 12 => "December",
        _ => "Unknown",
    }
}

/// v1.5.388 — Oldest valid (year, month) across EXIF DateTimeOriginal, file
/// modified, and file created (sane range 1995..=today). Mirrors commands.rs
/// date_bucket_for_file so a file dropped into the library lands in the same
/// YEAR\MM bucket the library/import would choose. None if nothing usable.
fn oldest_ym(path: &str) -> Option<(i32, u32)> {
    // v1.5.447 — CRITICAL date-corruption fix. This used to add the file's
    // modified/created timestamps as date candidates, so a file with no real
    // EXIF/embedded date got bucketed by its mtime. When AI tagging embeds an
    // XMP packet the file's bytes + mtime change to "now", so the next watch
    // event re-bucketed thousands of already-correct photos into the CURRENT
    // month folder (e.g. 2026\06-June) and re-dated them to today. Now we use
    // the canonical scanner::extract_date_taken (EXIF / embedded XMP / path
    // pattern only — NO mtime, per v1.5.266/415). None => the caller leaves the
    // file in place and never moves or re-dates it.
    let dt = crate::scanner::extract_date_taken(path)?;
    let s = dt.trim();
    let year: i32 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    Some((year, month))
}

/// v1.5.388 — Library root = parent-of-parent of any canonical
/// `<root>\YYYY\MM-Month` photo folder in the DB. None if the library isn't
/// organized that way yet (then the watcher imports in place — old behaviour).
fn derive_library_root(conn: &rusqlite::Connection) -> Option<String> {
    let folder: String = conn
        .query_row(
            "SELECT folder FROM photos WHERE folder GLOB '*\\[12][0-9][0-9][0-9]\\[0-9][0-9]-*' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok()?;
    let p = std::path::Path::new(&folder);
    Some(p.parent()?.parent()?.to_string_lossy().to_string())
}

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

    // v1.5.388 — derive the library root once (cheap, one query) so new
    // arrivals can be auto-filed into <root>\YYYY\MM-Month — the same buckets
    // the library + phone import use. None = library not organized that way
    // yet → we skip the move and import in place (old behaviour).
    let lib_root: Option<String> = db_conn.lock().ok().and_then(|c| derive_library_root(&c));

    for file_path in files {
        let path = std::path::Path::new(file_path);
        if !path.exists() {
            continue;
        }

        // v1.5.281 — Don't import files that live inside a `thumbnails`
        // directory.  These are almost certainly RetinaTag's own cached
        // 256×256 thumbnails (or some other app's cache); importing them
        // as photos is what caused the v1.5.278 mass-pollution incident.
        let in_thumb_dir = path.components().any(|c| {
            c.as_os_str().to_string_lossy().eq_ignore_ascii_case("thumbnails")
        });
        if in_thumb_dir {
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

        // v1.5.388 — Auto-file this fresh arrival into <root>\YYYY\MM-Month by
        // its OLDEST date, so a photo downloaded / dropped straight into the
        // library lands in the right year-month folder. Only genuinely-new
        // files reach here (MTP imports are already in the DB → skipped above),
        // so this never fights the import placement. No lock is held across the
        // rename (no freeze); the moved file's own watch event dedups against
        // the row we insert below, so there's no re-processing loop. Falls back
        // to in-place if the root is unknown, the file is already in its bucket,
        // or the rename fails.
        let mut work_path: String = file_path.clone();
        if let (Some(root), Some((y, m))) = (lib_root.as_deref(), oldest_ym(file_path)) {
            let tgt_dir = format!("{}\\{}\\{:02}-{}", root.trim_end_matches('\\'), y, m, month_name_en(m));
            let cur_parent = std::path::Path::new(&work_path)
                .parent()
                .map(|p| p.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if cur_parent != tgt_dir.to_lowercase() && std::fs::create_dir_all(&tgt_dir).is_ok() {
                let base = std::path::Path::new(&work_path)
                    .file_name().unwrap_or_default().to_string_lossy().to_string();
                let mut dest = format!("{}\\{}", tgt_dir, base);
                if std::path::Path::new(&dest).exists() {
                    let stem = std::path::Path::new(&base)
                        .file_stem().unwrap_or_default().to_string_lossy().to_string();
                    let ext = std::path::Path::new(&base)
                        .extension()
                        .map(|e| format!(".{}", e.to_string_lossy()))
                        .unwrap_or_default();
                    let mut i = 2;
                    loop {
                        let cand = format!("{}\\{}_{}{}", tgt_dir, stem, i, ext);
                        if !std::path::Path::new(&cand).exists() { dest = cand; break; }
                        i += 1;
                    }
                }
                if std::fs::rename(&work_path, &dest).is_ok() {
                    work_path = dest;
                }
            }
        }
        let path = std::path::Path::new(&work_path);

        let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let folder = path.parent().unwrap_or(path).to_string_lossy().to_string();

        // SLOW I/O — no lock held. image_dimensions decodes the image
        // header (~30 ms typical, 500 ms+ for HEIC); EXIF reads a few KB
        // off disk; video duration may spawn ffprobe.
        let (width, height) = {
            let fp = work_path.clone();
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

        let size = std::fs::metadata(&work_path).map(|m| m.len() as i64).unwrap_or(0);

        let mtype = crate::scanner::media_type_for_path(path);
        // v1.5.447 — use the canonical date reader (EXIF / embedded XMP / path
        // pattern, NO file-mtime fallback). A file with no real capture date gets
        // a NULL date_taken — it must NEVER be stamped with the file's mtime,
        // which becomes "today" the moment AI tagging rewrites the file's XMP.
        // That mtime fallback is what re-dated thousands of photos to today.
        let date_taken = crate::scanner::extract_date_taken(&work_path);
        let duration_secs = if mtype == "video" {
            crate::scanner::extract_video_duration_pub(&work_path)
        } else {
            None
        };

        // Thumbnail also slow — generate before re-locking.
        let thumb_path = crate::thumbnail::get_or_create_thumbnail(
            &work_path, &hash, thumbnails_dir, 256,
        )
        .ok()
        .map(|_| {
            let cache_name = crate::thumbnail::thumb_cache_name(&hash);
            thumbnails_dir.join(&cache_name)
        });

        let new_photo = db::NewPhoto {
            path: &work_path,
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
