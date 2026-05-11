use anyhow::Result;
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
    "tga", "ico", "pnm", "pbm", "pgm", "ppm", "dds", "qoi",
];

pub const RAW_EXTENSIONS: &[&str] = &[
    "cr2", "cr3", "nef", "arw", "dng", "srf", "sr2", "orf", "rw2", "raf", "pef", "raw",
];

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "avi", "mkv", "wmv", "flv", "webm", "m4v", "3gp", "mts", "m2ts",
];

pub fn is_media_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_lowercase();
            IMAGE_EXTENSIONS.contains(&lower.as_str())
                || RAW_EXTENSIONS.contains(&lower.as_str())
                || VIDEO_EXTENSIONS.contains(&lower.as_str())
        })
        .unwrap_or(false)
}

/// Keep backward compat for code using old name
pub fn is_image_file(path: &Path) -> bool {
    is_media_file(path)
}

pub fn media_type_for_path(path: &Path) -> &'static str {
    let ext = path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        "video"
    } else if RAW_EXTENSIONS.contains(&ext.as_str()) {
        "raw"
    } else {
        "image"
    }
}

pub fn compute_hash(path: &str) -> Result<String> {
    use std::io::Read;
    use xxhash_rust::xxh3::Xxh3;
    // Chunked reading — safe for large RAW files (50MB+). Uses a 256 KiB
    // buffer: xxh3 is so fast that the hash step is usually IO-bound, so
    // bigger buffers = fewer syscalls = measurable wins on SSD/NVMe.
    let file = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("Failed to open file '{}': {}", path, e))?;
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
    let mut hasher = Xxh3::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    // 128-bit digest in hex. Prefixed so we can distinguish from legacy
    // SHA-256 rows in the DB and never mistakenly collide them.
    Ok(format!("xxh3:{:032x}", hasher.digest128()))
}

/// Public wrapper for watcher.rs to use without duplication.
pub fn extract_video_duration_pub(path: &str) -> Option<i32> {
    extract_video_duration(path)
}

/// Extract video duration in seconds using ffprobe.
fn extract_video_duration(path: &str) -> Option<i32> {
    #[cfg(target_os = "windows")]
    use std::os::windows::process::CommandExt;

    let output = {
        #[cfg(target_os = "windows")]
        {
            std::process::Command::new("ffprobe")
                .args(["-v", "quiet", "-show_entries", "format=duration", "-of", "csv=p=0", path])
                .creation_flags(0x08000000)
                .output()
                .ok()?
        }
        #[cfg(not(target_os = "windows"))]
        {
            std::process::Command::new("ffprobe")
                .args(["-v", "quiet", "-show_entries", "format=duration", "-of", "csv=p=0", path])
                .output()
                .ok()?
        }
    };

    let s = String::from_utf8_lossy(&output.stdout);
    s.trim().parse::<f64>().ok().map(|d| d as i32)
}

/// v1.5.48 — Public so the detail panel (`get_photo_exif`) can override
/// EXIF DateTimeOriginal with the same path-aware oldest-date logic the
/// scanner uses.
pub fn best_date_taken(path: &str) -> Option<String> {
    extract_date_taken(path)
}

/// v1.5.46 — Extract the OLDEST plausible "date taken" for a photo.
///
/// Background: the previous version trusted EXIF `DateTimeOriginal` and fell
/// back to file creation/modification only when EXIF was missing entirely.
/// On a re-saved photo (eg. an image edited in 2022 from a 2010 original)
/// `DateTimeOriginal` reflects the SAVE date, not the date the photo was
/// taken — so the user saw "2022" for a photo they knew was from 2010.
///
/// New behaviour: gather every date candidate we can find, drop anything
/// implausible (before 1990 or in the future), and return the oldest. The
/// candidates are:
///   • EXIF `DateTimeOriginal`, `DateTimeDigitized`, `DateTime`
///   • EXIF `GPSDateStamp`
///   • The filesystem `mtime` and `birthtime`
///   • A `YYYY-MM-DD`, `YYYYMMDD`, or `YYYY` pattern in the filename
/// Returned format matches the SQLite text date format used elsewhere
/// (`YYYY-MM-DD HH:MM:SS`).
fn extract_date_taken(path: &str) -> Option<String> {
    use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};

    let mut candidates: Vec<NaiveDateTime> = Vec::with_capacity(8);

    // ── EXIF date tags ──────────────────────────────────────────────────
    // The stock `read_exif` only returns DateTimeOriginal; for the oldest-
    // date heuristic we open the EXIF directly and pull every dated tag.
    // v1.5.47 — try multiple formats; some cameras emit dashes or
    // DD/MM/YYYY, not the EXIF-spec colon form.
    const FORMATS: &[&str] = &[
        "%Y:%m:%d %H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y/%m/%d %H:%M:%S",
        "%d/%m/%Y %H:%M:%S",
        "%m/%d/%Y %H:%M:%S",
        "%Y:%m:%d %H:%M",
        "%Y-%m-%d %H:%M",
        "%d/%m/%Y %H:%M",
        "%m/%d/%Y %H:%M",
    ];
    if let Ok(file) = std::fs::File::open(path) {
        let mut reader = std::io::BufReader::new(file);
        if let Ok(exif) = exif::Reader::new().read_from_container(&mut reader) {
            for tag in [
                exif::Tag::DateTimeOriginal,
                exif::Tag::DateTimeDigitized,
                exif::Tag::DateTime,
            ] {
                if let Some(field) = exif.get_field(tag, exif::In::PRIMARY) {
                    let s = field.display_value().to_string();
                    let s = s.trim_matches('"').trim().to_string();
                    if let Some(dt) = FORMATS.iter().find_map(|f| NaiveDateTime::parse_from_str(&s, f).ok()) {
                        candidates.push(dt);
                    }
                }
            }
            // GPS date stamp ("YYYY:MM:DD") — combine with optional time
            // stamp; without a time we fall back to noon UTC so it doesn't
            // skew toward midnight on day boundaries.
            if let Some(field) = exif.get_field(exif::Tag::GPSDateStamp, exif::In::PRIMARY) {
                let s = field.display_value().to_string();
                let s = s.trim_matches('"').trim().to_string();
                if let Ok(d) = chrono::NaiveDate::parse_from_str(&s, "%Y:%m:%d") {
                    candidates.push(d.and_hms_opt(12, 0, 0).unwrap());
                }
            }
        }
    }

    // ── Filesystem metadata ─────────────────────────────────────────────
    if let Ok(meta) = std::fs::metadata(path) {
        if let Ok(t) = meta.modified() {
            let dt: DateTime<Local> = t.into();
            candidates.push(dt.naive_local());
        }
        if let Ok(t) = meta.created() {
            let dt: DateTime<Local> = t.into();
            candidates.push(dt.naive_local());
        }
    }

    // ── Path + filename pattern ─────────────────────────────────────────
    // Look for YYYY-MM-DD / YYYY_MM_DD / YYYYMMDD tokens anywhere in the
    // FULL PATH (not just the filename stem). Many users organise photos
    // into folder hierarchies like `D:\photos\2003\2003_12\2003_12_14\`
    // — the folder names carry the original date even when the EXIF was
    // stripped or rewritten by a re-save. Manual sliding-window search
    // instead of regex (the project doesn't pull the regex crate and
    // adding it for one parse would bloat the binary). Only used as a
    // candidate; it still has to win the "oldest plausible" race below.
    // v1.5.48 — Was scanning only the filename stem; now scans the whole
    // path string so folder-name dates contribute too. Also tracks the
    // best (most specific + earliest) match across the path so a photo
    // in `\2003\2003_12_14\` picks 2003-12-14 instead of just 2003.
    {
        let bytes = path.as_bytes();
        let mut i = 0;
        while i + 8 <= bytes.len() {
            // Need 4 ascii digits to start a year.
            let is_digit = |b: u8| b.is_ascii_digit();
            if !(is_digit(bytes[i]) && is_digit(bytes[i+1]) && is_digit(bytes[i+2]) && is_digit(bytes[i+3])) {
                i += 1;
                continue;
            }
            let y: i32 = std::str::from_utf8(&bytes[i..i+4]).unwrap_or("0").parse().unwrap_or(0);
            if !(1990..=2099).contains(&y) { i += 1; continue; }
            // Three forms supported:
            //   YYYYMMDD      (8 digits in a row)
            //   YYYY-MM-DD    (with - or _ or : as separators)
            //   YYYY_MM_DD    (same family, different sep)
            let try_compact = i + 8 <= bytes.len()
                && is_digit(bytes[i+4]) && is_digit(bytes[i+5])
                && is_digit(bytes[i+6]) && is_digit(bytes[i+7]);
            let sep_ok = |b: u8| b == b'-' || b == b'_' || b == b':' || b == b'.';
            let try_separated = i + 10 <= bytes.len()
                && sep_ok(bytes[i+4])
                && is_digit(bytes[i+5]) && is_digit(bytes[i+6])
                && sep_ok(bytes[i+7])
                && is_digit(bytes[i+8]) && is_digit(bytes[i+9]);
            let mm: u32; let dd: u32; let consumed: usize;
            if try_separated {
                mm = std::str::from_utf8(&bytes[i+5..i+7]).unwrap_or("0").parse().unwrap_or(0);
                dd = std::str::from_utf8(&bytes[i+8..i+10]).unwrap_or("0").parse().unwrap_or(0);
                consumed = 10;
            } else if try_compact {
                mm = std::str::from_utf8(&bytes[i+4..i+6]).unwrap_or("0").parse().unwrap_or(0);
                dd = std::str::from_utf8(&bytes[i+6..i+8]).unwrap_or("0").parse().unwrap_or(0);
                consumed = 8;
            } else {
                i += 1;
                continue;
            }
            if (1..=12).contains(&mm) && (1..=31).contains(&dd) {
                if let Some(date) = chrono::NaiveDate::from_ymd_opt(y, mm, dd) {
                    candidates.push(date.and_hms_opt(12, 0, 0).unwrap());
                    // Don't break — keep scanning. A path like
                    // `\backup-2024-01-01\photos-from-2003\2003_12_14\foo.jpg`
                    // should pick 2003-12-14 (oldest), not the first hit
                    // 2024-01-01. The "oldest plausible" picker below
                    // chooses the right one.
                }
            }
            i += consumed;
        }
    }

    // ── Pick the oldest plausible candidate ─────────────────────────────
    let now = Utc::now().naive_utc();
    let earliest_plausible = chrono::NaiveDate::from_ymd_opt(1990, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();

    candidates.retain(|d| *d >= earliest_plausible && *d <= now);
    candidates.sort();
    let chosen = candidates.first()?;

    // Re-anchor to local-tz formatting so the DB uses the same shape it
    // always has. We treat the naive date as local time (the EXIF spec is
    // ambiguous about timezone; treating it as local matches what cameras
    // print) and format identically to the previous code path.
    let dt_local = Local.from_local_datetime(chosen).single()?;
    Some(dt_local.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// Intermediate record flowing worker → writer. Holds everything the writer
/// needs to insert one photo row and link its thumbnail, without the writer
/// needing to touch disk itself.
struct PendingInsert {
    path: String,
    filename: String,
    folder: String,
    hash: String,
    size: i64,
    width: Option<i32>,
    height: Option<i32>,
    media_type: String,
    date_taken: Option<String>,
    duration_secs: Option<i32>,
    thumb_path: Option<String>,
}

pub async fn scan_folder_impl(
    folder: String,
    db_conn: Arc<Mutex<rusqlite::Connection>>,
    thumbnails_dir: std::path::PathBuf,
    stop_flag: Arc<AtomicBool>,
    app_handle: tauri::AppHandle,
) -> Result<ScanComplete> {
    let folder_clone = folder.clone();

    // Collect all media paths (images + RAW + video)
    let all_paths: Vec<std::path::PathBuf> = tokio::task::spawn_blocking(move || {
        WalkDir::new(&folder_clone)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file() && is_media_file(e.path()))
            .map(|e| e.path().to_path_buf())
            .collect()
    })
    .await?;

    // Drop into a blocking task so rayon's thread pool + the writer thread
    // don't run inside a tokio worker — they'd starve the runtime otherwise.
    let folder_out = folder.clone();
    tokio::task::spawn_blocking(move || {
        scan_folder_parallel(all_paths, folder_out, db_conn, thumbnails_dir, stop_flag, app_handle)
    })
    .await?
}

/// Parallel scan pipeline:
///   1. Pre-load all existing hashes into a `HashSet` once (a single SQL query
///      that scales O(n) memory but replaces N per-file `SELECT` round-trips
///      through the DB mutex — the big win on large libraries).
///   2. rayon `par_iter` workers hash each file, skip if already known, and
///      otherwise compute dimensions/metadata/thumbnail in parallel. The
///      expensive stuff (SHA, JPEG decode, ffmpeg) fans out across CPU cores.
///   3. Workers send completed records down an MPSC channel to a single
///      writer thread that batches 64 inserts per SQLite transaction. This
///      amortizes per-statement fsync cost (~2 ms each → ~0.05 ms amortized).
fn scan_folder_parallel(
    all_paths: Vec<std::path::PathBuf>,
    folder: String,
    db_conn: Arc<Mutex<rusqlite::Connection>>,
    thumbnails_dir: std::path::PathBuf,
    stop_flag: Arc<AtomicBool>,
    app_handle: tauri::AppHandle,
) -> Result<ScanComplete> {
    use rayon::prelude::*;
    use std::sync::atomic::AtomicUsize;

    let total = all_paths.len();

    // Pre-load known hashes — one query instead of N round-trips through the
    // DB mutex. Written as an explicit while-loop (rather than a collecting
    // iterator chain) so the borrow checker has no trouble proving that
    // `rows` drops before `stmt` drops before `conn`.
    let existing_hashes: std::collections::HashSet<String> = {
        let conn = db_conn.lock().map_err(|_| anyhow::anyhow!("db lock"))?;
        let mut stmt = conn.prepare("SELECT hash FROM photos WHERE hash IS NOT NULL")?;
        let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if let Ok(h) = row.get::<_, String>(0) { set.insert(h); }
        }
        set
    };

    // Emit a "scan-resumed" event if we have a prior checkpoint for this
    // folder. The UI uses this to show a "resuming from N/M" banner — the
    // user knows their previous stop/crash didn't throw away progress.
    {
        if let Ok(conn) = db_conn.lock() {
            if let Ok(Some((last_path, processed, _total))) =
                db::get_scan_checkpoint(&conn, &folder)
            {
                app_handle.emit("scan-resumed", serde_json::json!({
                    "folder": folder,
                    "last_path": last_path,
                    "processed": processed,
                    "total": total,
                })).ok();
            }
        }
    }

    let scanned = Arc::new(AtomicUsize::new(0));
    let new_count = Arc::new(AtomicUsize::new(0));
    let skipped_count = Arc::new(AtomicUsize::new(0));

    // Writer channel: workers fan in here, a single thread drains and batches.
    let (tx, rx) = std::sync::mpsc::channel::<PendingInsert>();

    // Writer thread — owns the DB mutex during each batch commit, never in
    // rayon pool so we can't deadlock against workers.
    let writer_db = db_conn.clone();
    let writer_new_count = new_count.clone();
    let writer_handle = std::thread::spawn(move || {
        const BATCH_SIZE: usize = 64;
        let mut batch: Vec<PendingInsert> = Vec::with_capacity(BATCH_SIZE);
        let mut drain = |batch: &mut Vec<PendingInsert>| {
            if batch.is_empty() { return; }
            let Ok(conn) = writer_db.lock() else { batch.clear(); return; };
            let Ok(txn) = conn.unchecked_transaction() else { batch.clear(); return; };
            for p in batch.drain(..) {
                let np = db::NewPhoto {
                    path: &p.path,
                    filename: &p.filename,
                    folder: &p.folder,
                    hash: &p.hash,
                    size: p.size,
                    width: p.width,
                    height: p.height,
                    media_type: &p.media_type,
                    date_taken: p.date_taken.clone(),
                    duration_secs: p.duration_secs,
                };
                if let Ok(id) = db::insert_photo(&txn, &np) {
                    if id > 0 {
                        if let Some(tp) = &p.thumb_path {
                            db::update_thumbnail_path(&txn, id, tp).ok();
                        }
                        // v1.5.104 — pick up XMP sidecar tags on first
                        // scan. Mac side (and any external tool like
                        // Lightroom / DigiKam) writes a `.xmp` next to
                        // the photo; reading it here means
                        // cross-machine tag flow finally works without
                        // a separate sync pipeline. v1.5.106: use a
                        // direct INSERT — db::insert_tags would open a
                        // nested transaction which SQLite silently
                        // fails. Failures are still soft; a malformed
                        // sidecar shouldn't abort the scan tx.
                        if let Ok(Some(xmp)) = crate::xmp::read_xmp_sidecar(&p.path) {
                            if !xmp.keywords.is_empty() {
                                // v1.5.111 — case-insensitive dup guard
                                // (see lib.rs comment for rationale).
                                if let Ok(mut stmt) = txn.prepare_cached(
                                    "INSERT INTO tags (photo_id, tag, confidence, source)
                                     SELECT ?1, ?2, ?3, ?4
                                     WHERE NOT EXISTS (
                                         SELECT 1 FROM tags
                                         WHERE photo_id = ?1 AND tag = ?2 COLLATE NOCASE
                                     )"
                                ) {
                                    for tag in &xmp.keywords {
                                        let _ = stmt.execute(rusqlite::params![id, tag, 1.0_f64, "xmp_sidecar"]);
                                    }
                                }
                                // v1.5.110 — fresh scan that picked up
                                // sidecar tags transitions the row
                                // straight to 'tagged' so the
                                // "Tagged" stat is correct from the
                                // first scan, not after the next
                                // backfill.
                                let now = chrono::Utc::now().to_rfc3339();
                                let _ = txn.execute(
                                    "UPDATE photos SET status='tagged', tagged_at=?2 WHERE id=?1 AND status='pending'",
                                    rusqlite::params![id, now],
                                );
                            }
                            if let Some(desc) = xmp.description.as_deref() {
                                let _ = db::update_photo_description(&txn, id, desc);
                            }
                            if let Some(r) = xmp.rating {
                                let _ = db::set_rating(&txn, id, r);
                            }
                            if xmp.label.as_deref() == Some("Red") {
                                let _ = db::set_favorite(&txn, id, true);
                            }
                        }
                        writer_new_count.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            let _ = txn.commit();
        };

        while let Ok(item) = rx.recv() {
            batch.push(item);
            if batch.len() >= BATCH_SIZE {
                drain(&mut batch);
            }
        }
        drain(&mut batch);
    });

    // Worker pool — fan out across rayon's global thread pool.
    all_paths.par_iter().for_each(|path| {
        if stop_flag.load(Ordering::Relaxed) { return; }

        let path_str = path.to_string_lossy().to_string();
        // v1.5.75 — guard against empty filename. A drive root like `D:\`
        // (which some WalkDir entries return) has no file_name → empty
        // string, and that empty string flowed into DB.filename. Later
        // operations (XMP write, rename UI, search highlight) all break
        // on a "" filename. Skip these rows entirely.
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let filename = match filename {
            Some(f) => f,
            None => return,
        };
        let folder_str = path.parent().unwrap_or(path).to_string_lossy().to_string();
        let mtype = media_type_for_path(path);

        let emit_progress = |current: &str| {
            let done = scanned.load(Ordering::Relaxed);
            if done % 20 == 0 || done == total {
                app_handle.emit("scan-progress", ScanProgress {
                    total,
                    scanned: done,
                    new_files: new_count.load(Ordering::Relaxed),
                    skipped: skipped_count.load(Ordering::Relaxed),
                    current_file: current.to_string(),
                    is_running: true,
                }).ok();
            }
        };

        // Hash first — cheap enough to always compute, and it's the dedup key.
        // Cache (path, size, mtime) → hash so re-scans of unchanged files
        // skip the 20-50 ms SHA pass. On a 50 k library this removes the
        // bulk of rescan time.
        let meta = std::fs::metadata(&path_str).ok();
        let file_size = meta.as_ref().map(|m| m.len() as i64).unwrap_or(-1);
        let file_mtime = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(-1);

        let cached = if file_size >= 0 && file_mtime >= 0 {
            if let Ok(conn) = db_conn.lock() {
                db::get_cached_file_hash(&conn, &path_str, file_size, file_mtime)
            } else {
                None
            }
        } else {
            None
        };

        let hash = match cached {
            Some(h) => h,
            None => match compute_hash(&path_str) {
                Ok(h) => {
                    if file_size >= 0 && file_mtime >= 0 {
                        if let Ok(conn) = db_conn.lock() {
                            db::put_cached_file_hash(&conn, &path_str, file_size, file_mtime, &h);
                        }
                    }
                    h
                }
                Err(e) => {
                    app_handle.emit("scan-file-error", serde_json::json!({
                        "file": path_str, "error": e.to_string()
                    })).ok();
                    scanned.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            },
        };

        if existing_hashes.contains(&hash) {
            skipped_count.fetch_add(1, Ordering::Relaxed);
            scanned.fetch_add(1, Ordering::Relaxed);
            emit_progress(&filename);
            // Checkpoint every 200 files so a crash doesn't lose progress.
            let done = scanned.load(Ordering::Relaxed);
            if done % 200 == 0 {
                if let Ok(conn) = db_conn.lock() {
                    let _ = db::put_scan_checkpoint(&conn, &folder, &path_str, done as i64, total as i64);
                }
            }
            return;
        }

        // New file — gather dimensions/metadata and try to build a thumbnail.
        let (width, height) = if mtype == "image" {
            image::image_dimensions(&path_str)
                .map(|(w, h)| (Some(w as i32), Some(h as i32)))
                .unwrap_or((None, None))
        } else {
            (None, None)
        };

        let size = std::fs::metadata(&path_str)
            .map(|m| m.len() as i64)
            .unwrap_or(0);

        let date_taken = extract_date_taken(&path_str);

        let duration_secs = if mtype == "video" {
            extract_video_duration(&path_str)
        } else { None };

        let thumb_path = match thumbnail::get_or_create_thumbnail(&path_str, &hash, &thumbnails_dir, 256) {
            Ok(_) => {
                let cache_name = thumbnail::thumb_cache_name(&hash);
                Some(thumbnails_dir.join(&cache_name).to_string_lossy().to_string())
            }
            Err(_) => None,
        };

        // Send to writer. If the channel is closed (stop_scan), ignore.
        let _ = tx.send(PendingInsert {
            path: path_str.clone(),
            filename: filename.clone(),
            folder: folder_str,
            hash,
            size,
            width, height,
            media_type: mtype.to_string(),
            date_taken,
            duration_secs,
            thumb_path,
        });

        scanned.fetch_add(1, Ordering::Relaxed);
        emit_progress(&filename);
        // Persist checkpoint every 200 files.
        let done = scanned.load(Ordering::Relaxed);
        if done % 200 == 0 {
            if let Ok(conn) = db_conn.lock() {
                let _ = db::put_scan_checkpoint(&conn, &folder, &path_str, done as i64, total as i64);
            }
        }
    });

    // Close channel, let writer finish draining its final partial batch.
    drop(tx);
    let _ = writer_handle.join();

    let new_files = new_count.load(Ordering::Relaxed);
    let skipped = skipped_count.load(Ordering::Relaxed);

    // If the scan ran to completion (not stopped), clear the checkpoint so
    // the next run starts fresh. A mid-run stop leaves the checkpoint in
    // place so the UI can show the resume banner.
    if !stop_flag.load(Ordering::Relaxed) {
        if let Ok(conn) = db_conn.lock() {
            let _ = db::clear_scan_checkpoint(&conn, &folder);
        }
    }

    app_handle.emit("scan-progress", ScanProgress {
        total,
        scanned: total,
        new_files,
        skipped,
        current_file: String::new(),
        is_running: false,
    }).ok();

    Ok(ScanComplete { new_files, skipped, total, folder })
}
