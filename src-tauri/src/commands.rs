use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use tauri::{Emitter, Manager};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

use crate::{clip, db, export, exif_reader, models::*, providers::{self, DEFAULT_OLLAMA_URL}, router::SmartRouter, thumbnail, xmp, AppState};

// v1.5.144 — Network-mount detection. Windows equivalent of Mac's
// `/sbin/mount` parsing: we ask the OS what kind of drive a path
// lives on and treat DRIVE_REMOTE the same way Mac treats SMB/AFP.
// Used by the health-check + orphan-cleanup paths so a transiently
// unreachable network share never causes us to false-classify every
// photo as deleted (which would then let the user click "fix" and
// wipe their whole library's DB rows).
//
// UNC paths (`\\server\share\...`) are inherently remote and don't
// need GetDriveTypeW — short-circuit those first. Mapped drive
// letters (`Z:\...`) need the syscall because we can't tell from
// the path alone whether Z: is a USB stick or an SMB mount.
//
// Returns false (treat as local, safe to stat) on unknown/error so
// non-Windows builds and any future ambiguity defaults to the
// historical behaviour rather than newly hiding local files.
#[cfg(target_os = "windows")]
fn is_network_path(path: &str) -> bool {
    // UNC: starts with \\ or //
    let trimmed = path.trim_start();
    if trimmed.starts_with(r"\\") || trimmed.starts_with("//") {
        return true;
    }
    // Drive-letter form ("D:\..."): probe with GetDriveTypeW.
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        let root = format!("{}:\\", (bytes[0] as char).to_ascii_uppercase());
        let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        use windows::Win32::Storage::FileSystem::GetDriveTypeW;
        use windows::core::PCWSTR;
        // Safety: GetDriveTypeW reads the wide-string up to NUL; we
        // pass a freshly-built UTF-16 buffer terminated with 0.
        // DRIVE_REMOTE = 4 per the Win32 API contract — using the raw
        // integer rather than the windows-rs symbol avoids pulling in
        // an extra feature flag just for one named constant.
        let dt = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
        return dt == 4;
    }
    false
}

#[cfg(not(target_os = "windows"))]
fn is_network_path(_path: &str) -> bool {
    false
}

// ── Folder / Scan ─────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn open_folder_dialog(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let result = tokio::task::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title("Select Photo Folder")
            .blocking_pick_folder()
    })
    .await
    .map_err(|e| e.to_string())?;

    Ok(result
        .and_then(|fp| fp.into_path().ok())
        .map(|p| p.to_string_lossy().to_string()))
}

/// Single-file picker. Used by the Missing Files modal to let the user
/// point a library photo at its new on-disk location; `relink_photo`
/// then hash-verifies that the picked file matches before updating the
/// DB row, so tags/faces/ratings can't bond to the wrong image.
///
/// `title` is what the native OS picker shows; callers pass a
/// context-specific string (e.g. "Yeni dosya konumunu seç").
#[tauri::command]
pub async fn open_file_dialog(
    app: tauri::AppHandle,
    title: Option<String>,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let t = title.unwrap_or_else(|| "Select File".to_string());
    let result = tokio::task::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title(&t)
            .blocking_pick_file()
    })
    .await
    .map_err(|e| e.to_string())?;

    Ok(result
        .and_then(|fp| fp.into_path().ok())
        .map(|p| p.to_string_lossy().to_string()))
}

#[tauri::command]
pub async fn scan_folder(
    folder: String,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    if state.scan_running.swap(true, Ordering::SeqCst) {
        return Err("Scan already in progress".into());
    }

    let db_arc = state.db.clone();
    let thumbs_dir = state.thumbnails_dir.clone();
    let scan_running = state.scan_running.clone();
    let stop_flag = state.scan_stop.clone();
    stop_flag.store(false, Ordering::SeqCst);

    let started_at = chrono::Utc::now().to_rfc3339();
    let folder_for_log = folder.clone();
    let db_for_log = db_arc.clone();

    tokio::spawn(async move {
        let result = crate::scanner::scan_folder_impl(
            folder,
            db_arc,
            thumbs_dir,
            stop_flag,
            app_handle.clone(),
        )
        .await;

        scan_running.store(false, Ordering::SeqCst);

        let finished_at = chrono::Utc::now().to_rfc3339();
        match &result {
            Ok(stats) => {
                if let Ok(conn) = db_for_log.lock() {
                    let _ = db::log_scan_history(
                        &conn,
                        &folder_for_log,
                        &started_at,
                        &finished_at,
                        stats.new_files as i64,
                        stats.skipped as i64,
                        stats.total as i64,
                        None,
                    );
                    // Keep the "Last check" column in the Watch Folders UI
                    // honest. No-op if this scan wasn't a watch folder.
                    let _ = db::update_watch_folder_checked_by_path(&conn, &folder_for_log);
                }
            }
            Err(e) => {
                let err_str = e.to_string();
                if let Ok(conn) = db_for_log.lock() {
                    let _ = db::log_scan_history(
                        &conn,
                        &folder_for_log,
                        &started_at,
                        &finished_at,
                        0,
                        0,
                        0,
                        Some(err_str.as_str()),
                    );
                }
            }
        }

        match result {
            Ok(stats) => {
                app_handle.emit("scan-complete", stats).ok();
            }
            Err(e) => {
                app_handle.emit("scan-error", e.to_string()).ok();
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub async fn get_scan_history(
    limit: Option<i64>,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let limit = limit.unwrap_or(50).clamp(1, 500);
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let rows = db::get_scan_history(&conn, limit).map_err(|e| e.to_string())?;
    let list: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(id, folder, started_at, finished_at, new_files, skipped, total, error)| {
            serde_json::json!({
                "id": id,
                "folder": folder,
                "started_at": started_at,
                "finished_at": finished_at,
                "new_files": new_files,
                "skipped": skipped,
                "total": total,
                "error": error,
            })
        })
        .collect();
    Ok(serde_json::Value::Array(list))
}

#[tauri::command]
pub async fn stop_scan(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.scan_stop.store(true, Ordering::SeqCst);
    Ok(())
}

// ── Photo queries ─────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_folders(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<(String, i64)>, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|_| "db lock".to_string())?;
        db::get_folders(&conn).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_folders_with_status(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<(String, i64, i64)>, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|_| "db lock".to_string())?;
        db::get_folders_with_status(&conn).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// v1.5.72 — Heavy DB commands moved off the async runtime's worker threads
// via tauri::async_runtime::spawn_blocking. Without this, on a 60K+ photo
// library every query takes hundreds of ms to a few seconds while holding a
// std::sync::Mutex lock. Several parallel invokes (frontend init fans out
// 4–5 of these on launch) saturate Tokio's worker pool → IPC messages
// queue behind them and the WebView appears frozen for 5–10 seconds.
//
// Wrapping the blocking work in spawn_blocking lets the async runtime
// keep dispatching IPC while the DB work runs on the dedicated blocking
// pool (default 512 threads), so UI clicks register immediately even
// during a slow query.

#[tauri::command]
pub async fn get_photos(
    offset: i64,
    limit: i64,
    folder: Option<String>,
    tag_filter: Option<String>,
    status_filter: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<PhotosResponse, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|_| "db lock".to_string())?;
        db::get_photos(
            &conn,
            offset,
            limit,
            folder.as_deref(),
            tag_filter.as_deref(),
            status_filter.as_deref(),
        )
        .map(|(photos, total)| PhotosResponse { photos, total })
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_photo_detail(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<Photo, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|_| "db lock".to_string())?;
        db::get_photo_detail(&conn, photo_id).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_stats(state: tauri::State<'_, AppState>) -> Result<AppStats, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|_| "db lock".to_string())?;
        db::get_stats(&conn).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ── Search with multi-language translation ───────────────────────────────────

#[tauri::command]
pub async fn search_photos(
    query: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<PhotoSummary>, String> {
    let trimmed = query.trim().to_string();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }

    // v1.5.49 — Pre-parse for Google-style operators before any
    // translation. If the user wrote `tag:cat -dog year:2020 "red car"`
    // we need to honour each piece separately. The parser splits the
    // query into:
    //   * must_terms / phrases / must_not  → folded into the FTS query
    //   * fields                           → applied as post-filter
    let parsed = parse_search_query(&trimmed);
    let has_advanced_syntax = !parsed.fields.is_empty()
        || !parsed.phrases.is_empty()
        || !parsed.must_not.is_empty();

    // Helpers used by the advanced path.
    // v1.5.50 — apply_post also drops photos that contain any must_not
    // term (so `beach -night` excludes everything tagged with "night"
    // even on the translation+FTS code path).
    let apply_post = |list: Vec<PhotoSummary>, parsed: &ParsedQuery| -> Vec<PhotoSummary> {
        let must_not_lc: Vec<String> = parsed.must_not.iter().map(|s| s.to_lowercase()).collect();
        let mut out: Vec<PhotoSummary> = list.into_iter()
            .filter(|p| parsed.fields.iter().all(|(k, v)| passes_field_filter(p, k, v)))
            .filter(|p| {
                if must_not_lc.is_empty() { return true; }
                let lower_tags: Vec<String> = p.tags.iter().map(|t| t.to_lowercase()).collect();
                let lower_path = p.path.to_lowercase();
                !must_not_lc.iter().any(|n| {
                    lower_tags.iter().any(|t| t.contains(n)) || lower_path.contains(n)
                })
            })
            .collect();
        // Rank by relevance and stable-sort (highest score first).
        out.sort_by(|a, b| relevance_score(b, parsed).cmp(&relevance_score(a, parsed)));
        out
    };

    // Fast path for queries that ONLY use operators/fields without any
    // free-text terms (eg. `year:2020 fav:true`). FTS isn't needed —
    // pull a wide pool and filter server-side.
    if has_advanced_syntax && parsed.must_terms.is_empty() && parsed.phrases.is_empty() {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let (rows, _) = db::get_photos(&conn, 0, 5000, None, None, None)
            .map_err(|e| e.to_string())?;
        // Apply person filter via DB if present (joins face_regions).
        // v1.5.53 — comma in value = OR (`person:Lara,Buğra`).
        let mut results = rows;
        for (k, v) in &parsed.fields {
            if k == "person" {
                let names: Vec<&str> = v.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                let mut allowed: std::collections::HashSet<i64> = std::collections::HashSet::new();
                for name in &names {
                    if let Ok(hits) = db::search_photos_by_person(&conn, name) {
                        for p in hits { allowed.insert(p.id); }
                    }
                }
                results.retain(|p| allowed.contains(&p.id));
            } else if k == "camera" || k == "lens" || k == "location" || k == "place" || k == "city" || k == "country" {
                // No camera column in PhotoSummary; ignore for now.
            }
        }
        return Ok(apply_post(results, &parsed));
    }

    // Check if the query contains non-ASCII (likely non-English)
    let is_non_english = trimmed.chars().any(|c| !c.is_ascii());

    // If a non-ASCII query exactly matches an existing tag (e.g. person name "Buğra"),
    // return those results immediately without any translation.
    // Only for non-ASCII to avoid short-circuiting common English words like "blue".
    if is_non_english {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let exact: Vec<PhotoSummary> = db::search_photos_by_tag_exact(&conn, &trimmed)
            .unwrap_or_default();
        if !exact.is_empty() {
            return Ok(exact);
        }
    }

    if is_non_english {
        // Try translation cache first
        let cached = {
            let conn = state.db.lock().map_err(|_| "db lock")?;
            db::get_cached_translation(&conn, &trimmed).ok().flatten()
        };

        let english_terms = if let Some(cached_json) = cached {
            // Parse cached translation
            serde_json::from_str::<Vec<String>>(&cached_json).unwrap_or_else(|_| {
                cached_json
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect()
            })
        } else {
            // Need to translate — find cheapest text provider
            let router = SmartRouter::new(&state.db);
            if let Some((provider, api_key)) = router.cheapest_text_provider() {
                match providers::translate_query(&trimmed, provider, &api_key).await {
                    Ok(terms) if !terms.is_empty() => {
                        // Cache translation
                        let json = serde_json::to_string(&terms).unwrap_or_default();
                        let conn = state.db.lock().map_err(|_| "db lock")?;
                        db::cache_translation(
                            &conn,
                            &trimmed,
                            None,
                            &json,
                            provider.key_name(),
                        )
                        .ok();
                        terms
                    }
                    _ => {
                        // Fallback: search as-is
                        vec![trimmed.clone()]
                    }
                }
            } else {
                vec![trimmed.clone()]
            }
        };

        // Search with translated terms + original + synonym groups
        // v1.5.52 — Also run the original (Turkish) word through
        // expand_synonyms; the synonyms table now carries Turkish ↔
        // English mappings so a query like "kedi" instantly fans out
        // to ["cat", "kitten", "feline", ...] without needing the
        // translation API to succeed first.
        let mut all_terms = english_terms;
        all_terms.push(trimmed.clone());
        for word in trimmed.split_whitespace() {
            for v in expand_synonyms(word) {
                if !all_terms.iter().any(|x| x.eq_ignore_ascii_case(&v)) {
                    all_terms.push(v);
                }
            }
        }
        all_terms.sort();
        all_terms.dedup();

        let conn = state.db.lock().map_err(|_| "db lock")?;
        let mut results = db::search_photos_multi(&conn, &all_terms).map_err(|e| e.to_string())?;
        // Also search by person name and merge
        if let Ok(person_results) = db::search_photos_by_person(&conn, &trimmed) {
            merge_photo_results(&mut results, person_results);
        }
        // Also search filename/folder path via photos_fts (typed "Tatil" → matches folder)
        if let Ok(path_results) = db::search_photos_by_path(&conn, &trimmed) {
            merge_photo_results(&mut results, path_results);
        }
        // Also search AI descriptions (with all translated terms)
        for term in &all_terms {
            if let Ok(desc_results) = db::search_photos_by_description(&conn, term) {
                merge_photo_results(&mut results, desc_results);
            }
        }
        // Re-rank: photos with context tags score higher (e.g. "bardak" → "cup" + context "drink")
        let ctx_tags = get_context_tags(&trimmed);
        if !ctx_tags.is_empty() {
            rank_by_context(&mut results, &ctx_tags);
        }
        // v1.5.52/53 — Person field intersection. If the user wrote
        // `person:Lara beach`, the FTS hit only enforced "beach"; we
        // now intersect with photos containing Lara. Comma-separated
        // names (`person:Lara,Buğra`) are treated as OR — photos with
        // EITHER person pass.
        for (k, v) in &parsed.fields {
            if k == "person" {
                let names: Vec<&str> = v.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                let mut allowed: std::collections::HashSet<i64> = std::collections::HashSet::new();
                for name in &names {
                    if let Ok(hits) = db::search_photos_by_person(&conn, name) {
                        for p in hits { allowed.insert(p.id); }
                    }
                }
                results.retain(|p| allowed.contains(&p.id));
            }
        }
        // v1.5.51 — fuzzy fallback for the non-English path too.
        if results.is_empty() && parsed.must_terms.len() == 1 {
            // Drop the lock that search_photos_multi acquired so the
            // fallback can re-acquire.
            drop(conn);
            results = fuzzy_tag_fallback(&state, &parsed).unwrap_or_default();
        }
        // v1.5.49 — apply field filters + relevance sort as the final pass.
        Ok(apply_post(results, &parsed))
    } else {
        // ── English search (v1.5.71) ─────────────────────────────────────────
        // Stop-word filtering + per-concept group-AND logic.
        //
        // "couple on the boat" previously OR-expanded ALL synonym groups for
        // all 4 words, making "on", "the", "love", "gemi" etc. all contribute
        // to a massive OR that matched most of the library.
        //
        // New approach:
        //   1. Strip common prepositions/articles so only subject words remain.
        //   2. Build a synonym group for each subject word.
        //   3. Multi-word: intersect per-group result sets (AND of ORs).
        //      "couple on the boat" → couple-synonyms ∩ boat-synonyms.
        //   4. Single word: keep the full synonym OR for broad recall.

        const STOP_WORDS: &[&str] = &[
            "the","a","an","in","on","at","of","by","to","for","with",
            "is","are","was","were","be","been","from","and","or","but",
            "this","that","these","those","into","over","after","under",
            "about","between","through","up","out","its","it","as","all",
            "both","than","too","also","some","any","my","your","our",
            "his","her","their","i","we","you","he","she","they",
        ];
        let all_words: Vec<String> = trimmed.split_whitespace().map(|w| w.to_string()).collect();
        let content_words: Vec<String> = all_words.iter()
            .filter(|w| !STOP_WORDS.contains(&w.to_lowercase().as_str()))
            .cloned()
            .collect();
        // Edge-case: if every word was a stop word, fall back to the full list.
        let content_words = if content_words.is_empty() { all_words } else { content_words };

        // Per-concept synonym groups — one Vec per content word.
        // e.g. "boat" → ["boat","ship","tekne","gemi","yat"]
        //      "couple" → ["couple","romantic","love","together","pair",...]
        let synonym_groups: Vec<Vec<String>> = content_words.iter()
            .map(|w| expand_synonyms(w))
            .collect();

        let conn = state.db.lock().map_err(|_| "db lock")?;

        let mut results: Vec<PhotoSummary> = if content_words.len() > 1 {
            // Multi-concept: find photos that match EVERY concept (AND of ORs).
            let mut group_ids: Vec<std::collections::HashSet<i64>> = Vec::new();
            for group in &synonym_groups {
                if let Ok(hits) = db::search_photos_multi(&conn, group) {
                    let ids: std::collections::HashSet<i64> =
                        hits.iter().map(|p| p.id).collect();
                    if !ids.is_empty() {
                        group_ids.push(ids);
                    }
                }
            }
            if group_ids.is_empty() {
                vec![]
            } else {
                let mut intersection = group_ids[0].clone();
                for next in group_ids.iter().skip(1) {
                    intersection = intersection.intersection(next).cloned().collect();
                }
                if !intersection.is_empty() {
                    let mut ids: Vec<i64> = intersection.into_iter().collect();
                    ids.sort_unstable();
                    db::get_photos_by_ids(&conn, &ids).unwrap_or_default()
                } else {
                    // Intersection empty — OR-search over content words only
                    // (no synonym explosion keeps results tight).
                    db::search_photos_multi(&conn, &content_words).unwrap_or_default()
                }
            }
        } else {
            // Single concept: expand synonyms fully and OR-search for broad recall.
            let mut all_terms: Vec<String> = synonym_groups.iter()
                .flatten()
                .cloned()
                .collect();
            all_terms.sort();
            all_terms.dedup();
            if all_terms.len() > 1 {
                db::search_photos_multi(&conn, &all_terms).map_err(|e| e.to_string())?
            } else {
                let fts_query = format!("{}*", all_terms.first().unwrap_or(&trimmed));
                db::search_photos_fts(&conn, &fts_query).map_err(|e| e.to_string())?
            }
        };

        // Person name search uses the full trimmed query.
        if let Ok(person_results) = db::search_photos_by_person(&conn, &trimmed) {
            merge_photo_results(&mut results, person_results);
        }
        // Path/filename search — pass content words only so stop words like
        // "on" and "the" don't hit every path fragment.
        let path_query = content_words.join(" ");
        if let Ok(path_results) = db::search_photos_by_path(&conn, &path_query) {
            merge_photo_results(&mut results, path_results);
        }
        // Description search — content words only (not full synonym expansion,
        // which caused tangential matches via description text).
        for word in &content_words {
            if let Ok(desc_results) = db::search_photos_by_description(&conn, word) {
                merge_photo_results(&mut results, desc_results);
            }
        }
        // Context re-ranking.
        let ctx_tags = get_context_tags(&trimmed);
        if !ctx_tags.is_empty() {
            rank_by_context(&mut results, &ctx_tags);
        }
        // v1.5.52/53 — Person field intersection (English path); same
        // OR-on-comma semantics as the non-English branch above.
        for (k, v) in &parsed.fields {
            if k == "person" {
                let names: Vec<&str> = v.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                let mut allowed: std::collections::HashSet<i64> = std::collections::HashSet::new();
                for name in &names {
                    if let Ok(hits) = db::search_photos_by_person(&conn, name) {
                        for p in hits { allowed.insert(p.id); }
                    }
                }
                results.retain(|p| allowed.contains(&p.id));
            }
        }
        // v1.5.51 — fuzzy fallback for single short-word queries.
        if results.is_empty() && parsed.must_terms.len() == 1 && parsed.must_terms[0].len() >= 1 {
            drop(conn);
            results = fuzzy_tag_fallback(&state, &parsed).unwrap_or_default();
        }
        Ok(apply_post(results, &parsed))
    }
}

/// v1.5.51 — Look up every tag in the library, score by edit distance
/// to the user's query token, and return photos whose tags fall inside
/// the budget. Capped to top 30 fuzzy tags to keep cost bounded.
fn fuzzy_tag_fallback(
    state: &tauri::State<'_, AppState>,
    parsed: &ParsedQuery,
) -> Option<Vec<PhotoSummary>> {
    let token = parsed.must_terms.first()?.first()?.to_lowercase();
    let budget = fuzzy_budget(&token);
    if budget == 0 { return None; }
    let conn = state.db.lock().ok()?;
    let all_tags = db::get_all_tags(&conn, None).ok()?;
    let mut scored: Vec<(String, usize, i64)> = Vec::new();
    for (tag, count) in all_tags.iter() {
        let lc = tag.to_lowercase();
        // Cheap pre-filter: skip tags whose length difference exceeds
        // the budget (early-out).
        if lc.chars().count().abs_diff(token.chars().count()) > budget { continue; }
        let d = edit_distance_cap(&lc, &token, budget);
        if d <= budget {
            scored.push((tag.clone(), d, *count));
        }
    }
    if scored.is_empty() { return None; }
    // Closest-first; ties broken by frequency.
    scored.sort_by(|a, b| a.1.cmp(&b.1).then(b.2.cmp(&a.2)));
    scored.truncate(30);
    let terms: Vec<String> = scored.iter().map(|(t, _, _)| t.clone()).collect();
    db::search_photos_multi(&conn, &terms).ok()
}

/// v1.5.53 — Split a string like "30days" or "6months" into ("30", "days").
/// Returns None if it's not a count+unit pair.
fn split_count_unit(s: &str) -> Option<(&str, &str)> {
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    if split == 0 { return None; }
    let (num, rest) = s.split_at(split);
    let unit = rest.trim_start_matches('-');
    if unit.is_empty() { return None; }
    Some((num, unit))
}

// ── v1.5.50 search text normalization ─────────────────────────────────────────
//
// Helpers used by the search pipeline to make a tag query like "kediler"
// match a stored tag of "kedi" (Turkish plural strip), or "agac" match
// "ağaç" (Turkish accent strip), or "running" match "run" (English -ing
// strip). All variants are added to the FTS query as alternatives so the
// MATCH still goes through the FTS5 index — we never do a full table
// scan.

/// Strip Turkish-specific accents to ASCII so accent-insensitive
/// matching works without a custom SQLite collation.
fn strip_tr_accents(s: &str) -> String {
    s.chars().map(|c| match c {
        'ç' => 'c', 'Ç' => 'C',
        'ğ' => 'g', 'Ğ' => 'G',
        'ı' => 'i', 'İ' => 'i',
        'ö' => 'o', 'Ö' => 'O',
        'ş' => 's', 'Ş' => 'S',
        'ü' => 'u', 'Ü' => 'U',
        c => c,
    }).collect()
}

/// Very light Turkish + English stemmer. We only strip a few highly
/// productive suffixes; deeper morphology would need a real stemmer
/// crate which isn't worth the binary cost for this use case.
fn light_stem(s: &str) -> String {
    let lower = s.to_lowercase();
    let n = lower.chars().count();
    // Turkish plural -ler / -lar (≥4 chars)
    if n >= 4 {
        if lower.ends_with("ler") || lower.ends_with("lar") {
            return lower[..lower.len()-3].to_string();
        }
    }
    // Turkish person/posessive -dan/-den/-tan/-ten/-nin/-nın/-da/-de/-ta/-te
    // (kept conservative: -ler/-lar already covers most plural search needs)
    // English -ing (≥5 chars), -ed (≥4 chars), -es (≥4 chars), -s (≥3 chars).
    if n >= 5 && lower.ends_with("ing") {
        return lower[..lower.len()-3].to_string();
    }
    if n >= 4 && lower.ends_with("ed") {
        return lower[..lower.len()-2].to_string();
    }
    if n >= 4 && lower.ends_with("es") {
        return lower[..lower.len()-2].to_string();
    }
    if n >= 3 && lower.ends_with('s') && !lower.ends_with("ss") {
        return lower[..lower.len()-1].to_string();
    }
    lower
}

/// Levenshtein edit distance between two strings, capped at `max`. Returns
/// `max+1` once the bound is exceeded so callers can short-circuit.
/// Used by the fuzzy-match fallback when an exact / variant search
/// returns nothing — gives the user typo tolerance.
fn edit_distance_cap(a: &str, b: &str, max: usize) -> usize {
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();
    let (la, lb) = (av.len(), bv.len());
    if la == 0 { return lb; }
    if lb == 0 { return la; }
    if la.abs_diff(lb) > max { return max + 1; }
    let mut prev: Vec<usize> = (0..=lb).collect();
    let mut curr: Vec<usize> = vec![0; lb + 1];
    for i in 1..=la {
        curr[0] = i;
        let mut row_min = curr[0];
        for j in 1..=lb {
            let cost = if av[i-1].to_lowercase().eq(bv[j-1].to_lowercase()) { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j-1] + 1).min(prev[j-1] + cost);
            if curr[j] < row_min { row_min = curr[j]; }
        }
        if row_min > max { return max + 1; }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[lb]
}

/// Allowed edit-distance budget for a query token. Short queries don't
/// tolerate any errors (matching the user's intent); medium queries
/// allow 1; long queries allow 2.
fn fuzzy_budget(token: &str) -> usize {
    let n = token.chars().count();
    if n <= 4 { 0 }
    else if n <= 7 { 1 }
    else { 2 }
}

/// Generate alternative spellings for a single search term: original,
/// accent-stripped, and stemmed. Deduped + lowercased. The FTS query is
/// then `(variant1 OR variant2 OR …)` for that term, which matches a
/// tag stored in any of the variant forms.
fn term_variants(term: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(4);
    let push = |out: &mut Vec<String>, s: String| {
        if s.is_empty() { return; }
        if !out.iter().any(|e: &String| e == &s) { out.push(s); }
    };
    let lower = term.to_lowercase();
    push(&mut out, lower.clone());
    let no_accent = strip_tr_accents(&lower);
    if no_accent != lower { push(&mut out, no_accent.clone()); }
    let stemmed = light_stem(&lower);
    if stemmed != lower { push(&mut out, stemmed); }
    let stemmed_no_accent = light_stem(&no_accent);
    if stemmed_no_accent != lower { push(&mut out, stemmed_no_accent); }
    out
}

/// Turn a date-keyword shorthand ("today" / "yesterday" / "this-year"
/// / "last-month" / "bugün" / "dün" / "bu-yıl" / "30days" / "6months"
/// …) into a `(after, before)` pair of YYYY-MM-DD strings. Returns None
/// when the value isn't a recognised keyword. Used by
/// `parse_search_query` to resolve `date:today` / `before:30days` style
/// filters before they hit the field post-filter.
fn date_keyword_range(v: &str) -> Option<(String, String)> {
    use chrono::{Datelike, Duration, Local, NaiveDate};
    let key = v.to_lowercase().replace('_', "-");
    let today = Local::now().date_naive();
    let fmt = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
    // v1.5.53 — Relative date math: "30days", "6months", "2years",
    // "1week" — interpreted as "N units ago" so `before:30days` means
    // "older than 30 days ago" and `after:6months` means "newer than
    // 6 months ago". The day/month/year-end heuristic keeps the
    // comparison stable across year boundaries.
    if let Some((n_str, unit)) = split_count_unit(&key) {
        if let Ok(n) = n_str.parse::<i64>() {
            let target = match unit {
                "day" | "days" | "gun" | "günler" | "gün" | "günü" => Some(today - Duration::days(n)),
                "week" | "weeks" | "hafta" | "haftalar" => Some(today - Duration::days(n * 7)),
                "month" | "months" | "ay" | "aylar" => {
                    let mut y = today.year();
                    let mut m = today.month() as i64 - n;
                    while m < 1 { m += 12; y -= 1; }
                    NaiveDate::from_ymd_opt(y, m as u32, today.day().min(28))
                }
                "year" | "years" | "yil" | "yıl" | "yillar" | "yıllar" => {
                    NaiveDate::from_ymd_opt(today.year() - n as i32, today.month(), today.day().min(28))
                }
                _ => None,
            };
            if let Some(d) = target {
                return Some((fmt(d), fmt(d)));
            }
        }
    }
    match key.as_str() {
        "today" | "bugun" | "bugün" => {
            Some((fmt(today), fmt(today)))
        }
        "yesterday" | "dun" | "dün" => {
            let y = today - Duration::days(1);
            Some((fmt(y), fmt(y)))
        }
        "this-week" | "thisweek" | "bu-hafta" | "buhafta" => {
            // ISO week starts Monday.
            let weekday = today.weekday().num_days_from_monday() as i64;
            let start = today - Duration::days(weekday);
            Some((fmt(start), fmt(today)))
        }
        "last-week" | "lastweek" | "gecen-hafta" | "geçen-hafta" => {
            let weekday = today.weekday().num_days_from_monday() as i64;
            let end = today - Duration::days(weekday + 1);
            let start = end - Duration::days(6);
            Some((fmt(start), fmt(end)))
        }
        "this-month" | "thismonth" | "bu-ay" | "buay" => {
            let start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)?;
            Some((fmt(start), fmt(today)))
        }
        "last-month" | "lastmonth" | "gecen-ay" | "geçen-ay" => {
            let (y, m) = if today.month() == 1 { (today.year()-1, 12) } else { (today.year(), today.month()-1) };
            let start = NaiveDate::from_ymd_opt(y, m, 1)?;
            // last day of that month = (next month 1) - 1 day
            let next_m = if m == 12 { (y+1, 1) } else { (y, m+1) };
            let end = NaiveDate::from_ymd_opt(next_m.0, next_m.1, 1)? - Duration::days(1);
            Some((fmt(start), fmt(end)))
        }
        "this-year" | "thisyear" | "bu-yil" | "bu-yıl" => {
            let start = NaiveDate::from_ymd_opt(today.year(), 1, 1)?;
            Some((fmt(start), fmt(today)))
        }
        "last-year" | "lastyear" | "gecen-yil" | "geçen-yıl" => {
            let start = NaiveDate::from_ymd_opt(today.year()-1, 1, 1)?;
            let end = NaiveDate::from_ymd_opt(today.year()-1, 12, 31)?;
            Some((fmt(start), fmt(end)))
        }
        _ => None,
    }
}

// ── v1.5.49 search query parser ───────────────────────────────────────────────
//
// Inputs we want to handle (Google-style, scoped to a photo library):
//   plain words      → AND tokens against tags / description / path / person
//   "quoted phrase"  → adjacent word match (FTS5 phrase syntax)
//   -word            → must-not (FTS5 NOT)
//   field:value      → typed filter, applied as a post-filter against the
//                      photo row (date_taken / folder / camera / person /
//                      rating / favorite / media_type / location)
//   year:2020-2023   → range
//   before:YYYY-MM   → upper-bound on date_taken
//   after:YYYY       → lower-bound on date_taken
//   OR / AND         → explicit boolean (default is AND)
//
// We parse to a small struct, build the FTS query for free-text terms, then
// apply the typed filters as a SQL WHERE clause on the row.

#[derive(Default, Debug, Clone)]
struct ParsedQuery {
    /// Free-text tokens that must appear (joined with FTS AND). Each
    /// inner Vec is an OR-group: alternative spellings/stems/accents of
    /// the same logical term, all of which are acceptable.
    must_terms: Vec<Vec<String>>,
    /// Quoted phrases — passed to FTS as `"..."`.
    phrases: Vec<String>,
    /// Tokens that must NOT appear.
    must_not: Vec<String>,
    /// (field, value) filters applied after FTS. Negated fields use a
    /// leading `!` on the field name (eg. `!tag` for `-tag:night`).
    fields: Vec<(String, String)>,
    /// Wildcard prefix terms (eg. `cat*`), passed straight to FTS5.
    wildcards: Vec<String>,
    /// User wrote raw OR — track to allow OR groups in the future. For
    /// now we treat a chain of `a OR b` as an OR-group (one slot in
    /// must_terms with both alternatives).
    pending_or_group: Vec<Vec<String>>,
}

fn parse_search_query(input: &str) -> ParsedQuery {
    let mut q = ParsedQuery::default();
    let mut chars = input.chars().peekable();
    let mut last_was_or = false;
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '"' {
            chars.next();
            let mut s = String::new();
            while let Some(ch) = chars.next() {
                if ch == '"' { break; }
                s.push(ch);
            }
            let s = s.trim().to_string();
            if !s.is_empty() { q.phrases.push(s); }
            last_was_or = false;
            continue;
        }
        if c == '-' {
            chars.next();
            // v1.5.55 — `-"phrase here"` excludes a phrase. Detect the
            // quote right after the minus and consume the full phrase.
            if chars.peek() == Some(&'"') {
                chars.next();
                let mut s = String::new();
                while let Some(ch) = chars.next() {
                    if ch == '"' { break; }
                    s.push(ch);
                }
                let s = s.trim().to_string();
                if !s.is_empty() { q.must_not.push(s); }
                last_was_or = false;
                continue;
            }
            let token = read_word(&mut chars);
            if !token.is_empty() {
                // Negated field operator: `-tag:night`
                if let Some(idx) = token.find(':') {
                    let key = token[..idx].to_lowercase();
                    let val = token[idx+1..].trim().to_string();
                    if !key.is_empty() && !val.is_empty() && is_known_field(&key) {
                        q.fields.push((format!("!{}", key), val));
                        last_was_or = false;
                        continue;
                    }
                }
                q.must_not.push(token);
            }
            last_was_or = false;
            continue;
        }
        let token = read_word(&mut chars);
        if token.is_empty() { continue; }
        // Explicit boolean
        if token.eq_ignore_ascii_case("or") {
            last_was_or = true;
            continue;
        }
        if token.eq_ignore_ascii_case("and") {
            last_was_or = false;
            continue;
        }
        // Field operator? `key:value`
        if let Some(idx) = token.find(':') {
            let key = token[..idx].to_lowercase();
            let val = token[idx+1..].trim().to_string();
            if !key.is_empty() && !val.is_empty() && is_known_field(&key) {
                // Date keyword shorthand: `date:today`, `year:this-year`,
                // `before:yesterday`. Resolve to absolute date pairs so
                // the post-filter sees a comparable value.
                if (key == "date" || key == "year" || key == "before" || key == "after"
                    || key == "from" || key == "to") {
                    if let Some((after, before)) = date_keyword_range(&val) {
                        if key == "before" || key == "to" {
                            q.fields.push(("before".to_string(), before));
                        } else if key == "after" || key == "from" {
                            q.fields.push(("after".to_string(), after));
                        } else {
                            // date / year — both bounds.
                            q.fields.push(("after".to_string(), after));
                            q.fields.push(("before".to_string(), before));
                        }
                        last_was_or = false;
                        continue;
                    }
                }
                q.fields.push((key, val));
                last_was_or = false;
                continue;
            }
        }
        // Wildcard prefix: `cat*` (FTS5 supports prefix searches natively).
        if token.ends_with('*') && token.len() > 1 {
            q.wildcards.push(token);
            last_was_or = false;
            continue;
        }
        let variants = term_variants(&token);
        if last_was_or {
            // Append alternatives to the previous group.
            if let Some(last) = q.must_terms.last_mut() {
                for v in variants { if !last.contains(&v) { last.push(v); } }
            } else {
                q.must_terms.push(variants);
            }
        } else {
            q.must_terms.push(variants);
        }
        last_was_or = false;
    }
    q
}

fn read_word<I: Iterator<Item = char>>(chars: &mut std::iter::Peekable<I>) -> String {
    let mut s = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() || c == '"' { break; }
        s.push(c);
        chars.next();
    }
    s
}

fn is_known_field(s: &str) -> bool {
    matches!(s, "tag" | "person" | "folder" | "in" | "path" | "year" | "date"
        | "before" | "after" | "from" | "to" | "camera" | "lens" | "rating"
        | "fav" | "favorite" | "media" | "type" | "ext" | "location" | "place"
        | "city" | "country" | "iso" | "aperture" | "focal" | "color"
        | "stars")
}

/// Build the FTS5 query string for free-text terms in a ParsedQuery.
/// v1.5.50 — handles OR-groups (variant alternatives + explicit OR),
/// wildcard prefix terms, and negated terms together.
#[allow(dead_code)]
fn build_fts_query(q: &ParsedQuery) -> Option<String> {
    let escape = |s: &str| s.replace('"', "");
    let mut parts: Vec<String> = Vec::new();
    for p in &q.phrases {
        parts.push(format!("\"{}\"", escape(p)));
    }
    for group in &q.must_terms {
        if group.len() == 1 {
            parts.push(format!("\"{}\"", escape(&group[0])));
        } else if group.len() > 1 {
            let inner: Vec<String> = group.iter().map(|v| format!("\"{}\"", escape(v))).collect();
            parts.push(format!("({})", inner.join(" OR ")));
        }
    }
    for w in &q.wildcards {
        // Strip the trailing `*` and re-emit FTS5 prefix syntax.
        let stem = w.trim_end_matches('*');
        if !stem.is_empty() {
            parts.push(format!("\"{}\"*", escape(stem)));
        }
    }
    if parts.is_empty() && q.must_not.is_empty() && q.fields.is_empty() {
        return None;
    }
    let mut s = parts.join(" AND ");
    for n in &q.must_not {
        if !s.is_empty() { s.push_str(" NOT "); } else { s.push_str("NOT "); }
        s.push_str(&format!("\"{}\"", escape(n)));
    }
    if s.is_empty() { None } else { Some(s) }
}

/// Apply field filters as a post-pass on a list of photos. Each field check
/// returns true if the photo passes; failing photos are dropped.
/// v1.5.50 — handles negated fields (key prefixed with `!`) by inverting
/// the result.
fn passes_field_filter(p: &PhotoSummary, key: &str, value: &str) -> bool {
    let (negated, real_key) = if let Some(stripped) = key.strip_prefix('!') {
        (true, stripped)
    } else {
        (false, key)
    };
    let pass = passes_field_filter_inner(p, real_key, value);
    if negated { !pass } else { pass }
}

fn passes_field_filter_inner(p: &PhotoSummary, key: &str, value: &str) -> bool {
    let v = value.to_lowercase();
    match key {
        "tag" => p.tags.iter().any(|t| t.to_lowercase().contains(&v)),
        "folder" | "in" | "path" => p.path.to_lowercase().contains(&v),
        "year" => {
            // year:2023  or  year:2020-2023
            let dt = p.date_taken.as_deref().unwrap_or("");
            if v.contains('-') && v.split('-').count() == 2 && v.split('-').all(|s| s.len()==4 && s.chars().all(|c| c.is_ascii_digit())) {
                let mut it = v.splitn(2, '-');
                let lo: i32 = it.next().unwrap().parse().unwrap_or(0);
                let hi: i32 = it.next().unwrap().parse().unwrap_or(9999);
                let y: i32 = dt.get(..4).and_then(|s| s.parse().ok()).unwrap_or(0);
                y >= lo && y <= hi
            } else if v.len() == 4 {
                dt.starts_with(&v)
            } else {
                false
            }
        }
        "before" | "to" => {
            let dt = p.date_taken.as_deref().unwrap_or("");
            normalize_date_for_compare(dt) <= normalize_date_for_compare(&v) || dt < v.as_str()
        }
        "after" | "from" => {
            let dt = p.date_taken.as_deref().unwrap_or("");
            normalize_date_for_compare(dt) >= normalize_date_for_compare(&v) || dt > v.as_str()
        }
        "stars" | "rating" => {
            // rating:3   rating:>=4   rating:5
            // (PhotoSummary.rating is a plain i32; no Option to unwrap.)
            if let Some(rest) = v.strip_prefix(">=") { rest.parse::<i32>().ok().map_or(false, |n| p.rating >= n) }
            else if let Some(rest) = v.strip_prefix("<=") { rest.parse::<i32>().ok().map_or(false, |n| p.rating <= n) }
            else if let Some(rest) = v.strip_prefix('>') { rest.parse::<i32>().ok().map_or(false, |n| p.rating > n) }
            else if let Some(rest) = v.strip_prefix('<') { rest.parse::<i32>().ok().map_or(false, |n| p.rating < n) }
            else { v.parse::<i32>().ok().map_or(false, |n| p.rating == n) }
        }
        "fav" | "favorite" => {
            let truthy = matches!(v.as_str(), "1" | "true" | "yes" | "y" | "on");
            p.favorite == truthy
        }
        "media" | "type" => p.media_type.eq_ignore_ascii_case(value),
        "ext" => p.filename.to_lowercase().ends_with(&format!(".{}", v)),
        // person / camera / lens / location — these aren't on PhotoSummary
        // directly; they're handled by extra DB queries before the field
        // post-filter and merged into the result set, so we accept-all here
        // (the joined result already implies the filter).
        _ => true,
    }
}

/// Try to normalise a date string to "YYYY-MM-DD" for lexicographic compare.
fn normalize_date_for_compare(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() { return String::new(); }
    // accept YYYY, YYYY-MM, YYYY-MM-DD; pad year-only to "YYYY-12-31" so
    // before:2020 means "before end of 2020" and after:2020 means "from
    // 2020-01-01 onwards".
    let mut out = s.replace(':', "-").replace('/', "-");
    out.truncate(10);
    out
}

/// Score a result by overlap with the must_terms. Used to sort results so
/// photos hitting multiple search terms float to the top (Google-style
/// relevance). Phrases count as a match; field filters don't (they were
/// already filtered before scoring).
/// v1.5.50 — must_terms is now Vec<Vec<String>> (each inner Vec is an
/// OR-group of variants); we score by the BEST match in the group.
/// Position-aware: matches at the start of a tag (or as the first
/// few tags on the photo) score higher because the AI tagger emits
/// principal subjects first.
fn relevance_score(p: &PhotoSummary, q: &ParsedQuery) -> i32 {
    let lower_path = p.path.to_lowercase();
    let lower_tags: Vec<String> = p.tags.iter().map(|t| t.to_lowercase()).collect();
    let mut score = 0i32;
    // Each must_terms group: best score among its variants wins. Multiple
    // groups stack additively (a photo matching both "cat" and "beach"
    // outranks one with just "cat").
    for group in &q.must_terms {
        let mut best = 0i32;
        for variant in group {
            let lt = variant.to_lowercase();
            // Position bonus: matches in the FIRST 3 tags get +2.
            let position_bonus = lower_tags.iter().take(3).any(|t| t == &lt) as i32 * 2;
            let s = if lower_tags.iter().any(|t| t == &lt) { 6 + position_bonus }
                else if lower_tags.iter().any(|t| t.starts_with(&lt)) { 5 }
                else if lower_tags.iter().any(|t| t.contains(&lt)) { 4 }
                else if lower_path.contains(&lt) { 1 }
                else { 0 };
            if s > best { best = s; }
        }
        score += best;
    }
    for ph in &q.phrases {
        let lt = ph.to_lowercase();
        if lower_tags.iter().any(|t| t == &lt) { score += 8; }
        else if lower_tags.iter().any(|t| t.contains(&lt)) { score += 5; }
        else if lower_path.contains(&lt) { score += 1; }
    }
    // Lots of tags = better-described photo, slight tiebreaker boost.
    score += (p.tag_count.min(20) / 5) as i32;
    // Recency tiebreaker via favorite/rating.
    if p.favorite { score += 1; }
    score += p.rating.max(0).min(5);
    score
}

/// Re-rank and filter search results using context tags.
/// If there are enough results WITH context matches, remove those WITHOUT.
fn rank_by_context(results: &mut Vec<PhotoSummary>, context_tags: &[String]) {
    // Score each result by context tag overlap
    let scored: Vec<(usize, &PhotoSummary)> = results.iter()
        .map(|p| {
            let score = p.tags.iter()
                .filter(|t| context_tags.iter().any(|c| t.contains(c)))
                .count();
            (score, p)
        })
        .collect();

    let with_context: usize = scored.iter().filter(|(s, _)| *s > 0).count();

    if with_context >= 2 {
        // Enough context-matched results — filter out zero-context ones
        let filtered: Vec<PhotoSummary> = scored.iter()
            .filter(|(s, _)| *s > 0)
            .map(|(_, p)| (*p).clone())
            .collect();
        *results = filtered;
    }

    // Sort by score descending
    results.sort_by(|a, b| {
        let score_a = a.tags.iter().filter(|t| context_tags.iter().any(|c| t.contains(c))).count();
        let score_b = b.tags.iter().filter(|t| context_tags.iter().any(|c| t.contains(c))).count();
        score_b.cmp(&score_a)
    });
}

/// Merge additional photo results into the main list, deduplicating by photo id.
fn merge_photo_results(main: &mut Vec<PhotoSummary>, extra: Vec<PhotoSummary>) {
    let existing: std::collections::HashSet<i64> = main.iter().map(|p| p.id).collect();
    for p in extra {
        if !existing.contains(&p.id) {
            main.push(p);
        }
    }
}

/// Expand search term with common synonyms for better recall.
/// v1.5.52 — Added Turkish ↔ English common-word groups so the user
/// can search in their native language and still hit photos tagged in
/// English (and vice-versa) without waiting for the AI translator. Also
/// folds in synonym groups for foods, locations, weather, etc.
fn expand_synonyms(term: &str) -> Vec<String> {
    let lower = term.to_lowercase();
    let groups: &[&[&str]] = &[
        // ── People ────────────────────────────────────────────────────
        &["woman", "women", "female", "lady", "girl",
            "kadın", "kadin", "kız", "kiz", "bayan"],
        &["man", "men", "male", "boy", "guy",
            "adam", "erkek", "oğlan", "oglan", "delikanlı"],
        &["child", "kid", "children", "baby", "toddler", "infant",
            "çocuk", "cocuk", "bebek", "yavru"],
        &["family", "aile", "ailem", "akraba"],
        &["friend", "friends", "arkadaş", "arkadas", "dost"],
        &["couple", "romantic", "love", "together", "pair",
            "çift", "cift", "aşk", "ask", "sevgili"],
        &["wedding", "bride", "groom", "düğün", "dugun", "gelin", "damat", "nikah"],
        &["birthday", "doğumgünü", "dogumgunu", "doğum-günü"],
        // ── Animals ───────────────────────────────────────────────────
        &["dog", "puppy", "canine", "köpek", "kopek", "yavru-köpek"],
        &["cat", "kitten", "feline", "kedi", "yavru-kedi", "pisi"],
        &["bird", "kuş", "kus"],
        &["horse", "at", "tay"],
        &["fish", "balık", "balik"],
        // ── Food & drink ──────────────────────────────────────────────
        &["food", "meal", "dish", "cuisine", "dinner", "lunch", "breakfast",
            "yemek", "kahvaltı", "kahvalti", "öğle", "ogle", "akşam-yemeği"],
        &["coffee", "kahve", "espresso", "cappuccino", "latte"],
        &["tea", "çay", "cay"],
        &["cake", "pastry", "dessert", "pasta", "tatlı", "tatli", "kek"],
        &["bread", "loaf", "ekmek", "somun"],
        &["fruit", "meyve", "meyvalar"],
        &["vegetable", "vegetables", "veggie", "sebze", "sebzeler"],
        &["cup", "mug", "glass", "tumbler", "goblet", "teacup",
            "bardak", "fincan", "kupa"],
        &["plate", "dish", "bowl", "tray", "tabak", "kase"],
        // ── Vehicles ──────────────────────────────────────────────────
        &["car", "vehicle", "automobile", "auto", "araba", "otomobil", "araç", "arac"],
        &["motorcycle", "bike", "motosiklet", "motor"],
        &["bicycle", "bike", "bisiklet"],
        &["airplane", "plane", "uçak", "ucak"],
        &["boat", "ship", "tekne", "gemi", "yat"],
        // ── Places & buildings ────────────────────────────────────────
        &["house", "home", "building", "residence", "ev", "yuva", "konak", "bina"],
        &["street", "road", "avenue", "path", "alley",
            "sokak", "yol", "cadde", "patika"],
        &["beach", "shore", "coast", "plaj", "kumsal", "sahil"],
        &["ocean", "sea", "water", "deniz", "okyanus", "su"],
        &["mountain", "hill", "peak", "summit", "dağ", "dag", "tepe"],
        &["tree", "forest", "woods", "jungle", "ağaç", "agac", "orman"],
        &["flower", "blossom", "bloom", "floral", "çiçek", "cicek"],
        &["church", "cathedral", "chapel", "basilica", "kilise"],
        &["mosque", "minaret", "dome", "cami", "camii", "minare"],
        &["bridge", "overpass", "viaduct", "köprü", "kopru"],
        &["park", "garden", "park", "bahçe", "bahce"],
        &["restaurant", "cafe", "diner", "lokanta", "restoran", "kafe"],
        &["museum", "müze", "muze"],
        &["beach", "kumsal", "plaj", "sahil"],
        // ── Adjectives & moods ────────────────────────────────────────
        &["happy", "smiling", "joyful", "cheerful", "laughing",
            "mutlu", "gülen", "gulen", "neşeli", "neseli"],
        &["sad", "crying", "unhappy", "melancholy",
            "üzgün", "uzgun", "ağlayan", "aglayan"],
        &["beautiful", "pretty", "gorgeous", "stunning",
            "güzel", "guzel", "şirin", "sirin"],
        &["old", "elderly", "aged", "senior", "ancient",
            "yaşlı", "yasli", "ihtiyar", "eski"],
        &["young", "youthful", "teen", "teenager", "genç", "genc"],
        &["big", "large", "huge", "giant", "massive", "büyük", "buyuk", "kocaman"],
        &["small", "little", "tiny", "miniature", "küçük", "kucuk", "minik"],
        // ── Weather & time ────────────────────────────────────────────
        &["night", "dark", "evening", "nighttime", "gece", "akşam", "aksam", "karanlık"],
        &["day", "morning", "daytime", "gündüz", "gunduz", "sabah"],
        &["rain", "rainy", "drizzle", "storm", "wet", "yağmur", "yagmur", "fırtına"],
        &["snow", "snowy", "winter", "frost", "ice",
            "kar", "karlı", "karli", "kış", "kis", "buz"],
        &["sun", "sunny", "güneş", "gunes", "güneşli"],
        &["sunset", "sunrise", "dawn", "dusk", "twilight",
            "günbatımı", "gunbatimi", "gündoğumu", "gundogumu", "alacakaranlık"],
        &["cloud", "clouds", "cloudy", "bulut", "bulutlar", "bulutlu"],
        // ── Activities ────────────────────────────────────────────────
        &["phone", "telephone", "cellphone", "mobile",
            "telefon", "cep-telefonu"],
        &["book", "reading", "kitap", "okuma"],
        &["music", "musician", "concert", "müzik", "muzik", "konser", "şarkı", "sarki"],
        &["dance", "dancing", "dans", "dansçı"],
        &["sport", "sports", "athletic", "spor", "atlet"],
        &["travel", "trip", "vacation", "holiday",
            "seyahat", "tatil", "yolculuk", "gezi"],
        &["selfie", "self-portrait", "öz-çekim", "selfi"],
        &["portrait", "portre"],
    ];
    for group in groups {
        if group.contains(&lower.as_str()) {
            return group.iter().map(|s| s.to_string()).collect();
        }
    }
    // Also try the accent-stripped form so "agac" finds the "ağaç" group.
    let stripped = strip_tr_accents(&lower);
    if stripped != lower {
        for group in groups {
            if group.contains(&stripped.as_str()) {
                return group.iter().map(|s| s.to_string()).collect();
            }
        }
    }
    vec![lower]
}

// ── Thumbnail ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_thumbnail(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let (path, hash) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_photo_path_and_hash(&conn, photo_id).map_err(|e| e.to_string())?
    };

    let thumbs_dir = state.thumbnails_dir.clone();
    let db_arc = state.db.clone();

    tokio::task::spawn_blocking(move || {
        let b64 = thumbnail::get_or_create_thumbnail(&path, &hash, &thumbs_dir, 256)
            .map_err(|e| e.to_string())?;

        // Persist thumbnail path
        let cache_name = thumbnail::thumb_cache_name(&hash);
        let thumb_path = thumbs_dir.join(&cache_name);
        if let Ok(conn) = db_arc.lock() {
            db::update_thumbnail_path(&conn, photo_id, &thumb_path.to_string_lossy()).ok();
        }

        Ok(b64)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Faster variant of `get_thumbnail` that returns the thumbnail's on-disk
/// path instead of a base64-encoded JPEG. The frontend then uses Tauri's
/// `convertFileSrc()` to produce an `asset://` URL and set `<img src>`
/// directly — bypassing the IPC base64 round-trip entirely.
///
/// On a 50k-photo library this cuts thumbnail-visible latency dramatically:
/// - Old path: disk read → base64 encode (~33% size inflation) → JSON-encode
///   → IPC → JSON-decode → data URL → image decode. Each call is a full
///   round-trip per photo.
/// - New path: one small string (the path) over IPC; all subsequent loads
///   stream straight off disk via Tauri's asset protocol.
///
/// Triggers thumbnail creation on first request (same as `get_thumbnail`),
/// so callers don't need to pre-check existence.
#[tauri::command]
pub async fn get_thumbnail_path(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let (path, hash) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_photo_path_and_hash(&conn, photo_id).map_err(|e| e.to_string())?
    };

    let thumbs_dir = state.thumbnails_dir.clone();
    let db_arc = state.db.clone();

    tokio::task::spawn_blocking(move || {
        let cache_name = thumbnail::thumb_cache_name(&hash);
        let thumb_path = thumbs_dir.join(&cache_name);

        // If cache already exists, skip the decode entirely — just hand back
        // the path. This is the hot path once a library has been scanned.
        if !thumb_path.exists() {
            thumbnail::get_or_create_thumbnail(&path, &hash, &thumbs_dir, 256)
                .map_err(|e| e.to_string())?;
            if let Ok(conn) = db_arc.lock() {
                db::update_thumbnail_path(&conn, photo_id, &thumb_path.to_string_lossy()).ok();
            }
        }

        Ok(thumb_path.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Clear all cached thumbnails so they get regenerated with correct EXIF orientation.
#[tauri::command]
pub async fn regenerate_thumbnails(
    state: tauri::State<'_, AppState>,
) -> Result<u32, String> {
    let dir = state.thumbnails_dir.clone();
    tokio::task::spawn_blocking(move || {
        let mut count = 0u32;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("jpg") {
                    if std::fs::remove_file(&p).is_ok() {
                        count += 1;
                    }
                }
            }
        }
        Ok(count)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Smart fix for rotated/sideways photos. Instead of nuking ALL thumbnails
/// (slow for 50k+ libraries), this only invalidates thumbnails whose source
/// photo has a non-trivial EXIF Orientation tag (2–8). Next time they're
/// requested, they regenerate with the correct rotation applied.
///
/// Returns (checked, fixed).
#[tauri::command]
pub async fn fix_sideways_thumbnails(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    use tauri::Emitter;
    let thumbs_dir = state.thumbnails_dir.clone();

    // Pull path+hash for every photo so we can map thumbnail filename → source.
    let rows: Vec<(i64, String, String)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let mut stmt = conn
            .prepare("SELECT id, path, hash FROM photos WHERE hash IS NOT NULL")
            .map_err(|e| e.to_string())?;
        let v: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        v
    };

    let total = rows.len();
    let result = tokio::task::spawn_blocking(move || {
        let mut checked = 0u32;
        let mut fixed = 0u32;
        let mut last_emit = std::time::Instant::now();

        for (_id, path, hash) in &rows {
            checked += 1;

            // Cache file uses first 24 chars of hash, per get_or_create_thumbnail.
            let cache_name = thumbnail::thumb_cache_name(&hash);
            let cache_path = thumbs_dir.join(&cache_name);
            if !cache_path.exists() {
                continue;
            }

            // Only JPEG/TIFF files carry EXIF Orientation. Skip HEIC/video
            // (their thumbnail is already rotated via WPF/ffmpeg) and RAW
            // (varies — leave those alone to avoid a ton of reads).
            let ext = std::path::Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            let worth_checking = matches!(
                ext.as_str(),
                "jpg" | "jpeg" | "jpe" | "tif" | "tiff" | "png"
            );
            if !worth_checking {
                continue;
            }

            let orientation = crate::thumbnail::get_exif_orientation(path);
            if orientation != 1 {
                if std::fs::remove_file(&cache_path).is_ok() {
                    fixed += 1;
                }
            }

            // Progress ping every ~500ms.
            if last_emit.elapsed() >= std::time::Duration::from_millis(500) {
                app_handle
                    .emit(
                        "thumb-fix-progress",
                        serde_json::json!({
                            "checked": checked,
                            "fixed": fixed,
                            "total": total,
                        }),
                    )
                    .ok();
                last_emit = std::time::Instant::now();
            }
        }

        (checked, fixed)
    })
    .await
    .map_err(|e| e.to_string())?;

    let (checked, fixed) = result;
    eprintln!("[thumb-fix] checked {} / fixed {}", checked, fixed);
    Ok(serde_json::json!({
        "checked": checked,
        "fixed": fixed,
        "total": total,
    }))
}

/// Return the full-resolution photo as a base64-encoded JPEG for the lightbox.
/// Resizes to max 2560px on the longest side to keep memory manageable.
#[tauri::command]
pub async fn get_photo_full(photo_id: i64, state: tauri::State<'_, AppState>) -> Result<String, String> {
    let path = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_photo_path_and_hash(&conn, photo_id)
            .map_err(|e| e.to_string())
            .map(|(p, _)| p)?
    };
    tokio::task::spawn_blocking(move || {
        let img = crate::thumbnail::open_image(&path).map_err(|e| format!("open image: {e}"))?;
        // Resize to max 2560 on the longest side
        let (w, h) = (img.width(), img.height());
        let max = 2560u32;
        let img = if w > max || h > max {
            img.resize(max, max, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Jpeg)
            .map_err(|e| format!("encode: {e}"))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(buf.into_inner()))
    })
    .await
    .map_err(|e| e.to_string())?
}

// ── AI Tagging ────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn start_tagging(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    if state.tag_running.swap(true, Ordering::SeqCst) {
        return Err("Tagging already in progress".into());
    }

    let db_arc = state.db.clone();
    let tag_running = state.tag_running.clone();
    let stop_flag = state.tag_stop.clone();
    stop_flag.store(false, Ordering::SeqCst);

    tokio::spawn(async move {
        // Auto-reset error photos to pending before starting
        {
            let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
            let _ = db::reset_error_photos(&conn);
        }

        let result = crate::tagger::run_tagging(db_arc.clone(), stop_flag, app_handle.clone()).await;

        tag_running.store(false, Ordering::SeqCst);

        // Free Ollama model from VRAM after tagging
        let endpoint = {
            let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
            db::get_setting(&conn, "ollama_endpoint").ok().flatten()
                .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string())
        };
        let model = {
            let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
            db::get_setting(&conn, "model_local").ok().flatten()
                .unwrap_or_else(|| "gemma3:4b".to_string())
        };
        let enabled_local = {
            let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
            db::get_setting(&conn, "enabled_local").ok().flatten()
                .map(|v| v == "true" || v == "1").unwrap_or(false)
        };
        if enabled_local {
            let _ = reqwest::Client::new()
                .post(format!("{}/api/chat", endpoint.trim_end_matches('/')))
                .json(&serde_json::json!({
                    "model": model,
                    "keep_alive": 0,
                    "messages": []
                }))
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await;
        }

        app_handle.emit("tag-complete", &result).ok();

        // v1.5.56 — Auto-write XMP sidecars after tagging completes (or
        // is stopped). Setting `auto_xmp_after_tag` defaults ON; users
        // can disable from Export modal. Runs in the SAME background
        // task — already off the UI thread, so a slow 60k-photo write
        // doesn't block anything else. Errors are logged to stderr,
        // never bubble up to UI (best-effort sync).
        let auto_xmp = {
            let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
            db::get_setting(&conn, "auto_xmp_after_tag").ok().flatten()
                .map(|v| v.eq_ignore_ascii_case("false") || v == "0" || v.eq_ignore_ascii_case("off"))
                .map(|disabled| !disabled) // invert: setting "false" => off
                .unwrap_or(true)
        };
        if auto_xmp {
            app_handle.emit("xmp-auto-write-start", ()).ok();
            let written = run_write_xmp_all_inline(&db_arc).unwrap_or(0);
            app_handle.emit("xmp-auto-write-complete", written).ok();
        }
    });

    Ok(())
}

/// v1.5.56 — Pure-blocking variant of write_xmp_all the auto-after-tag
/// hook can call from inside its existing tokio::spawn task without
/// having to go back through the Tauri command machinery (which would
/// require a State handle we don't have at that scope).
fn run_write_xmp_all_inline(db_arc: &std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>) -> Result<usize, String> {
    let all_xmp: Vec<xmp::XmpData> = {
        let conn = db_arc.lock().map_err(|_| "db lock")?;
        let photo_rows: Vec<(i64, String, u32, u32, i32, bool, Option<String>, Option<String>)> = {
            let mut s = conn.prepare(
                "SELECT id, path, COALESCE(width,0), COALESCE(height,0),
                        COALESCE(rating,0), COALESCE(favorite,0),
                        description, estimated_location
                 FROM photos"
            ).map_err(|e| e.to_string())?;
            let v: Vec<(i64, String, u32, u32, i32, bool, Option<String>, Option<String>)> =
                s.query_map([], |r| Ok((
                    r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?,
                    r.get(4)?,
                    { let fav: i32 = r.get(5)?; fav != 0 },
                    r.get(6)?, r.get(7)?,
                ))).map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            v
        };
        let mut result = Vec::with_capacity(photo_rows.len());
        for (id, path, width, height, rating, favorite, description, location) in photo_rows {
            // v1.5.56 — Bind .collect() to a local before letting the
            // prepare_cached statement drop. Inline collect at end of
            // block tripped E0597 (temporary outlives the prepare's
            // lifetime tied to `conn`).
            let tags: Vec<String> = {
                let mut s = conn.prepare_cached(
                    "SELECT tag FROM tags WHERE photo_id = ?1"
                ).map_err(|e| e.to_string())?;
                let v: Vec<String> = s.query_map(rusqlite::params![id], |r| r.get(0))
                    .map_err(|e| e.to_string())?
                    .filter_map(|r| r.ok())
                    .collect();
                v
            };
            // Skip photos with no tags AND no rating/fav/description: nothing
            // to write yet, no point creating an empty .xmp.
            if tags.is_empty() && rating == 0 && !favorite && description.is_none() {
                continue;
            }
            let (w_f, h_f) = (width as f32, height as f32);
            let faces: Vec<xmp::XmpFace> = if width > 0 && height > 0 {
                let raw: Vec<(i32,i32,i32,i32,String)> = {
                    let mut s = conn.prepare_cached(
                        "SELECT fr.x1, fr.y1, fr.x2, fr.y2, p.name
                         FROM face_regions fr
                         JOIN persons p ON fr.person_id = p.id
                         WHERE fr.photo_id = ?1 AND fr.person_id > 0"
                    ).map_err(|e| e.to_string())?;
                    let v: Vec<(i32,i32,i32,i32,String)> = s.query_map(rusqlite::params![id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                    }).map_err(|e| e.to_string())?.filter_map(|r| r.ok()).collect();
                    v
                };
                raw.into_iter().map(|(x1, y1, x2, y2, name)| xmp::XmpFace {
                    name,
                    cx: ((x1 + x2) as f32 / 2.0) / w_f,
                    cy: ((y1 + y2) as f32 / 2.0) / h_f,
                    w:  (x2 - x1) as f32 / w_f,
                    h:  (y2 - y1) as f32 / h_f,
                }).collect()
            } else { Vec::new() };
            result.push(xmp::XmpData {
                photo_path: path,
                tags, rating, favorite,
                description, location,
                img_width: width, img_height: height,
                faces,
            });
        }
        result
    };
    // v1.5.58/59 — Two flags drive how metadata is persisted:
    //   embed_xmp_in_jpeg: write tags INSIDE the JPEG (APP1 segment)
    //   skip_sidecar    : skip the .xmp sidecar file
    // Sane combos:
    //   default          → sidecar only       (clutter, but safest)
    //   embed=true       → sidecar + embed    (belt + suspenders)
    //   embed=true,
    //   skip_sidecar=true→ embed only         (no clutter, tags travel)
    //   skip_sidecar=true→ NOTHING            (DB-only — discouraged)
    let (embed_into_jpeg, skip_sidecar) = {
        let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
        let embed = db::get_setting(&conn, "embed_xmp_in_jpeg").ok().flatten()
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1" || v.eq_ignore_ascii_case("on"))
            .unwrap_or(false);
        let skip = db::get_setting(&conn, "skip_sidecar").ok().flatten()
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1" || v.eq_ignore_ascii_case("on"))
            .unwrap_or(false);
        (embed, skip)
    };
    let mut written = 0usize;
    for data in &all_xmp {
        if !skip_sidecar {
            if xmp::write_xmp_full(data).is_ok() { written += 1; }
        }
        if embed_into_jpeg {
            // Best-effort; ignore errors (eg. PNG/HEIC where we don't
            // embed yet, or files held open by another process).
            let xmp_str = xmp::build_xmp_string(data);
            if xmp::embed_xmp_in_jpeg(&data.photo_path, &xmp_str).is_ok() && skip_sidecar {
                written += 1;
            }
        }
    }
    Ok(written)
}

/// v1.5.59 — Remove every `.xmp` sidecar in a given folder tree (or
/// across the whole library if folder is None). Useful for users who
/// switched to embedded XMP and want to clean the existing sidecar
/// clutter without touching the original photos. Returns the number
/// of files actually deleted.
#[tauri::command]
pub async fn delete_all_xmp_sidecars(
    folder: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    // Determine roots: explicit folder or every distinct photo folder
    // in the DB. Walking the DB-known folders is faster than a full
    // disk crawl and avoids touching paths the user never imported.
    let folders: Vec<String> = if let Some(f) = folder.as_ref().filter(|s| !s.trim().is_empty()) {
        vec![f.clone()]
    } else {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let mut s = conn.prepare("SELECT DISTINCT folder FROM photos WHERE folder IS NOT NULL")
            .map_err(|e| e.to_string())?;
        let v: Vec<String> = s.query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        v
    };

    // v1.5.146 — Mac-audit follow-up. Was walking every photo folder
    // with .exists() + read_dir() + remove_file() directly on the
    // tokio worker. With a few hundred folders on an SMB share, the
    // syscalls can take minutes — and any one slow folder stalled the
    // IPC mutex until it completed. Also apply the v1.5.144 network-
    // path skip: if a folder lives on an unreachable share, don't
    // pretend we can't find XMP files there; we just defer (the user
    // can retry once the share is up).
    let deleted = tauri::async_runtime::spawn_blocking(move || -> usize {
        let mut share_status: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
        let mut deleted = 0usize;
        for folder in &folders {
            // Skip vault-unreachable shares so a momentary outage
            // doesn't make the user think their XMPs got cleaned up.
            if is_network_path(folder) {
                let root = share_root_of(folder);
                let reachable = *share_status.entry(root.clone())
                    .or_insert_with(|| std::fs::metadata(&root).is_ok());
                if !reachable { continue; }
            }
            let p = std::path::Path::new(folder);
            if !p.exists() { continue; }
            let rd = match std::fs::read_dir(p) { Ok(r) => r, Err(_) => continue };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("xmp")).unwrap_or(false) {
                    if std::fs::remove_file(&path).is_ok() {
                        deleted += 1;
                    }
                }
            }
        }
        deleted
    })
    .await
    .map_err(|e| format!("join error: {}", e))?;
    Ok(deleted)
}

#[tauri::command]
pub async fn stop_tagging(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.tag_stop.store(true, Ordering::SeqCst);
    Ok(())
}

// ── Tag edits ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn add_tag(
    photo_id: i64,
    tag: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::add_manual_tag(&conn, photo_id, &tag).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn remove_tag(
    photo_id: i64,
    tag: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::delete_tag(&conn, photo_id, &tag).map_err(|e| e.to_string())
}

// ── Timeline ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_photos_timeline(
    offset: i64,
    limit: i64,
    folder: Option<String>,
    year_month: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<TimelineGroup>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let year_month_ref = year_month.as_deref().filter(|s| !s.trim().is_empty());
    let groups = db::get_photos_timeline(&conn, offset, limit, folder.as_deref(), year_month_ref)
        .map_err(|e| e.to_string())?;
    Ok(groups.into_iter().map(|(date, photos)| TimelineGroup { date, photos }).collect())
}

/// Returns (YYYY-MM, count) buckets for every month that contains photos.
/// Used by the aperture-dial timeline UI to build year/month pickers.
#[tauri::command]
pub async fn get_timeline_buckets(
    folder: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<(String, i64)>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let (sql, has_folder) = match folder.as_deref() {
        Some(f) if !f.trim().is_empty() => (
            "SELECT COALESCE(SUBSTR(date_taken,1,7), SUBSTR(created_at,1,7)) AS m, COUNT(*)
             FROM photos WHERE folder = ?1 GROUP BY m ORDER BY m ASC".to_string(),
            Some(f.to_string()),
        ),
        _ => (
            "SELECT COALESCE(SUBSTR(date_taken,1,7), SUBSTR(created_at,1,7)) AS m, COUNT(*)
             FROM photos GROUP BY m ORDER BY m ASC".to_string(),
            None,
        ),
    };
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows: Vec<(String, i64)> = if let Some(f) = has_folder {
        stmt.query_map(rusqlite::params![f], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok()).collect()
    } else {
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok()).collect()
    };
    Ok(rows)
}

/// v1.5.47 — Manually set a photo's date_taken. The auto-extractor can
/// only return what's in EXIF / file metadata; for re-saved or scanned
/// photos where the original capture date is lost the user needs an
/// escape hatch. Frontend exposes this as a click-to-edit pencil on the
/// "Date Taken" row in the detail panel. Accepts either "YYYY-MM-DD" or
/// the full "YYYY-MM-DD HH:MM:SS" form; "YYYY" alone gets normalised to
/// Jan 1 of that year so users can quickly classify a vague memory like
/// "this is from 2008" without having to look up a specific day.
#[tauri::command]
pub async fn set_photo_date_taken(
    photo_id: i64,
    date: String,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let raw = date.trim().to_string();
    if raw.is_empty() {
        return Err("date must not be empty".into());
    }
    let normalised = if raw.len() == 4 && raw.chars().all(|c| c.is_ascii_digit()) {
        format!("{}-01-01 12:00:00", raw)
    } else if raw.len() == 7 {
        // YYYY-MM
        format!("{}-01 12:00:00", raw)
    } else if raw.len() == 10 {
        // YYYY-MM-DD
        format!("{} 12:00:00", raw)
    } else {
        raw.clone()
    };
    // Sanity check: must parse as a known date format.
    use chrono::NaiveDateTime;
    const FORMATS: &[&str] = &[
        "%Y-%m-%d %H:%M:%S",
        "%Y:%m:%d %H:%M:%S",
        "%Y/%m/%d %H:%M:%S",
        "%d/%m/%Y %H:%M:%S",
        "%d/%m/%Y %H:%M",
        "%Y-%m-%d %H:%M",
    ];
    let parsed = FORMATS.iter()
        .find_map(|f| NaiveDateTime::parse_from_str(&normalised, f).ok())
        .ok_or_else(|| format!("Could not parse date: {}", raw))?;
    let stored = parsed.format("%Y-%m-%d %H:%M:%S").to_string();
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::update_photo_date_taken(&conn, photo_id, &stored)
        .map_err(|e| e.to_string())?;
    Ok(stored)
}

#[tauri::command]
pub async fn backfill_dates(
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let photos = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_photos_without_date(&conn).map_err(|e| e.to_string())?
    };
    let db_arc = state.db.clone();
    let count = tokio::task::spawn_blocking(move || {
        let mut updated = 0usize;
        for (id, path) in &photos {
            let date = crate::exif_reader::read_exif(path)
                .ok().and_then(|e| e.date_taken)
                .or_else(|| {
                    std::fs::metadata(path).ok().and_then(|m| {
                        m.created().or_else(|_| m.modified()).ok().map(|t| {
                            let dt: chrono::DateTime<chrono::Local> = t.into();
                            dt.format("%Y-%m-%d %H:%M:%S").to_string()
                        })
                    })
                });
            if let Some(d) = date {
                if let Ok(conn) = db_arc.lock() {
                    db::update_photo_date_taken(&conn, *id, &d).ok();
                    updated += 1;
                }
            }
        }
        updated
    }).await.map_err(|e| e.to_string())?;
    Ok(count)
}

#[tauri::command]
pub async fn check_ffmpeg() -> Result<bool, String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new("ffmpeg")
            .arg("-version")
            .creation_flags(0x08000000)
            .output();
        Ok(output.map(|o| o.status.success()).unwrap_or(false))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let output = std::process::Command::new("ffmpeg").arg("-version").output();
        Ok(output.map(|o| o.status.success()).unwrap_or(false))
    }
}

// ── Settings ──────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_settings(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<(String, String)>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_all_settings(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn save_setting(
    key: String,
    value: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    if value.is_empty() {
        db::delete_setting(&conn, &key).map_err(|e| e.to_string())
    } else {
        db::set_setting(&conn, &key, &value).map_err(|e| e.to_string())
    }
}

// ── Manual location edit ─────────────────────────────────────────────────────

#[tauri::command]
pub async fn set_estimated_location(
    photo_id: i64,
    name: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::set_location_name(&conn, photo_id, &name).map_err(|e| e.to_string())
}

// ── Provider status ──────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_provider_statuses(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ProviderStatus>, String> {
    let (settings_list, usage_stats) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let settings = db::get_all_settings(&conn).unwrap_or_default();
        let mut usage = std::collections::HashMap::new();
        for provider in AiProvider::all() {
            let stats = db::get_provider_stats(&conn, provider.key_name()).unwrap_or((0, 0, 0.0));
            usage.insert(provider.key_name(), stats);
        }
        (settings, usage)
    };

    let settings: std::collections::HashMap<String, String> =
        settings_list.into_iter().collect();

    let mut statuses = vec![];

    for provider in AiProvider::all() {
        let (has_key, enabled) = if *provider == AiProvider::Local {
            // Local: "has_key" means Ollama is configured/enabled
            let enabled = settings
                .get("enabled_local")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false);
            (enabled, enabled)
        } else {
            let key = settings.get(&format!("api_key_{}", provider.key_name()));
            let has_key = key.map(|k| !k.is_empty()).unwrap_or(false);
            let enabled = settings
                .get(&format!("enabled_{}", provider.key_name()))
                .map(|v| v == "true" || v == "1")
                .unwrap_or(has_key);
            (has_key, enabled)
        };

        let model = settings
            .get(&format!("model_{}", provider.key_name()))
            .cloned()
            .unwrap_or_else(|| provider.default_model().to_string());

        let (total_tagged, total_errors, total_cost) =
            usage_stats.get(provider.key_name()).copied().unwrap_or((0, 0, 0.0));

        statuses.push(ProviderStatus {
            provider: *provider,
            name: provider.name().to_string(),
            has_key,
            enabled,
            model,
            cost_per_image: provider.cost_per_image(),
            total_tagged,
            total_errors,
            total_cost_usd: total_cost,
        });
    }

    Ok(statuses)
}

// ── Ollama health check ───────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct OllamaStatus {
    pub running: bool,
    pub model_loaded: bool,
    pub models: Vec<String>,
    pub message: String,
}

#[tauri::command]
pub async fn check_ollama(state: tauri::State<'_, AppState>) -> Result<OllamaStatus, String> {
    get_ollama_status(state).await
}

// ── Tag autocomplete ─────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_all_tags(
    prefix: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<(String, i64)>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_all_tags(&conn, prefix.as_deref()).map_err(|e| e.to_string())
}

/// Tags that most often appear alongside `tag`. Used by the detail panel to
/// suggest related tags ("users who tagged this photo 'beach' also tagged
/// 'sunset', 'ocean', 'vacation'"). Returns (tag, co_occurrence_count) pairs
/// sorted by count desc. Excludes the query tag itself.
#[tauri::command]
pub async fn get_related_tags(
    tag: String,
    limit: Option<i64>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<(String, i64)>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_related_tags(&conn, &tag, limit.unwrap_or(10)).map_err(|e| e.to_string())
}

// ══════════════════════════════════════════════════════════════════════════════
// NEW FEATURES
// ══════════════════════════════════════════════════════════════════════════════

// ── 1. XMP Sidecar Writing ──────────────────────────────────────────────────

#[tauri::command]
pub async fn write_xmp_for_photo(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let xmp_data: xmp::XmpData = {
        let conn = state.db.lock().map_err(|_| "db lock")?;

        // ── Core photo fields ────────────────────────────────────────────────
        let (path, width, height, rating, favorite, description, location): (
            String, u32, u32, i32, bool, Option<String>, Option<String>,
        ) = conn.query_row(
            "SELECT path, COALESCE(width,0), COALESCE(height,0),
                    COALESCE(rating,0), COALESCE(favorite,0),
                    description, estimated_location
             FROM photos WHERE id = ?1",
            rusqlite::params![photo_id],
            |r| Ok((
                r.get(0)?, r.get(1)?, r.get(2)?,
                r.get(3)?,
                { let v: i32 = r.get(4)?; v != 0 },
                r.get(5)?, r.get(6)?,
            )),
        ).map_err(|e| e.to_string())?;

        // ── Tags ─────────────────────────────────────────────────────────────
        let tags: Vec<String> = {
            let mut s = conn.prepare_cached(
                "SELECT tag FROM tags WHERE photo_id = ?1"
            ).map_err(|e| e.to_string())?;
            let v: Vec<String> = s.query_map(rusqlite::params![photo_id], |r| r.get(0))
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            v
        };

        // ── Named face regions ───────────────────────────────────────────────
        let faces: Vec<xmp::XmpFace> = if width > 0 && height > 0 {
            let mut s = conn.prepare_cached(
                "SELECT fr.x1, fr.y1, fr.x2, fr.y2, p.name
                 FROM face_regions fr
                 JOIN persons p ON fr.person_id = p.id
                 WHERE fr.photo_id = ?1 AND fr.person_id > 0"
            ).map_err(|e| e.to_string())?;
            let raw: Vec<(i32,i32,i32,i32,String)> = s.query_map(rusqlite::params![photo_id], |r| {
                let (x1, y1, x2, y2): (i32, i32, i32, i32) =
                    (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
                let name: String = r.get(4)?;
                Ok((x1, y1, x2, y2, name))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
            raw.into_iter().map(|(x1, y1, x2, y2, name)| {
                let w_f = width as f32;
                let h_f = height as f32;
                xmp::XmpFace {
                    name,
                    cx: ((x1 + x2) as f32 / 2.0) / w_f,
                    cy: ((y1 + y2) as f32 / 2.0) / h_f,
                    w:  (x2 - x1) as f32 / w_f,
                    h:  (y2 - y1) as f32 / h_f,
                }
            }).collect()
        } else {
            vec![]
        };

        xmp::XmpData {
            photo_path: path,
            tags,
            rating,
            favorite,
            description,
            location,
            img_width: width,
            img_height: height,
            faces,
        }
    };

    tokio::task::spawn_blocking(move || {
        xmp::write_xmp_full(&xmp_data).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn write_xmp_all(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<usize, String> {
    // Collect all photo data needed for XMP in one pass (no N+1 — joins do it)
    let all_xmp: Vec<xmp::XmpData> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;

        // ── All tagged photos ────────────────────────────────────────────────
        let photo_rows: Vec<(i64, String, u32, u32, i32, bool, Option<String>, Option<String>)> = {
            let mut s = conn.prepare(
                "SELECT id, path, COALESCE(width,0), COALESCE(height,0),
                        COALESCE(rating,0), COALESCE(favorite,0),
                        description, estimated_location
                 FROM photos"
            ).map_err(|e| e.to_string())?;
            let v: Vec<_> = s.query_map([], |r| Ok((
                r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?,
                r.get(4)?,
                { let fav: i32 = r.get(5)?; fav != 0 },
                r.get(6)?, r.get(7)?,
            )))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
            v
        };

        let mut result = Vec::with_capacity(photo_rows.len());
        for (id, path, width, height, rating, favorite, description, location) in photo_rows {

            // Tags
            let tags: Vec<String> = {
                let mut s = conn.prepare_cached(
                    "SELECT tag FROM tags WHERE photo_id = ?1"
                ).map_err(|e| e.to_string())?;
                let v: Vec<String> = s.query_map(rusqlite::params![id], |r| r.get(0))
                    .map_err(|e| e.to_string())?
                    .filter_map(|r| r.ok())
                    .collect();
                v
            };

            // Named faces
            let faces: Vec<xmp::XmpFace> = if width > 0 && height > 0 {
                let mut s = conn.prepare_cached(
                    "SELECT fr.x1, fr.y1, fr.x2, fr.y2, p.name
                     FROM face_regions fr
                     JOIN persons p ON fr.person_id = p.id
                     WHERE fr.photo_id = ?1 AND fr.person_id > 0"
                ).map_err(|e| e.to_string())?;
                let raw: Vec<(i32,i32,i32,i32,String)> = s.query_map(rusqlite::params![id], |r| {
                    let (x1, y1, x2, y2): (i32, i32, i32, i32) =
                        (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
                    let name: String = r.get(4)?;
                    Ok((x1, y1, x2, y2, name))
                })
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
                raw.into_iter().map(|(x1, y1, x2, y2, name)| {
                    let w_f = width as f32;
                    let h_f = height as f32;
                    xmp::XmpFace {
                        name,
                        cx: ((x1 + x2) as f32 / 2.0) / w_f,
                        cy: ((y1 + y2) as f32 / 2.0) / h_f,
                        w:  (x2 - x1) as f32 / w_f,
                        h:  (y2 - y1) as f32 / h_f,
                    }
                }).collect()
            } else {
                vec![]
            };

            // Only include photos that have something worth writing
            if !tags.is_empty() || rating != 0 || favorite || description.is_some()
                || location.is_some() || !faces.is_empty()
            {
                result.push(xmp::XmpData {
                    photo_path: path,
                    tags,
                    rating,
                    favorite,
                    description,
                    location,
                    img_width: width,
                    img_height: height,
                    faces,
                });
            }
        }
        result
    };

    // v1.5.58/59 — Honour both embed and skip_sidecar settings.
    let (embed_into_jpeg, skip_sidecar) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let e = db::get_setting(&conn, "embed_xmp_in_jpeg").ok().flatten()
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1" || v.eq_ignore_ascii_case("on"))
            .unwrap_or(false);
        let s = db::get_setting(&conn, "skip_sidecar").ok().flatten()
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1" || v.eq_ignore_ascii_case("on"))
            .unwrap_or(false);
        (e, s)
    };
    let total = all_xmp.len();
    let mut success = 0usize;
    for (i, data) in all_xmp.iter().enumerate() {
        if !skip_sidecar {
            if xmp::write_xmp_full(data).is_ok() { success += 1; }
        }
        if embed_into_jpeg {
            let xmp_str = xmp::build_xmp_string(data);
            if xmp::embed_xmp_in_jpeg(&data.photo_path, &xmp_str).is_ok() && skip_sidecar {
                success += 1;
            }
        }
        if i % 50 == 0 {
            app_handle.emit("xmp-progress", serde_json::json!({"done": i+1, "total": total})).ok();
        }
    }
    Ok(success)
}

// ── 2. Export ───────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn export_data(
    format: String,
    output_path: String,
    strip_gps: Option<bool>,
    state: tauri::State<'_, AppState>,
) -> Result<ExportResult, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let scrub = strip_gps.unwrap_or(false);
    let count = match format.as_str() {
        "csv" => export::export_csv_with_options(&conn, &output_path, scrub).map_err(|e| e.to_string())?,
        "json" => export::export_json_with_options(&conn, &output_path, scrub).map_err(|e| e.to_string())?,
        "md" | "markdown" => export::export_markdown(&conn, &output_path, scrub).map_err(|e| e.to_string())?,
        _ => return Err("Unknown format. Use 'csv', 'json', or 'md'".into()),
    };
    Ok(ExportResult { path: output_path, count })
}

/// Copy every photo in `collection_id` to `dest_dir`. Preserves original
/// filenames; on collision appends `-1`, `-2`, …. Emits
/// `collection-export-progress` events (payload: {done, total, copied, skipped})
/// so the frontend can show a live count. Returns a summary object.
#[tauri::command]
pub async fn export_collection_as_folder(
    collection_id: i64,
    dest_dir: String,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    use std::path::{Path, PathBuf};
    // Validate dest.
    let dest = PathBuf::from(&dest_dir);
    if !dest.is_dir() {
        return Err(format!("Destination is not a directory: {}", dest_dir));
    }

    // Gather (path, filename) pairs under the db lock, then release it so the
    // copy loop doesn't block other DB work.
    let photo_paths: Vec<String> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let ids = db::get_collection_photo_ids(&conn, collection_id)
            .map_err(|e| e.to_string())?;
        if ids.is_empty() {
            return Err("Collection has no photos".into());
        }
        // Look up paths in bulk. rusqlite doesn't love IN(?,?,?)-bind dynamic
        // length, so loop one-by-one — this is O(N) but collection sizes are
        // small (hundreds at most).
        let mut out: Vec<String> = Vec::with_capacity(ids.len());
        for id in &ids {
            if let Ok(path) = conn.query_row(
                "SELECT path FROM photos WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get::<_, String>(0),
            ) {
                out.push(path);
            }
        }
        out
    };

    let total = photo_paths.len();
    let mut copied = 0usize;
    let mut skipped = 0usize;
    let mut errors: Vec<String> = Vec::new();
    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (i, src_str) in photo_paths.iter().enumerate() {
        let src = Path::new(src_str);
        if !src.exists() {
            skipped += 1;
            continue;
        }
        // Build a unique destination filename.
        let file_name = src.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("photo_{}.bin", i));
        let (stem, ext) = match src.extension() {
            Some(e) => {
                let s = file_name.trim_end_matches(&format!(".{}", e.to_string_lossy())).to_string();
                (s, format!(".{}", e.to_string_lossy()))
            }
            None => (file_name.clone(), String::new()),
        };
        let mut candidate = file_name.clone();
        let mut n = 1usize;
        while used_names.contains(&candidate) || dest.join(&candidate).exists() {
            candidate = format!("{}-{}{}", stem, n, ext);
            n += 1;
            if n > 9999 { break; }
        }
        used_names.insert(candidate.clone());
        let dest_path = dest.join(&candidate);
        match std::fs::copy(src, &dest_path) {
            Ok(_) => copied += 1,
            Err(e) => {
                errors.push(format!("{}: {}", src_str, e));
                skipped += 1;
            }
        }
        // Emit progress every file for small collections, throttle to every
        // 10 for larger ones.
        if total <= 50 || (i + 1) % 10 == 0 || i + 1 == total {
            let _ = app_handle.emit(
                "collection-export-progress",
                serde_json::json!({
                    "done": i + 1,
                    "total": total,
                    "copied": copied,
                    "skipped": skipped,
                }),
            );
        }
    }

    Ok(serde_json::json!({
        "total": total,
        "copied": copied,
        "skipped": skipped,
        "errors": errors.into_iter().take(20).collect::<Vec<_>>(),
        "dest_dir": dest_dir,
    }))
}

// ── 2b. Metadata snapshot (backup / restore) ────────────────────────────────
//
// Dumps every photo's user-editable metadata (tags, description, rating,
// favorite, assigned persons) to a single JSON file keyed by SHA-256 hash.
// Hash-keyed so the user can reinstall / move the library and restore their
// work as long as the actual files survive. Thumbnails, embeddings, and
// detected-but-unnamed faces are NOT exported — they can be regenerated.

/// Write a full metadata snapshot to `output_path`. Overwrites the file.
/// Returns {photos, tags, descriptions, path, bytes}.
#[tauri::command]
pub async fn export_metadata_snapshot(
    output_path: String,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    use std::io::Write;
    let conn = state.db.lock().map_err(|_| "db lock")?;

    // Pull all photos with at least one piece of user data attached — skip
    // completely un-annotated rows to keep the snapshot compact.
    let mut photos_stmt = conn
        .prepare(
            "SELECT id, hash, filename, rating, favorite, COALESCE(description, '')
             FROM photos
             WHERE hash IS NOT NULL AND hash != ''",
        )
        .map_err(|e| e.to_string())?;
    let photo_rows: Vec<(i64, String, String, i64, i64, String)> = photos_stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();
    drop(photos_stmt);

    let mut tags_count = 0usize;
    let mut desc_count = 0usize;
    let mut entries: Vec<serde_json::Value> = Vec::new();

    for (pid, hash, filename, rating, favorite, desc) in &photo_rows {
        // Gather tags. Only user-meaningful fields kept.
        let tags: Vec<serde_json::Value> = {
            let mut stmt = match conn.prepare(
                "SELECT tag, confidence, COALESCE(source, '') FROM tags WHERE photo_id = ?1 ORDER BY tag",
            ) {
                Ok(s) => s,
                Err(_) => continue,
            };
            stmt.query_map(rusqlite::params![pid], |r| {
                Ok(serde_json::json!({
                    "tag": r.get::<_, String>(0)?,
                    "confidence": r.get::<_, Option<f64>>(1)?,
                    "source": r.get::<_, String>(2)?,
                }))
            })
            .ok()
            .map(|it| it.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        };
        // Persons assigned on this photo via face_regions.
        let persons: Vec<String> = {
            let mut stmt = match conn.prepare(
                "SELECT DISTINCT pe.name
                 FROM face_regions fr
                 JOIN persons pe ON pe.id = fr.person_id
                 WHERE fr.photo_id = ?1 AND fr.person_id > 0",
            ) {
                Ok(s) => s,
                Err(_) => continue,
            };
            stmt.query_map(rusqlite::params![pid], |r| r.get::<_, String>(0))
                .ok()
                .map(|it| it.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        };

        // Skip rows that have literally nothing user-authored.
        let has_data = !tags.is_empty()
            || !desc.is_empty()
            || *rating != 0
            || *favorite != 0
            || !persons.is_empty();
        if !has_data {
            continue;
        }

        tags_count += tags.len();
        if !desc.is_empty() {
            desc_count += 1;
        }

        entries.push(serde_json::json!({
            "hash": hash,
            "filename": filename,
            "rating": rating,
            "favorite": *favorite != 0,
            "description": desc,
            "tags": tags,
            "persons": persons,
        }));
    }

    let snapshot = serde_json::json!({
        "version": 1,
        "app": "retinatag",
        "created_at": chrono::Utc::now().to_rfc3339(),
        "photo_count": entries.len(),
        "photos": entries,
    });

    let text = serde_json::to_string_pretty(&snapshot).map_err(|e| e.to_string())?;
    let mut f = std::fs::File::create(&output_path).map_err(|e| e.to_string())?;
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    let bytes = text.len();

    Ok(serde_json::json!({
        "path": output_path,
        "photos": snapshot["photo_count"],
        "tags": tags_count,
        "descriptions": desc_count,
        "bytes": bytes,
    }))
}

/// Read a snapshot file and apply it to the DB. Matches rows by hash so the
/// library can have been moved / renamed as long as file contents are
/// unchanged. `dry_run = true` reports what would change without writing.
#[tauri::command]
pub async fn import_metadata_snapshot(
    input_path: String,
    dry_run: Option<bool>,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let dry_run = dry_run.unwrap_or(false);
    let raw = std::fs::read_to_string(&input_path).map_err(|e| e.to_string())?;
    let snapshot: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("Invalid JSON: {}", e))?;

    let photos = snapshot.get("photos").and_then(|v| v.as_array())
        .ok_or_else(|| "Snapshot is missing 'photos' array".to_string())?;

    let conn = state.db.lock().map_err(|_| "db lock")?;

    let mut matched = 0usize;
    let mut missing = 0usize;
    let mut tags_added = 0usize;
    let mut descs_set = 0usize;
    let mut ratings_set = 0usize;
    let mut favs_set = 0usize;
    let mut persons_created = 0usize;
    let mut faces_bound = 0usize;

    for entry in photos {
        let hash = match entry.get("hash").and_then(|v| v.as_str()) {
            Some(h) if !h.is_empty() => h,
            _ => continue,
        };
        // Find local photo id by hash. Same file may appear under multiple
        // paths — apply to all.
        let ids: Vec<i64> = {
            let mut stmt = match conn.prepare("SELECT id FROM photos WHERE hash = ?1") {
                Ok(s) => s,
                Err(_) => continue,
            };
            stmt.query_map(rusqlite::params![hash], |r| r.get::<_, i64>(0))
                .ok()
                .map(|it| it.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        };
        if ids.is_empty() {
            missing += 1;
            continue;
        }
        matched += ids.len();

        let rating = entry.get("rating").and_then(|v| v.as_i64()).unwrap_or(0);
        let favorite = entry.get("favorite").and_then(|v| v.as_bool()).unwrap_or(false);
        let desc = entry.get("description").and_then(|v| v.as_str()).unwrap_or("");
        let tags_arr = entry.get("tags").and_then(|v| v.as_array());
        let persons_arr = entry.get("persons").and_then(|v| v.as_array());

        for pid in &ids {
            // Rating / favorite — apply only if snapshot differs from zero/
            // defaults, so we never clobber local work with "empty" fields
            // the user set AFTER the backup.
            if rating > 0 && !dry_run {
                let _ = conn.execute(
                    "UPDATE photos SET rating = ?1 WHERE id = ?2",
                    rusqlite::params![rating, pid],
                );
            }
            if rating > 0 { ratings_set += 1; }

            if favorite && !dry_run {
                let _ = conn.execute(
                    "UPDATE photos SET favorite = 1 WHERE id = ?1",
                    rusqlite::params![pid],
                );
            }
            if favorite { favs_set += 1; }

            if !desc.is_empty() {
                if !dry_run {
                    let _ = db::update_photo_description(&conn, *pid, desc);
                }
                descs_set += 1;
            }

            if let Some(tags) = tags_arr {
                let mut to_insert: Vec<(String, f64, String)> = Vec::new();
                for t in tags {
                    let tag = t.get("tag").and_then(|v| v.as_str()).unwrap_or("").trim();
                    if tag.is_empty() { continue; }
                    let conf = t.get("confidence").and_then(|v| v.as_f64()).unwrap_or(1.0);
                    let src = t.get("source").and_then(|v| v.as_str()).unwrap_or("restore").to_string();
                    to_insert.push((tag.to_string(), conf, src));
                }
                if !to_insert.is_empty() {
                    tags_added += to_insert.len();
                    if !dry_run {
                        let _ = db::insert_tags(&conn, *pid, &to_insert);
                    }
                }
            }

            if let Some(persons) = persons_arr {
                for name_val in persons {
                    let name = name_val.as_str().unwrap_or("").trim();
                    if name.is_empty() { continue; }
                    let person_id = match db::find_person_by_name(&conn, name) {
                        Ok(Some(id)) => id,
                        _ => {
                            if dry_run {
                                persons_created += 1;
                                continue;
                            }
                            match db::create_person(&conn, name) {
                                Ok(id) => { persons_created += 1; id }
                                Err(_) => continue,
                            }
                        }
                    };
                    // Always drop a name tag so search works even if no face
                    // region exists.
                    if !dry_run {
                        let _ = db::insert_tags(
                            &conn, *pid,
                            &[(name.to_string(), 1.0, "face".to_string())],
                        );
                    }
                    // Bind the biggest unassigned face region on this photo,
                    // matching the batch_assign_person heuristic.
                    let face_id: Option<i64> = conn
                        .query_row(
                            "SELECT id FROM face_regions
                             WHERE photo_id = ?1
                               AND (person_id IS NULL OR person_id <= 0)
                             ORDER BY ((x2 - x1) * (y2 - y1)) DESC
                             LIMIT 1",
                            rusqlite::params![pid],
                            |r| r.get::<_, i64>(0),
                        )
                        .ok();
                    if let Some(fid) = face_id {
                        if !dry_run {
                            let _ = db::assign_face_to_person(&conn, fid, Some(person_id));
                        }
                        faces_bound += 1;
                    }
                }
            }
        }
    }

    Ok(serde_json::json!({
        "dry_run": dry_run,
        "photos_in_snapshot": photos.len(),
        "photos_matched": matched,
        "photos_missing": missing,
        "tags_added": tags_added,
        "descriptions_set": descs_set,
        "ratings_set": ratings_set,
        "favorites_set": favs_set,
        "persons_created_or_existing": persons_created,
        "faces_bound": faces_bound,
    }))
}

// ── 3. Drag & Drop scan ────────────────────────────────────────────────────

#[tauri::command]
pub async fn scan_dropped_paths(
    paths: Vec<String>,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    // Find directories among dropped paths; for individual files, scan parent
    let mut folders_to_scan: Vec<String> = Vec::new();
    for p in &paths {
        let path = std::path::Path::new(p);
        if path.is_dir() {
            folders_to_scan.push(p.clone());
        } else if path.is_file() {
            if let Some(parent) = path.parent() {
                let ps = parent.to_string_lossy().to_string();
                if !folders_to_scan.contains(&ps) {
                    folders_to_scan.push(ps);
                }
            }
        }
    }

    for folder in folders_to_scan {
        let db_arc = state.db.clone();
        let thumbs_dir = state.thumbnails_dir.clone();
        let stop_flag = state.scan_stop.clone();
        let ah = app_handle.clone();

        tokio::spawn(async move {
            crate::scanner::scan_folder_impl(folder, db_arc, thumbs_dir, stop_flag, ah).await.ok();
        });
    }
    Ok(())
}

// ── 4. Watch Folders ────────────────────────────────────────────────────────

#[tauri::command]
pub async fn add_watch_folder(
    path: String,
    auto_tag: bool,
    state: tauri::State<'_, AppState>,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::add_watch_folder(&conn, &path, auto_tag).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn remove_watch_folder(
    id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::remove_watch_folder(&conn, id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_watch_folders(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<WatchFolder>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_watch_folders(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_watch_folder_enabled(
    id: i64,
    enabled: bool,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::update_watch_folder_enabled(&conn, id, enabled).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_watch_folder_auto_tag(
    id: i64,
    auto_tag: bool,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::update_watch_folder_auto_tag(&conn, id, auto_tag).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn start_watching(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    // v1.5.145 — Third command flagged in Mac Claude's audit. The
    // notify crate's RecommendedWatcher.watch() validates the path
    // synchronously by stat()ing it. With a watched folder on an
    // unresponsive SMB share, the tokio worker that handled the JS
    // "Save settings → Start watching" invoke would hang inside
    // FolderWatcher::new(), starving the IPC mutex until the share
    // came back. Move the constructor into spawn_blocking; the
    // resulting RecommendedWatcher is Send (Windows backend uses
    // ReadDirectoryChangesW which is Send + Sync) so we can carry
    // it back across the await and stash in state.
    let (folders, auto_tag_set) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let all = db::get_watch_folders(&conn).map_err(|e| e.to_string())?;
        let folders: Vec<String> = all.iter()
            .filter(|w| w.enabled)
            .map(|w| w.path.clone())
            .collect();
        let auto_tag_set: std::collections::HashSet<String> = all.iter()
            .filter(|w| w.enabled && w.auto_tag)
            .map(|w| w.path.clone())
            .collect();
        (folders, auto_tag_set)
    };

    if folders.is_empty() {
        return Err("No watch folders configured".into());
    }

    let db_arc = state.db.clone();
    let thumbs_dir = state.thumbnails_dir.clone();
    let tag_running = state.tag_running.clone();
    let tag_stop = state.tag_stop.clone();

    let watcher = tauri::async_runtime::spawn_blocking(move || {
        crate::watcher::FolderWatcher::new(
            folders, db_arc, thumbs_dir, app_handle,
            auto_tag_set, tag_running, tag_stop,
        ).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {}", e))??;

    // Store watcher in state
    let mut guard = state.watcher.lock().map_err(|_| "watcher lock")?;
    *guard = Some(watcher);

    Ok(())
}

#[tauri::command]
pub async fn stop_watching(
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut guard = state.watcher.lock().map_err(|_| "watcher lock")?;
    *guard = None;
    Ok(())
}

// ── 4b. Device Auto-Import ──────────────────────────────────────────────────

#[tauri::command]
pub async fn start_device_monitor(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    // New behaviour: the monitor only detects drives and emits
    // `device-detected`. The frontend then asks the user where to save
    // and calls `import_from_device(drive, dest_dir)` below.
    let monitor = crate::device_monitor::DeviceMonitor::start(app_handle);

    let mut guard = state.device_monitor.lock().map_err(|_| "monitor lock")?;
    *guard = Some(monitor);

    {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::set_setting(&conn, "auto_import_enabled", "true").ok();
    }

    Ok(())
}

/// Manually re-scan removable drives *right now* and re-emit
/// `device-detected` for every one that's found. Needed because:
///  - users often plug a USB stick/SD card *before* starting RetinaTag,
///  - the auto-monitor is off unless the user turned it on in Settings,
///  - iPhones on Windows connect as MTP (portable) devices that don't get
///    a drive letter, so even an always-on monitor can't see them — the
///    caller uses the return value (0 drives found) to show a fallback
///    UI that explains this and offers "pick folder manually" instead.
#[tauri::command]
pub async fn rescan_devices(app_handle: tauri::AppHandle) -> Result<usize, String> {
    let found = crate::device_monitor::rescan_now(&app_handle);
    Ok(found)
}

// ── MTP (iPhone / Android over USB) ────────────────────────────────────────
//
// These commands wrap `crate::mtp` which talks to Windows' IPortableDevice
// COM API. On non-Windows platforms the commands compile but immediately
// return an error — we don't currently ship to macOS/Linux but the stubs
// keep the command surface consistent.

/// List all MTP/WPD devices currently connected to the PC. This is what
/// the "Cihazdan aktar → 📱 iPhone" device picker calls. Returns an empty
/// vec when no devices are connected OR when the iPhone is locked / the
/// user hasn't tapped "Trust This Computer" yet.
#[tauri::command]
pub async fn mtp_list_devices() -> Result<Vec<serde_json::Value>, String> {
    #[cfg(windows)]
    {
        let devices = tokio::task::spawn_blocking(crate::mtp::list_devices)
            .await
            .map_err(|e| format!("mtp_list_devices join: {e}"))??;
        Ok(devices
            .into_iter()
            .map(|d| serde_json::to_value(d).unwrap_or(serde_json::Value::Null))
            .collect())
    }
    #[cfg(not(windows))]
    {
        Err("MTP is only supported on Windows in this build".to_string())
    }
}

/// Enumerate all photos and videos on an MTP device. Returns counts,
/// total size and a (potentially large) list of object IDs. This is
/// slow for a full iPhone — wrapped in spawn_blocking.
#[tauri::command]
pub async fn mtp_list_media(device_id: String) -> Result<serde_json::Value, String> {
    #[cfg(windows)]
    {
        let list = tokio::task::spawn_blocking(move || crate::mtp::list_media(&device_id))
            .await
            .map_err(|e| format!("mtp_list_media join: {e}"))??;
        Ok(serde_json::to_value(list).unwrap_or(serde_json::Value::Null))
    }
    #[cfg(not(windows))]
    {
        let _ = device_id;
        Err("MTP is only supported on Windows in this build".to_string())
    }
}

/// Import media from an MTP device into `dest_dir`. If `object_ids` is
/// `None`, everything on the phone is imported; otherwise only the given
/// objects. Copies files to disk under `dest_dir/YYYY/YYYY-MM - Month/`
/// using each file's EXIF date (falling back to the MTP date), hashes +
/// dedups against the library DB, records the MTP object_id → photo_id
/// mapping in `mtp_imports` so a later "delete from phone except
/// favorites" can match them back. Emits `mtp-import-progress` events.
#[tauri::command]
pub async fn mtp_import(
    device_id: String,
    dest_dir: String,
    object_ids: Option<Vec<String>>,
    remember_dest: bool,
    // v1.5.151 — Mac-parity filters. "image" | "video" | None=all.
    filter_kind: Option<String>,
    // ISO-8601 date string (YYYY-MM-DD or full). Objects with no
    // date_created OR a date_created strictly less than this are
    // skipped before download. Saves bandwidth on big phones.
    date_from_iso: Option<String>,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    #[cfg(not(windows))]
    {
        let _ = (device_id, dest_dir, object_ids, remember_dest, filter_kind, date_from_iso, state, app_handle);
        return Err("MTP is only supported on Windows in this build".to_string());
    }
    #[cfg(windows)]
    {
        use tauri::Emitter;
        let dest_root = std::path::PathBuf::from(&dest_dir);
        std::fs::create_dir_all(&dest_root).map_err(|e| e.to_string())?;

        if remember_dest {
            let conn = state.db.lock().map_err(|_| "db lock")?;
            db::set_setting(&conn, "auto_import_dir", &dest_dir).ok();
        }

        let db_clone = state.db.clone();
        let ah = app_handle.clone();
        let device_id_clone = device_id.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<(usize, usize, usize, usize, usize), String> {
            // Decide which objects to import.
            let mut objects: Vec<crate::mtp::MtpObject> = match object_ids {
                Some(ids) => {
                    // User passed a filtered subset — we still need the
                    // full property info (name/size/date) to place files,
                    // so scan once and filter. This is slower but correct
                    // and keeps the import command uniform.
                    let all = crate::mtp::list_media(&device_id_clone)?;
                    let want: std::collections::HashSet<String> =
                        ids.into_iter().collect();
                    all.photos.into_iter().filter(|o| want.contains(&o.id)).collect()
                }
                None => crate::mtp::list_media(&device_id_clone)?.photos,
            };

            // v1.5.151 — Apply pre-download filters from the import-options
            // panel. Reduces bandwidth + time on big phones; otherwise we'd
            // download the file just to skip it after EXIF inspection.
            if let Some(kind) = filter_kind.as_deref() {
                match kind {
                    "image" => objects.retain(|o| !o.is_video),
                    "video" => objects.retain(|o|  o.is_video),
                    _ => {} // "all" or unknown — no filter
                }
            }
            if let Some(min_date) = date_from_iso.as_deref().filter(|s| !s.is_empty()) {
                // Lex-compare on ISO date prefixes — both sides are in the
                // same YYYY-MM-DD shape so the string comparison is correct.
                objects.retain(|o| match o.date_created.as_deref() {
                    Some(d) if d.len() >= 10 => &d[..10] >= &min_date[..min_date.len().min(10)],
                    _ => false, // no date → exclude when user asked for a window
                });
            }

            let total = objects.len();
            if total == 0 {
                ah.emit(
                    "mtp-import-complete",
                    serde_json::json!({
                        "device": device_id_clone,
                        "copied": 0,
                        "skipped": 0,
                        "skip_dest": 0,
                        "skip_dup": 0,
                        "skip_fail": 0,
                        "total": 0,
                    }),
                )
                .ok();
                return Ok((0, 0, 0, 0, 0));
            }

            // Open device ONCE for the whole batch so we don't pay the
            // Open() latency per file (iPhone takes ~200ms to open).
            let device = crate::mtp::open_device_for_bulk(&device_id_clone)?;

            let mut copied = 0usize;
            // v1.5.151 — Per Mac parity, split the single `skipped` counter
            // into three reason buckets so the UI can show "X already in
            // library · Y already in destination · Z failed".
            let mut skip_dest = 0usize;
            let mut skip_dup  = 0usize;
            let mut skip_fail = 0usize;
            let mut imported_ids: Vec<(String, String, i64)> = Vec::new(); // (object_id, dest_path, photo_id)

            // v1.5.153 — Temp inbox for the slow-path (WPD date missing).
            // Files land here first, then get re-bucketed using their own
            // EXIF / mtime so they never end up in `Unknown/Unknown/`.
            let inbox = dest_root.join("_inbox_tmp");
            let _ = std::fs::create_dir_all(&inbox);

            for (i, obj) in objects.iter().enumerate() {
                let filename = if obj.name.is_empty() {
                    format!("mtp_{}.bin", i)
                } else {
                    obj.name.clone()
                };

                // v1.5.153 — Two-path placement. FAST path: WPD knows the
                // capture date → place directly into the bucket so we can
                // skip the download entirely if the file is already there.
                // SLOW path: WPD has no date → download to a temp inbox,
                // then read EXIF + mtime from the file itself and pick a
                // real year/month. Either way: the file NEVER lands in
                // "Unknown/Unknown/" anymore.
                let (opt_year, opt_month_folder) = parse_mtp_date_bucket(obj.date_created.as_deref());
                let use_fast_path = opt_year != "Unknown";

                let target_dir = if use_fast_path {
                    dest_root.join(&opt_year).join(&opt_month_folder)
                } else {
                    // Slow path: download first, decide bucket later.
                    inbox.clone()
                };
                if let Err(e) = std::fs::create_dir_all(&target_dir) {
                    eprintln!("create_dir {:?}: {}", target_dir, e);
                    skip_fail += 1;
                    continue;
                }
                // For the slow path the file name in the inbox is
                // prefixed with the index so two phone-side objects with
                // the same filename don't collide during the temp stage.
                let dest_path = if use_fast_path {
                    target_dir.join(&filename)
                } else {
                    target_dir.join(format!("{}_{}", i, filename))
                };

                // Fast-path skip — if a file with this name already exists
                // at this size in the bucket, assume it's the same image
                // and skip download entirely. A real content dedup
                // happens later via hash. (Slow path can't optimize
                // this — we don't know the bucket yet.)
                if use_fast_path && dest_path.exists() {
                    if let Ok(md) = std::fs::metadata(&dest_path) {
                        if md.len() == obj.size {
                            skip_dest += 1;
                            continue;
                        }
                    }
                }

                // v1.5.151 — Patient retry around copy_object. iPhone
                // auto-locks during long imports and refuses MTP reads
                // until the user wakes + unlocks again. Old code gave
                // up on the first error; now we back off (1s, 2s, 4s,
                // 8s, then 30s × 6 = ~3 min per file budget) and emit
                // mtp-import-waiting between attempts so the UI can
                // show "iPhone locked — retry N/10". Mac shipped the
                // same schedule in v1.5.142.
                const RETRY_DELAYS_MS: [u64; 9] = [
                    1_000, 2_000, 4_000, 8_000,
                    30_000, 30_000, 30_000, 30_000, 30_000,
                ];
                let mut copy_ok = false;
                let mut last_err = String::new();
                // First attempt is free; subsequent ones pay a wait.
                for attempt in 0..=RETRY_DELAYS_MS.len() {
                    if attempt > 0 {
                        let delay_ms = RETRY_DELAYS_MS[attempt - 1];
                        ah.emit(
                            "mtp-import-waiting",
                            serde_json::json!({
                                "device": device_id_clone,
                                "filename": filename,
                                "attempt": attempt,
                                "max_attempts": RETRY_DELAYS_MS.len(),
                                "wait_ms": delay_ms,
                                "last_error": last_err.clone(),
                            }),
                        ).ok();
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        // Re-open the device — iPhone may have closed
                        // the session while locked. Cheap if still alive.
                    }
                    match unsafe { crate::mtp::copy_object_with_device(&device, &obj.id, &dest_path) } {
                        Ok(_) => { copy_ok = true; break; }
                        Err(e) => {
                            last_err = e;
                            // Clean partial file before retry so the
                            // size-match dedup at the top doesn't
                            // false-positive on the next attempt.
                            let _ = std::fs::remove_file(&dest_path);
                        }
                    }
                }
                if !copy_ok {
                    eprintln!("copy_object {} failed after retries: {}", obj.id, last_err);
                    skip_fail += 1;
                    continue;
                }

                // v1.5.153 — Slow path: file is now in _inbox_tmp. Read
                // EXIF / mtime to figure out a real year/month bucket,
                // then move into place. NEVER falls into Unknown/.
                let final_path: std::path::PathBuf = if use_fast_path {
                    dest_path.clone()
                } else {
                    let (year, month) = date_bucket_for_file(&dest_path.to_string_lossy());
                    let month_name = english_month_name(month);
                    let real_dir = dest_root
                        .join(format!("{:04}", year))
                        .join(format!("{:02}-{}", month, month_name));
                    if let Err(e) = std::fs::create_dir_all(&real_dir) {
                        eprintln!("create_dir {:?}: {}", real_dir, e);
                        let _ = std::fs::remove_file(&dest_path);
                        skip_fail += 1;
                        continue;
                    }
                    let real_path = real_dir.join(&filename);
                    // Late dest-exists check: file might already be in
                    // the real bucket from a prior import. Honour the
                    // same size-match shortcut as the fast path.
                    if real_path.exists() {
                        if let Ok(md) = std::fs::metadata(&real_path) {
                            if md.len() == obj.size {
                                let _ = std::fs::remove_file(&dest_path);
                                skip_dest += 1;
                                continue;
                            }
                        }
                    }
                    if let Err(e) = std::fs::rename(&dest_path, &real_path) {
                        eprintln!("rename inbox→bucket failed: {}", e);
                        let _ = std::fs::remove_file(&dest_path);
                        skip_fail += 1;
                        continue;
                    }
                    real_path
                };

                // Hash + insert into photos table (reuse scanner logic).
                let src_str = final_path.to_string_lossy().to_string();
                let hash = match crate::scanner::compute_hash(&src_str) {
                    Ok(h) => h,
                    Err(_) => {
                        skip_fail += 1;
                        continue;
                    }
                };

                // If hash already exists in DB, delete the file we just
                // wrote and skip.
                let existing_id = {
                    let conn = db_clone.lock().unwrap_or_else(|e| e.into_inner());
                    conn.query_row(
                        "SELECT id FROM photos WHERE hash = ?1 LIMIT 1",
                        [&hash],
                        |r| r.get::<_, i64>(0),
                    )
                    .ok()
                };
                if let Some(existing_photo_id) = existing_id {
                    // File already in library under some other path.
                    // Delete our duplicate and still record the mtp
                    // mapping so "delete from phone except favorites"
                    // works correctly for dedup'd photos too.
                    let _ = std::fs::remove_file(&final_path);
                    imported_ids.push((obj.id.clone(), src_str, existing_photo_id));
                    skip_dup += 1;
                    continue;
                }

                // Register the new photo in the DB. EXIF, phash,
                // blur_score, CLIP embeddings, etc. get populated later
                // when the user runs a scan over the destination folder
                // (or when the existing watch-folder picks it up — we
                // could wire that here, but keeping this command small).
                let file_size = std::fs::metadata(&final_path)
                    .map(|m| m.len() as i64)
                    .unwrap_or(obj.size as i64);
                let folder = final_path
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                let media_type = if obj.is_video { "video" } else { "image" };
                // v1.5.152 — Normalize WPD's date format to ISO before
                // storing. WPD on Windows returns iPhone dates as
                // "2025/09/15:14:30:00"; the calendar/timeline SQL
                // assumes ISO ("2025-09-15 14:30:00"), and the timeline
                // dial in particular crashed into bogus "2025/09" year
                // entries because split('-') couldn't see the slashes.
                // Normalize once at the boundary so the rest of the
                // pipeline doesn't have to defend.
                let date_taken = obj.date_created.as_deref().map(|s| {
                    let mut t = s.replace('/', "-");
                    // Some pipelines also write "2025-09-15:14:30:00" —
                    // colon between date and time instead of a space.
                    // ISO accepts space; rewrite the first colon at
                    // position 10 (right after YYYY-MM-DD) to a space.
                    if t.len() > 10 && t.as_bytes().get(10) == Some(&b':') {
                        t.replace_range(10..11, " ");
                    }
                    t
                });

                let new_photo = db::NewPhoto {
                    path: &src_str,
                    filename: &filename,
                    folder: &folder,
                    hash: &hash,
                    size: file_size,
                    width: None,
                    height: None,
                    media_type,
                    date_taken,
                    duration_secs: None,
                };
                let new_id = {
                    let conn = db_clone.lock().unwrap_or_else(|e| e.into_inner());
                    match db::insert_photo(&conn, &new_photo) {
                        Ok(id) => id,
                        Err(e) => {
                            eprintln!("insert_photo: {e}");
                            skip_fail += 1;
                            continue;
                        }
                    }
                };

                imported_ids.push((obj.id.clone(), src_str, new_id));
                copied += 1;

                if i % 5 == 0 || i == total - 1 {
                    // v1.5.151 — Emit the new skip-reason buckets alongside
                    // the legacy `skipped` total so an older frontend keeps
                    // working without changes.
                    let skipped = skip_dest + skip_dup + skip_fail;
                    ah.emit(
                        "mtp-import-progress",
                        serde_json::json!({
                            "device": device_id_clone,
                            "current": filename,
                            "done": i + 1,
                            "total": total,
                            "copied": copied,
                            "skipped": skipped,
                            "skip_dest": skip_dest,
                            "skip_dup": skip_dup,
                            "skip_fail": skip_fail,
                        }),
                    )
                    .ok();
                }
            }

            // v1.5.153 — Tear down the slow-path temp inbox once the
            // loop is done. Best-effort: if any leftovers exist (a
            // crash mid-rename, etc.) they stay for the user to deal
            // with manually, but the empty dir gets removed cleanly.
            let _ = std::fs::remove_dir(&inbox);

            // Record the mtp_imports mapping in one transaction.
            {
                let now = chrono::Utc::now().timestamp();
                let mut conn = db_clone.lock().unwrap_or_else(|e| e.into_inner());
                let tx = conn.transaction().map_err(|e| e.to_string())?;
                for (object_id, _path, photo_id) in &imported_ids {
                    tx.execute(
                        "INSERT OR REPLACE INTO mtp_imports
                            (photo_id, device_id, object_id, imported_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![photo_id, device_id_clone, object_id, now],
                    )
                    .ok();
                }
                tx.commit().map_err(|e| e.to_string())?;
            }

            // v1.5.151 — Final event also carries the skip-reason split.
            let skipped = skip_dest + skip_dup + skip_fail;
            ah.emit(
                "mtp-import-complete",
                serde_json::json!({
                    "device": device_id_clone,
                    "copied": copied,
                    "skipped": skipped,
                    "skip_dest": skip_dest,
                    "skip_dup": skip_dup,
                    "skip_fail": skip_fail,
                    "total": total,
                }),
            )
            .ok();

            Ok((copied, skip_dest, skip_dup, skip_fail, total))
        })
        .await
        .map_err(|e| format!("mtp_import join: {e}"))??;

        // v1.5.151 — Return the skip-reason split so the cleanup screen
        // can show "X already in library · Y already in dest · Z failed"
        // instead of just one opaque total.
        let (copied, skip_dest, skip_dup, skip_fail, total) = result;
        let skipped = skip_dest + skip_dup + skip_fail;
        Ok(serde_json::json!({
            "copied": copied,
            "skipped": skipped,
            "skip_dest": skip_dest,
            "skip_dup": skip_dup,
            "skip_fail": skip_fail,
            "total": total,
        }))
    }
}

/// Delete a list of MTP objects directly from the phone.
/// Used by "iPhone'u komple sil" (after confirmation).
#[tauri::command]
pub async fn mtp_delete(
    device_id: String,
    object_ids: Vec<String>,
) -> Result<serde_json::Value, String> {
    #[cfg(windows)]
    {
        let (deleted, failed) =
            tokio::task::spawn_blocking(move || crate::mtp::delete_objects(&device_id, &object_ids))
                .await
                .map_err(|e| format!("mtp_delete join: {e}"))??;
        Ok(serde_json::json!({ "deleted": deleted, "failed": failed }))
    }
    #[cfg(not(windows))]
    {
        let _ = (device_id, object_ids);
        Err("MTP is only supported on Windows in this build".to_string())
    }
}

/// Delete every photo on the phone EXCEPT those whose corresponding
/// library photo is marked `favorite = 1`. Uses the `mtp_imports` map
/// built during import to know which on-device object corresponds to
/// which library photo. Favorites stay on the phone; everything else
/// is removed.
///
/// Returns { deleted, kept_favorites, not_imported }.
#[tauri::command]
pub async fn mtp_delete_non_favorites(
    device_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    #[cfg(not(windows))]
    {
        let _ = (device_id, state);
        return Err("MTP is only supported on Windows in this build".to_string());
    }
    #[cfg(windows)]
    {
        // 1. Walk the phone — get every current object_id.
        let device_id_for_list = device_id.clone();
        let on_phone = tokio::task::spawn_blocking(move || {
            crate::mtp::list_media(&device_id_for_list)
        })
        .await
        .map_err(|e| format!("list_media join: {e}"))??;

        // 2. For each on-phone object, look it up in mtp_imports. If the
        //    corresponding library photo exists AND is favorited, skip.
        //    Otherwise add to delete list.
        let mut favorite_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut not_imported_count = 0usize;
        {
            let conn = state.db.lock().map_err(|_| "db lock")?;
            let mut stmt = conn
                .prepare(
                    "SELECT m.object_id
                       FROM mtp_imports m
                       JOIN photos p ON p.id = m.photo_id
                      WHERE m.device_id = ?1
                        AND p.favorite = 1",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([&device_id], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            for row in rows {
                if let Ok(oid) = row {
                    favorite_ids.insert(oid);
                }
            }

            // Count on-phone objects that were never imported (no mapping)
            // just so we can surface the number in the UI. These would
            // ALSO be deleted by this call — warn the user in the
            // confirmation dialog client-side.
            let imported_ids: std::collections::HashSet<String> = {
                let mut stmt2 = conn
                    .prepare(
                        "SELECT object_id FROM mtp_imports WHERE device_id = ?1",
                    )
                    .map_err(|e| e.to_string())?;
                let mut ids = std::collections::HashSet::new();
                let iter = stmt2
                    .query_map([&device_id], |r| r.get::<_, String>(0))
                    .map_err(|e| e.to_string())?;
                for r in iter.flatten() {
                    ids.insert(r);
                }
                ids
            };
            for obj in &on_phone.photos {
                if !imported_ids.contains(&obj.id) {
                    not_imported_count += 1;
                }
            }
        }

        let to_delete: Vec<String> = on_phone
            .photos
            .iter()
            .filter(|o| !favorite_ids.contains(&o.id))
            .map(|o| o.id.clone())
            .collect();
        let kept = favorite_ids.len();

        let device_id_for_del = device_id.clone();
        let (deleted, _failed) = tokio::task::spawn_blocking(move || {
            crate::mtp::delete_objects(&device_id_for_del, &to_delete)
        })
        .await
        .map_err(|e| format!("delete join: {e}"))??;

        Ok(serde_json::json!({
            "deleted": deleted,
            "kept_favorites": kept,
            "not_imported": not_imported_count,
        }))
    }
}

/// Helper: parse an MTP-format date string ("2024/03/15:10:22:11.000") or
/// ISO-8601 variant into (year, month_folder). Falls back to ("Unknown",
/// "Unknown") so every photo has a home even if the phone doesn't report
/// a date.
#[cfg(windows)]
fn parse_mtp_date_bucket(s: Option<&str>) -> (String, String) {
    use chrono::Datelike;
    let s = match s {
        Some(x) if !x.is_empty() => x,
        _ => return ("Unknown".to_string(), "Unknown".to_string()),
    };
    // Try "YYYY/MM/DD:..." first (WPD format).
    let iso_like = s.replace('/', "-");
    // Chop off anything after the first ':' group past the date.
    let date_part = iso_like.split(':').next().unwrap_or(&iso_like);
    if let Ok(d) = chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d") {
        // v1.5.147 — Match Mac's v1.5.x rename: English month names
        // (Mac reverted all UI strings to English along with v1.5.139)
        // AND drop the redundant year prefix from the inner folder
        // (we're already inside `Year/`, so `08-August` is enough
        // versus the old `2024-08 - August`). Goal is identical
        // imported folder layout across Mac+Windows so an SMB-shared
        // library doesn't end up with two parallel structures.
        let month_name = english_month_name(d.month());
        return (
            d.year().to_string(),
            format!("{:02}-{}", d.month(), month_name),
        );
    }
    ("Unknown".to_string(), "Unknown".to_string())
}

fn english_month_name(m: u32) -> &'static str {
    match m {
        1 => "January", 2 => "February", 3 => "March", 4 => "April",
        5 => "May", 6 => "June", 7 => "July", 8 => "August",
        9 => "September", 10 => "October", 11 => "November", 12 => "December",
        _ => "Unknown",
    }
}

#[tauri::command]
pub async fn stop_device_monitor(
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut guard = state.device_monitor.lock().map_err(|_| "monitor lock")?;
    if let Some(mut monitor) = guard.take() {
        monitor.stop();
    }
    {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::set_setting(&conn, "auto_import_enabled", "false").ok();
    }
    Ok(())
}

/// Copy media from `source_path` (device folder like `E:\DCIM`) into
/// `dest_dir`, organised by EXIF capture date as
/// `dest_dir/YYYY/YYYY-MM - MonthName/filename.ext`.
///
/// - Uses EXIF `DateTimeOriginal` when available; falls back to file mtime.
/// - Skips files whose hash already exists in the DB (cross-folder dedup).
/// - Skips files whose destination already exists (idempotent re-run).
/// - Emits `device-import-progress` every 5 files and
///   `device-import-complete` when done.
/// - After copy, the destination is added as a watch folder so the existing
///   scanner picks up the new files (keeps a single import pipeline).
#[tauri::command]
pub async fn import_from_device(
    source_path: String,
    dest_dir: String,
    remember_dest: bool,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    let source = std::path::PathBuf::from(&source_path);
    let dest_root = std::path::PathBuf::from(&dest_dir);

    if !source.exists() {
        return Err(format!("Source does not exist: {}", source_path));
    }
    std::fs::create_dir_all(&dest_root).map_err(|e| e.to_string())?;

    if remember_dest {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::set_setting(&conn, "auto_import_dir", &dest_dir).ok();
    }

    let db_clone = state.db.clone();
    let ah = app_handle.clone();
    let drive_label = source_path.clone();

    // Spawn on a blocking thread — walking a slow SD card / camera can take
    // several minutes and we don't want to stall Tauri's async runtime.
    let result = tokio::task::spawn_blocking(move || -> Result<(usize, usize), String> {
        let files: Vec<std::path::PathBuf> = walkdir::WalkDir::new(&source)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file() && crate::scanner::is_image_file(e.path()))
            .map(|e| e.path().to_path_buf())
            .collect();

        let total = files.len();
        if total == 0 {
            ah.emit(
                "device-import-complete",
                serde_json::json!({
                    "drive": drive_label, "copied": 0, "skipped": 0, "total": 0
                }),
            )
            .ok();
            return Ok((0, 0));
        }

        let mut copied = 0usize;
        let mut skipped = 0usize;

        for (i, src) in files.iter().enumerate() {
            let src_str = src.to_string_lossy().to_string();

            // 1. Hash-dedup against DB (covers "already in library")
            let hash = match crate::scanner::compute_hash(&src_str) {
                Ok(h) => h,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let already_in_db = {
                let conn = db_clone.lock().unwrap_or_else(|e| e.into_inner());
                db::photo_exists_by_hash(&conn, &hash).unwrap_or(false)
            };
            if already_in_db {
                skipped += 1;
                if (i + 1) % 5 == 0 || i + 1 == total {
                    ah.emit(
                        "device-import-progress",
                        serde_json::json!({
                            "drive": drive_label,
                            "total": total,
                            "copied": copied,
                            "skipped": skipped,
                            "current": src.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                        }),
                    )
                    .ok();
                }
                continue;
            }

            // 2. Figure out the year/month bucket from EXIF, or fall back to mtime
            // v1.5.147 — see parse_mtp_date_bucket: English names, no
            // year prefix inside the year folder. Same folder layout
            // for the import_from_device path.
            let (year, month) = date_bucket_for_file(&src_str);
            let month_name = english_month_name(month);
            let subdir = dest_root
                .join(format!("{:04}", year))
                .join(format!("{:02}-{}", month, month_name));
            if std::fs::create_dir_all(&subdir).is_err() {
                skipped += 1;
                continue;
            }

            let filename = src
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let mut dest_file = subdir.join(&filename);

            // 3. Same-name collision → append _1, _2, ...
            if dest_file.exists() {
                let stem = src
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let ext = src
                    .extension()
                    .map(|e| format!(".{}", e.to_string_lossy()))
                    .unwrap_or_default();
                let mut n = 1;
                loop {
                    let candidate = subdir.join(format!("{}_{}{}", stem, n, ext));
                    if !candidate.exists() {
                        dest_file = candidate;
                        break;
                    }
                    n += 1;
                    if n > 9999 {
                        break;
                    }
                }
            }

            // 4. Copy (robust to flaky SD card reads — one retry)
            let copy_result = std::fs::copy(src, &dest_file)
                .or_else(|_| std::fs::copy(src, &dest_file));
            match copy_result {
                Ok(_) => copied += 1,
                Err(_) => skipped += 1,
            }

            if (i + 1) % 5 == 0 || i + 1 == total {
                ah.emit(
                    "device-import-progress",
                    serde_json::json!({
                        "drive": drive_label,
                        "total": total,
                        "copied": copied,
                        "skipped": skipped,
                        "current": filename,
                    }),
                )
                .ok();
            }
        }

        ah.emit(
            "device-import-complete",
            serde_json::json!({
                "drive": drive_label,
                "copied": copied,
                "skipped": skipped,
                "total": total,
            }),
        )
        .ok();

        Ok((copied, skipped))
    })
    .await
    .map_err(|e| e.to_string())??;

    // Register the destination as a watched folder so the scanner picks up
    // the new files without the user doing anything else.
    {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::add_watch_folder(&conn, &dest_dir, false).ok();
    }

    // Kick off a one-shot scan of the destination so the UI fills in right
    // away (rather than waiting for watcher events to trickle in).
    let dest_for_scan = dest_dir.clone();
    let db_arc = state.db.clone();
    let thumbs_dir = state.thumbnails_dir.clone();
    let stop_flag = state.scan_stop.clone();
    let ah = app_handle.clone();
    if !state.scan_running.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let scan_running = state.scan_running.clone();
        tokio::spawn(async move {
            let _ = crate::scanner::scan_folder_impl(
                dest_for_scan,
                db_arc,
                thumbs_dir,
                stop_flag,
                ah,
            )
            .await;
            scan_running.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    Ok(serde_json::json!({
        "copied": result.0,
        "skipped": result.1,
    }))
}

/// Return (year, month) for a media file: EXIF DateTimeOriginal first,
/// then file mtime, then (1970, 1) as a last-resort fallback so we never
/// crash on a file with no metadata at all.
fn date_bucket_for_file(path: &str) -> (i32, u32) {
    // EXIF path: "YYYY:MM:DD HH:MM:SS" or "YYYY-MM-DD HH:MM:SS"
    if let Ok(exif) = crate::exif_reader::read_exif(path) {
        if let Some(dt) = exif.date_taken {
            if let Some((y, m)) = parse_year_month(&dt) {
                return (y, m);
            }
        }
    }

    // File mtime fallback
    if let Ok(meta) = std::fs::metadata(path) {
        if let Ok(t) = meta.modified() {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            return (
                chrono::Datelike::year(&dt),
                chrono::Datelike::month(&dt),
            );
        }
    }

    (1970, 1)
}

fn parse_year_month(s: &str) -> Option<(i32, u32)> {
    // accept "YYYY:MM:DD ..." or "YYYY-MM-DD ..."
    let s = s.trim();
    if s.len() < 7 {
        return None;
    }
    let year: i32 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    Some((year, month))
}

// v1.5.147 — turkish_month_name removed. english_month_name above is
// the single source of truth for MTP/import folder naming. UI strings
// have been English across the rest of the app for a while; the MTP
// folder layout was the last Turkish holdout.

// ── 5. Tag Management ───────────────────────────────────────────────────────

#[tauri::command]
pub async fn merge_tags(
    source: String,
    target: String,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::merge_tags(&conn, &source, &target).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn rename_tag_global(
    old_name: String,
    new_name: String,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::rename_tag(&conn, &old_name, &new_name).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_tag_global(
    tag: String,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::delete_tag_globally(&conn, &tag).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_tag_details(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<TagInfo>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_tag_details(&conn).map_err(|e| e.to_string())
}

// ── 6. Collections / Smart Albums ───────────────────────────────────────────

#[tauri::command]
pub async fn create_collection(
    name: String,
    collection_type: String,
    rules_json: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::create_collection(&conn, &name, &collection_type, rules_json.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_collection(
    id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::delete_collection(&conn, id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_collections(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<Collection>, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|_| "db lock".to_string())?;
        db::get_collections(&conn).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn add_to_collection(
    collection_id: i64,
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::add_photo_to_collection(&conn, collection_id, photo_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn remove_from_collection(
    collection_id: i64,
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::remove_photo_from_collection(&conn, collection_id, photo_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_smart_collection_photos(
    rules_json: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<PhotoSummary>, String> {
    let rules: Vec<CollectionRule> = serde_json::from_str(&rules_json)
        .map_err(|e| format!("Invalid rules JSON: {}", e))?;
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::query_smart_collection(&conn, &rules).map_err(|e| e.to_string())
}

// ── 7. EXIF / GPS ───────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_photo_exif(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<PhotoExif, String> {
    // v1.5.48 — Also pull a manually-set DB date (if the user used the
    // Date Taken click-to-edit) and the scanner's full path-aware oldest
    // date logic. Priority for the displayed date:
    //   1. User-edited DB value (set_photo_date_taken)
    //   2. Path-aware oldest plausible (EXIF + folder path + mtime/birthtime)
    //   3. Whatever read_exif found (DateTimeOriginal/Digitized/DateTime)
    // This is what fixed the user's reproducer of a 2003 photo whose
    // EXIF was rewritten to 2022 — the folder hierarchy
    // `\2003\2003_12\2003_12_14\` now contributes 2003-12-14 as a
    // candidate, beating the EXIF on the oldest-plausible race.
    let (path, db_date) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let (p, _) = db::get_photo_path_and_hash(&conn, photo_id).map_err(|e| e.to_string())?;
        let d: Option<String> = conn.query_row(
            "SELECT date_taken FROM photos WHERE id = ?1",
            rusqlite::params![photo_id],
            |r| r.get(0),
        ).ok().flatten();
        (p, d)
    };
    tokio::task::spawn_blocking(move || -> Result<PhotoExif, String> {
        let mut exif = exif_reader::read_exif(&path).unwrap_or(PhotoExif::default());
        let path_aware = crate::scanner::best_date_taken(&path);
        // Pick the OLDEST among (db_date, path_aware, exif.date_taken).
        // db_date is a manually-set override and wins ties so the user's
        // explicit edit doesn't get silently overwritten by a path-derived
        // candidate that happens to be the same instant.
        use chrono::NaiveDateTime;
        const FORMATS: &[&str] = &[
            "%Y-%m-%d %H:%M:%S",
            "%Y:%m:%d %H:%M:%S",
            "%Y/%m/%d %H:%M:%S",
            "%d/%m/%Y %H:%M:%S",
            "%d/%m/%Y %H:%M",
            "%Y-%m-%d %H:%M",
        ];
        let parse = |s: &str| FORMATS.iter().find_map(|f| NaiveDateTime::parse_from_str(s.trim(), f).ok());
        let mut best: Option<(NaiveDateTime, String)> = None;
        for s in [&db_date, &path_aware, &exif.date_taken].into_iter().flatten() {
            if let Some(dt) = parse(s) {
                if best.as_ref().map_or(true, |(b, _)| dt < *b) {
                    best = Some((dt, s.clone()));
                }
            }
        }
        if let Some((_, s)) = best {
            exif.date_taken = Some(s);
        }
        Ok(exif)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_gps_photos(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<GpsPhoto>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let mut results = db::get_photos_with_gps(&conn).map_err(|e| e.to_string())?;
    // Also include AI-estimated locations (only for photos without real GPS)
    if let Ok(estimated) = db::get_photos_with_estimated_location(&conn) {
        results.extend(estimated);
    }
    Ok(results)
}

#[tauri::command]
pub async fn extract_all_gps(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<usize, String> {
    let photos: Vec<(i64, String)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let mut stmt = conn.prepare(
            "SELECT id, path FROM photos WHERE gps_lat IS NULL"
        ).map_err(|e| e.to_string())?;
        let rows: Vec<(i64, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    let total = photos.len();
    let mut found = 0usize;

    for (i, (id, path)) in photos.iter().enumerate() {
        if let Ok(exif) = exif_reader::read_exif(path) {
            if let (Some(lat), Some(lon)) = (exif.gps_lat, exif.gps_lon) {
                let conn = state.db.lock().map_err(|_| "db lock")?;
                db::update_photo_gps(&conn, *id, lat, lon, exif.gps_alt).ok();
                found += 1;
            }
        }
        if i % 100 == 0 {
            app_handle.emit("gps-progress", serde_json::json!({"done": i+1, "total": total, "found": found})).ok();
        }
    }
    Ok(found)
}

// ── 8. Cost Dashboard ───────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_cost_dashboard(
    state: tauri::State<'_, AppState>,
) -> Result<CostDashboard, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_cost_dashboard(&conn).map_err(|e| e.to_string())
}

// ── 9. Right-click → Open in Explorer ───────────────────────────────────────

#[tauri::command]
pub async fn open_in_explorer(path: String) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    if p.is_file() {
        // Open the parent folder and select the file
        #[cfg(target_os = "windows")]
        {
            std::process::Command::new("explorer")
                .arg("/select,")
                .arg(&path)
                .spawn()
                .map_err(|e| e.to_string())?;
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open")
                .arg("-R")
                .arg(&path)
                .spawn()
                .map_err(|e| e.to_string())?;
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(parent) = p.parent() {
                opener::open(parent).map_err(|e| e.to_string())?;
            }
        }
    } else {
        opener::open(&path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn open_file(path: String) -> Result<(), String> {
    opener::open(&path).map_err(|e| e.to_string())
}

/// Reveal the RetinaTag app-data directory in Explorer / Finder / xdg-open.
///
/// v1.5.21 — paired with the v1.5.20 "Copy App Diagnostic Info" palette
/// action. When a user is assembling a bug report, the diagnostic text
/// goes in the message body and the DB / thumbnails / logs live here.
/// One-click access beats walking someone through `%APPDATA%` paths.
///
/// We derive the path from `AppState.thumbnails_dir.parent()` rather than
/// re-running the Roaming-vs-Local resolution in `setup()` — the setup
/// path already picked the right directory at boot, and we want to point
/// the user at the same place the DB actually lives, not re-decide.
///
/// Returns the resolved path so the frontend can surface it in the toast
/// (useful if Explorer fails to open for whatever reason).
#[tauri::command]
pub async fn reveal_app_data_dir(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let dir = state
        .thumbnails_dir
        .parent()
        .ok_or_else(|| "thumbnails_dir has no parent".to_string())?
        .to_path_buf();
    let s = dir.to_string_lossy().to_string();
    opener::open(&dir).map_err(|e| format!("opener failed: {} (path: {})", e, s))?;
    Ok(s)
}

// ── 10. Duplicate Detection ─────────────────────────────────────────────────

/// Compute perceptual hashes for all photos that don't yet have one.
///
/// Performance notes:
/// - Up to 100k photos per call (was 10k — caused the backend to quietly
///   stop after 10k even when the library had 50k, leaving the user staring
///   at a `Hashing…` button that was actually idle).
/// - CPU-parallel via rayon: one worker per core. DCT + resize is pure
///   CPU, so this is ~N× faster on N-core machines.
/// - Uses the cached thumbnail JPEG when available — a 200×200 thumb is
///   ~20× cheaper to load than decoding a full 24MP original, and because
///   thumbnails are already EXIF-rotated at scan time the resulting hash
///   is identical.
/// - Respects `state.face_stop` so the UI Stop button actually stops it.
/// - Emits `phash-progress` events frequently so the UI can render a
///   real progress bar with ETA.
#[tauri::command]
pub async fn compute_phashes(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<usize, String> {
    use rayon::iter::{IntoParallelIterator, ParallelIterator};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let photos: Vec<(i64, String, String)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_photos_without_phash_with_hash(&conn, 100_000).map_err(|e| e.to_string())?
    };

    let total = photos.len();
    if total == 0 {
        app_handle.emit("phash-progress", serde_json::json!({
            "done": 0, "total": 0, "updated": 0, "finished": true,
        })).ok();
        return Ok(0);
    }

    // Reset + clone stop flag so the UI Stop button works.
    state.face_stop.store(false, Ordering::SeqCst);
    let stop_flag = state.face_stop.clone();
    let stop_flag_cleanup = state.face_stop.clone();

    let thumbs_dir = state.thumbnails_dir.clone();
    let db_arc = state.db.clone();
    let ah = app_handle.clone();
    let start_time = std::time::Instant::now();

    let result = tokio::task::spawn_blocking(move || -> Result<usize, String> {
        let done = AtomicUsize::new(0);
        let updated = AtomicUsize::new(0);

        // Background progress emitter — one thread that wakes every 250ms
        // and publishes the current counters. Keeps event volume bounded
        // regardless of how fast hashing runs.
        let done_snap = std::sync::Arc::new(AtomicUsize::new(0));
        let updated_snap = std::sync::Arc::new(AtomicUsize::new(0));
        let emitter_done = done_snap.clone();
        let emitter_updated = updated_snap.clone();
        let emitter_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let emitter_stop_clone = emitter_stop.clone();
        let ah_emit = ah.clone();
        let total_for_emit = total;
        let emitter = std::thread::spawn(move || {
            while !emitter_stop_clone.load(Ordering::SeqCst) {
                let d = emitter_done.load(Ordering::SeqCst);
                let u = emitter_updated.load(Ordering::SeqCst);
                let elapsed = start_time.elapsed().as_secs_f64();
                let rate = if elapsed > 0.01 { d as f64 / elapsed } else { 0.0 };
                let eta = if rate > 0.1 && d < total_for_emit {
                    ((total_for_emit - d) as f64 / rate) as u64
                } else { 0 };
                ah_emit.emit("phash-progress", serde_json::json!({
                    "done": d,
                    "total": total_for_emit,
                    "updated": u,
                    "rate_per_sec": rate,
                    "eta_secs": eta,
                    "elapsed_secs": elapsed,
                })).ok();
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        });

        // Parallel hashing. We process in a rayon parallel iterator so
        // each core grabs photos off the queue independently.
        photos.into_par_iter().for_each(|(id, path, hash)| {
            if stop_flag.load(Ordering::SeqCst) {
                return; // early-exit the closure; rayon drains the rest fast
            }

            // Try the cached thumbnail first (much faster). Thumbnails are
            // already EXIF-rotated so the hash is identical.
            let img = if !hash.is_empty() {
                let cache_name = thumbnail::thumb_cache_name(&hash);
                let cache_path = thumbs_dir.join(&cache_name);
                if cache_path.exists() {
                    image::open(&cache_path).ok()
                } else {
                    None
                }
            } else {
                None
            };
            // Fall back to the original file (with EXIF rotation).
            let img = img.or_else(|| crate::thumbnail::open_image(&path).ok());

            if let Some(img) = img {
                if let Ok(h) = exif_reader::compute_phash_from_image(&img) {
                    // Lock the DB only for the quick write; keep it brief.
                    let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                    db::update_photo_phash(&conn, id, &h).ok();
                    updated.fetch_add(1, Ordering::SeqCst);
                    updated_snap.store(updated.load(Ordering::SeqCst), Ordering::SeqCst);
                }
            }
            let d = done.fetch_add(1, Ordering::SeqCst) + 1;
            done_snap.store(d, Ordering::SeqCst);
        });

        // Tell emitter to stop and push a final event.
        emitter_stop.store(true, Ordering::SeqCst);
        emitter.join().ok();
        let final_done = done.load(Ordering::SeqCst);
        let final_updated = updated.load(Ordering::SeqCst);
        ah.emit("phash-progress", serde_json::json!({
            "done": final_done,
            "total": total,
            "updated": final_updated,
            "rate_per_sec": final_done as f64 / start_time.elapsed().as_secs_f64().max(0.01),
            "eta_secs": 0,
            "elapsed_secs": start_time.elapsed().as_secs_f64(),
            "finished": true,
            "stopped": stop_flag.load(Ordering::SeqCst),
        })).ok();
        Ok(final_updated)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    stop_flag_cleanup.store(false, Ordering::SeqCst);
    result
}

#[tauri::command]
pub async fn get_duplicates(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<DuplicateGroup>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let groups = db::get_duplicate_groups(&conn).map_err(|e| e.to_string())?;
    Ok(groups
        .into_iter()
        .map(|(hash, photos)| DuplicateGroup { hash, photos })
        .collect())
}

// ── Cleanup: Duplicates + Blurry photos ─────────────────────────────────────

/// Compute Laplacian-variance blur scores for photos that don't have one yet.
/// Reads each photo's cached thumbnail JPEG (fast — ~1ms per photo) instead
/// of re-opening the original. Emits "blur-scan-progress" events and respects
/// the `face_stop` flag (reused — no point having two stop flags).
#[tauri::command]
pub async fn compute_blur_scores(
    folder: Option<String>,
    batch_size: Option<i64>,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<usize, String> {
    let folder_filter: Option<String> = folder
        .as_ref()
        .and_then(|s| if s.trim().is_empty() { None } else { Some(s.clone()) });
    let limit = batch_size.unwrap_or(5000).max(1);

    // Collect photos that need scoring
    let photos: Vec<(i64, String, String)> = {
        let conn = state.db.lock().map_err(|_| "db lock".to_string())?;
        if let Some(f) = &folder_filter {
            // Prefix match via substr() instead of LIKE '%'-concat — LIKE
            // would interpret `%` and `_` inside the path as wildcards.
            let mut stmt = conn.prepare(
                "SELECT id, path, hash FROM photos
                 WHERE blur_score IS NULL
                   AND media_type = 'image'
                   AND (folder = ?1 OR substr(path, 1, length(?1)) = ?1)
                 ORDER BY id DESC
                 LIMIT ?2"
            ).map_err(|e| e.to_string())?;
            let v: Vec<(i64, String, String)> = stmt.query_map(rusqlite::params![f, limit], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                ))
            }).map_err(|e| e.to_string())?
              .filter_map(|r| r.ok())
              .collect();
            v
        } else {
            db::get_photos_without_blur_score(&conn, limit)
                .map_err(|e| e.to_string())?
        }
    };

    if photos.is_empty() {
        return Ok(0);
    }

    let total = photos.len();
    let thumbs_dir = state.thumbnails_dir.clone();
    let db_arc = state.db.clone();
    let ah = app.clone();

    state.face_stop.store(false, std::sync::atomic::Ordering::SeqCst);
    let stop_flag = state.face_stop.clone();
    let stop_flag_cleanup = state.face_stop.clone();

    let result = tokio::task::spawn_blocking(move || -> Result<usize, String> {
        let mut done = 0usize;
        let mut scored = 0usize;
        let start = std::time::Instant::now();
        for (id, path, hash) in &photos {
            if stop_flag.load(std::sync::atomic::Ordering::SeqCst) {
                eprintln!("[blur] compute_blur_scores: stop flag set at {}/{}", done, total);
                break;
            }

            // Prefer the cached thumbnail (fast). Fall back to opening the
            // original if no thumbnail exists yet.
            let detail: Option<crate::quality::BlurScoreDetail> = if !hash.is_empty() {
                let cache_name = thumbnail::thumb_cache_name(&hash);
                let cache_path = thumbs_dir.join(&cache_name);
                if cache_path.exists() {
                    crate::quality::score_thumbnail_file_detailed(&cache_path).ok()
                } else {
                    crate::thumbnail::open_image(path)
                        .ok()
                        .map(|img| crate::quality::compute_blur_score_detailed(&img))
                }
            } else {
                crate::thumbnail::open_image(path)
                    .ok()
                    .map(|img| crate::quality::compute_blur_score_detailed(&img))
            };

            if let Some(d) = detail {
                // Store "sharpest region anywhere" instead of a global average.
                // This is intentionally permissive: we only want to flag photos
                // where *no part* of the frame has usable detail. Bokeh
                // portraits (center sharp), foggy landscapes (one visible
                // object), and night phone shots (bright highlights) all have
                // at least one sharp patch and stop being flagged as blurry.
                let effective = crate::quality::effective_sharpness(&d);
                let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                db::update_blur_score(&conn, *id, effective).ok();
                scored += 1;
            }
            done += 1;
            if done % 50 == 0 {
                let elapsed = start.elapsed().as_secs_f64();
                let rate = if elapsed > 0.01 { done as f64 / elapsed } else { 0.0 };
                let eta = if rate > 0.1 && done < total {
                    ((total - done) as f64 / rate) as u64
                } else { 0 };
                ah.emit("blur-scan-progress", serde_json::json!({
                    "done": done, "total": total, "scored": scored,
                    "rate_per_sec": rate, "eta_secs": eta, "elapsed_secs": elapsed,
                })).ok();
            }
        }
        // Final progress emit
        let elapsed = start.elapsed().as_secs_f64();
        let rate = if elapsed > 0.01 { done as f64 / elapsed } else { 0.0 };
        ah.emit("blur-scan-progress", serde_json::json!({
            "done": done, "total": total, "scored": scored, "finished": true,
            "rate_per_sec": rate, "eta_secs": 0, "elapsed_secs": elapsed,
        })).ok();
        Ok(scored)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    stop_flag_cleanup.store(false, std::sync::atomic::Ordering::SeqCst);
    result
}

/// Return the summary counts for the Cleanup dashboard.
#[tauri::command]
pub async fn get_cleanup_summary(
    state: tauri::State<'_, AppState>,
) -> Result<CleanupSummary, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_cleanup_summary(&conn).map_err(|e| e.to_string())
}

fn row_to_signals(r: &db::CleanupRow) -> crate::quality::KeeperSignals {
    crate::quality::KeeperSignals {
        width: r.width,
        height: r.height,
        size_bytes: r.size_bytes,
        blur: r.blur_score,
        rating: r.rating,
        favorite: r.favorite,
        tag_count: r.tag_count,
        person_count: r.person_count,
        collection_count: r.collection_count,
        has_xmp: r.has_xmp,
        format_priority: crate::quality::format_priority_from_filename(&r.filename),
    }
}

/// Median of a slice of floats. Returns None if the slice is empty.
fn median_f32(values: &[f32]) -> Option<f32> {
    if values.is_empty() { return None; }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    if v.len() % 2 == 0 {
        Some((v[mid - 1] + v[mid]) / 2.0)
    } else {
        Some(v[mid])
    }
}

fn to_cleanup_photo(
    r: &db::CleanupRow,
    is_keeper: bool,
    keeper_score: f64,
    reasons: Vec<String>,
) -> CleanupPhoto {
    let sig = row_to_signals(r);
    CleanupPhoto {
        id: r.id,
        path: r.path.clone(),
        filename: r.filename.clone(),
        folder: r.folder.clone(),
        width: r.width,
        height: r.height,
        size_bytes: r.size_bytes,
        rating: r.rating,
        favorite: r.favorite,
        blur_score: r.blur_score,
        date_taken: r.date_taken.clone(),
        tag_count: r.tag_count,
        person_count: r.person_count,
        collection_count: r.collection_count,
        has_xmp: r.has_xmp,
        is_invested: sig.is_invested(),
        keeper_score,
        is_keeper,
        keeper_reasons: reasons,
    }
}

/// Return duplicate groups with per-photo detail and an auto-picked keeper
/// in each group. The keeper is the photo with the highest `keeper_score`
/// (see `quality::keeper_score`).
#[tauri::command]
pub async fn get_cleanup_duplicates(
    folder: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<CleanupDuplicateGroup>, String> {
    let folder_filter: Option<String> = folder
        .as_ref()
        .and_then(|s| if s.trim().is_empty() { None } else { Some(s.clone()) });

    let conn = state.db.lock().map_err(|_| "db lock")?;
    let raw_groups = db::get_duplicate_cleanup_rows(&conn, folder_filter.as_deref())
        .map_err(|e| e.to_string())?;

    let mut out: Vec<CleanupDuplicateGroup> = Vec::with_capacity(raw_groups.len());
    for (hash, rows) in raw_groups {
        // Per-group blur median for imputation: if a row is missing blur_score
        // (not yet analyzed), using 0 would unfairly punish it versus a row
        // that happens to have been scored. Use the group's median so a
        // missing-blur row lands at "typical" rather than "worst".
        let group_blurs: Vec<f32> = rows.iter().filter_map(|r| r.blur_score).collect();
        let median_blur = median_f32(&group_blurs);

        // Score each row with the rich KeeperSignals bundle (tags/person/
        // collection/xmp/format all factor in). Blur is imputed to group
        // median when missing so scoring is apples-to-apples.
        let scored: Vec<(db::CleanupRow, f64, Vec<String>)> = rows
            .into_iter()
            .map(|r| {
                let mut sig = row_to_signals(&r);
                if sig.blur.is_none() {
                    sig.blur = median_blur;
                }
                let s = crate::quality::keeper_score(&sig);
                let reasons = crate::quality::explain_keeper(&sig);
                (r, s, reasons)
            })
            .collect();

        // Highest score = keeper.
        // Tiebreakers (stable + meaningful):
        //   1. older date_taken wins (usually the original capture), BUT only
        //      when BOTH sides have a date — otherwise skip to id so we never
        //      penalize a missing date.
        //   2. lower id wins (first imported)
        let mut max_idx = 0usize;
        let mut best_score: Option<f64> = None;
        let mut best_date: Option<String> = None;
        let mut best_id: i64 = 0;
        for (i, (r, s, _)) in scored.iter().enumerate() {
            let replace = match best_score {
                None => true,
                Some(bs) => {
                    if *s > bs {
                        true
                    } else if *s < bs {
                        false
                    } else {
                        // Equal score — apply date tiebreak only when both have dates.
                        match (&best_date, &r.date_taken) {
                            (Some(bd), Some(cd)) if cd < bd => true,
                            (Some(bd), Some(cd)) if cd > bd => false,
                            // Same date or one/both missing → fall through to id
                            _ => r.id < best_id,
                        }
                    }
                }
            };
            if replace {
                best_score = Some(*s);
                best_date = r.date_taken.clone();
                best_id = r.id;
                max_idx = i;
            }
        }

        // Order: keeper first, then non-keepers by score desc
        let mut indexed: Vec<(usize, f64)> = scored.iter().enumerate()
            .map(|(i, (_, s, _))| (i, *s)).collect();
        indexed.sort_by(|a, b| {
            if a.0 == max_idx { return std::cmp::Ordering::Less; }
            if b.0 == max_idx { return std::cmp::Ordering::Greater; }
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut photos: Vec<CleanupPhoto> = Vec::with_capacity(scored.len());
        let mut bytes_reclaimable: i64 = 0;
        for (i, _s) in &indexed {
            let (r, score, reasons) = &scored[*i];
            let is_keeper = *i == max_idx;

            // Guard: never count an *invested* non-keeper toward reclaimable
            // bytes — we would NOT auto-delete it, so it's misleading to
            // show it as "can free X MB".
            let sig = row_to_signals(r);
            let invested = sig.is_invested();
            if !is_keeper && !invested {
                bytes_reclaimable += r.size_bytes.max(0);
            }

            photos.push(to_cleanup_photo(r, is_keeper, *score, reasons.clone()));
        }

        // Exact-pHash groups are high-confidence — 0.95 (room for near-dup
        // groups in the future with lower confidence).
        out.push(CleanupDuplicateGroup {
            hash,
            photos,
            bytes_reclaimable,
            confidence: 0.95,
        });
    }

    // Largest reclaim potential first — user sees biggest wins up top.
    out.sort_by(|a, b| b.bytes_reclaimable.cmp(&a.bytes_reclaimable));
    Ok(out)
}

/// Return blurry photos below `threshold`. Photos that are favorites or
/// rated ≥ 4 are excluded by default (user wants them). Set
/// `include_protected = true` to show them anyway.
#[tauri::command]
pub async fn get_cleanup_blurry(
    threshold: Option<f32>,
    folder: Option<String>,
    include_protected: Option<bool>,
    limit: Option<i64>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<CleanupPhoto>, String> {
    let folder_filter: Option<String> = folder
        .as_ref()
        .and_then(|s| if s.trim().is_empty() { None } else { Some(s.clone()) });
    let threshold = threshold.unwrap_or(100.0);
    let include_protected = include_protected.unwrap_or(false);
    let limit = limit.unwrap_or(2000).clamp(1, 10_000);

    let conn = state.db.lock().map_err(|_| "db lock")?;
    let rows = db::get_blurry_photos(
        &conn, threshold, folder_filter.as_deref(), include_protected, limit,
    ).map_err(|e| e.to_string())?;

    let photos: Vec<CleanupPhoto> = rows.iter().map(|r| {
        let sig = row_to_signals(r);
        let s = crate::quality::keeper_score(&sig);
        let reasons = crate::quality::explain_keeper(&sig);
        to_cleanup_photo(r, false, s, reasons)
    }).collect();
    Ok(photos)
}

// ── 11. Budget Management ───────────────────────────────────────────────────

#[tauri::command]
pub async fn get_budget_status(
    state: tauri::State<'_, AppState>,
) -> Result<BudgetStatus, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let limit: f64 = db::get_setting(&conn, "monthly_budget_usd")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let spent = db::get_monthly_spend(&conn).map_err(|e| e.to_string())?;
    Ok(BudgetStatus {
        monthly_limit_usd: limit,
        spent_this_month: spent,
        remaining: (limit - spent).max(0.0),
        is_over: limit > 0.0 && spent >= limit,
    })
}

// ── 12. Natural Language Search ─────────────────────────────────────────────

#[tauri::command]
pub async fn natural_language_search(
    query: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<PhotoSummary>, String> {
    let trimmed = query.trim().to_string();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }

    // Use cheapest AI provider to parse natural language into tag terms
    let router = SmartRouter::new(&state.db);
    let (provider, api_key) = router.cheapest_text_provider()
        .ok_or("No AI providers available for search")?;

    let prompt = format!(
        "Extract search keywords from this natural language photo search query. \
         Return ONLY a JSON array of English keywords suitable for searching photo tags. \
         No explanation. Query: \"{}\"",
        trimmed
    );

    // Reuse the translate function with our custom prompt
    let terms = providers::translate_query(&prompt, provider, &api_key)
        .await
        .map_err(|e| e.to_string())?;

    if terms.is_empty() {
        // Fallback to direct FTS search
        let conn = state.db.lock().map_err(|_| "db lock")?;
        return db::search_photos_fts(&conn, &format!("{}*", trimmed)).map_err(|e| e.to_string());
    }

    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::search_photos_multi(&conn, &terms).map_err(|e| e.to_string())
}

/// Translate a non-English query to English for semantic (CLIP) search.
/// Returns the English translation, or the original if translation fails.
#[tauri::command]
/// Fast Turkish→English dictionary for common photo search terms.
/// Returns (primary_term, context_tags) where context_tags help disambiguate.
/// When searching, photos matching primary_term AND any context_tag rank higher.
fn quick_translate_contextual(word: &str) -> Option<(&'static str, &'static [&'static str])> {
    let w = word.to_lowercase();
    match w.as_str() {
        // People
        "kadın"|"kadin" => Some(("woman", &[])), "erkek" => Some(("man", &[])),
        "çocuk"|"cocuk" => Some(("child", &[])), "bebek" => Some(("baby", &[])),
        "kız"|"kiz" => Some(("girl", &[])), "oğlan"|"oglan" => Some(("boy", &[])),
        "aile" => Some(("family", &[])), "çift"|"cift" => Some(("couple", &[])),
        "arkadaş"|"arkadas" => Some(("friend", &[])), "insan" => Some(("person", &[])),
        "insanlar" => Some(("people", &[])), "grup" => Some(("group", &[])),
        // Emotions
        "mutlu" => Some(("happy", &[])), "üzgün"|"uzgun" => Some(("sad", &[])),
        "gülümseyen"|"gulen" => Some(("smiling", &[])), "gülen" => Some(("laughing", &[])),
        "ağlayan"|"aglayan" => Some(("crying", &[])), "kızgın"|"kizgin" => Some(("angry", &[])),
        // Food & Drink — context-aware disambiguation
        // Context tags are ONLY for re-ranking, NOT added as search terms
        "bardak" => Some(("cup", &["drink", "beverage", "coffee", "tea", "water", "food"])),
        "tabak" => Some(("plate", &["food", "meal", "dinner", "dish"])),
        "yemek" => Some(("food", &["meal", "dinner", "restaurant", "dish"])),
        "kahve" => Some(("coffee", &[])), "çay"|"cay" => Some(("tea", &[])),
        "su" => Some(("water", &["drink", "bottle"])),
        "bira" => Some(("beer", &[])), "şarap"|"sarap" => Some(("wine", &[])),
        "kokteyl" => Some(("cocktail", &[])),
        "ekmek" => Some(("bread", &[])), "pasta" => Some(("cake", &[])), "et" => Some(("meat", &[])),
        "balık"|"balik" => Some(("fish", &[])), "salata" => Some(("salad", &[])),
        "makarna" => Some(("pasta", &[])), "pizza" => Some(("pizza", &[])),
        "meyve" => Some(("fruit", &[])), "sebze" => Some(("vegetable", &[])),
        "tatlı"|"tatli" => Some(("dessert", &[])), "dondurma" => Some(("ice cream", &[])),
        "kahvaltı"|"kahvalti" => Some(("breakfast", &[])), "öğle"|"ogle" => Some(("lunch", &[])),
        "akşam yemeği"|"aksam yemegi" => Some(("dinner", &[])), "restoran" => Some(("restaurant", &[])),
        // Animals
        "kedi" => Some(("cat", &[])), "köpek"|"kopek" => Some(("dog", &[])),
        "kuş"|"kus" => Some(("bird", &[])), "at" => Some(("horse", &[])),
        // Nature & Places
        "deniz" => Some(("sea", &["ocean", "beach"])), "plaj" => Some(("beach", &[])),
        "dağ"|"dag" => Some(("mountain", &[])), "göl"|"gol" => Some(("lake", &[])),
        "nehir" => Some(("river", &[])), "orman" => Some(("forest", &[])),
        "ağaç"|"agac" => Some(("tree", &[])), "çiçek"|"cicek" => Some(("flower", &[])),
        "güneş"|"gunes" => Some(("sun", &["sunny"])), "ay" => Some(("moon", &[])),
        "yıldız"|"yildiz" => Some(("star", &[])), "gökyüzü"|"gokyuzu" => Some(("sky", &[])),
        "bulut" => Some(("cloud", &[])), "yağmur"|"yagmur" => Some(("rain", &[])),
        "kar" => Some(("snow", &[])),
        "gün batımı"|"gun batimi" => Some(("sunset", &[])), "gündoğumu" => Some(("sunrise", &[])),
        "park" => Some(("park", &[])), "bahçe"|"bahce" => Some(("garden", &[])),
        // Places & Buildings
        "ev" => Some(("house", &[])), "bina" => Some(("building", &[])),
        "cami" => Some(("mosque", &[])), "kilise" => Some(("church", &[])),
        "köprü"|"kopru" => Some(("bridge", &[])), "kale" => Some(("castle", &[])),
        "müze"|"muze" => Some(("museum", &[])), "okul" => Some(("school", &[])),
        "havalimanı"|"havalimani" => Some(("airport", &[])), "otel" => Some(("hotel", &[])),
        "cadde" => Some(("street", &[])), "sokak" => Some(("alley", &[])),
        "şehir"|"sehir" => Some(("city", &[])), "köy"|"koy" => Some(("village", &[])),
        // Objects — context-aware for ambiguous terms
        "cam" => Some(("glass", &["window", "transparent"])),  // glass material
        "pencere" => Some(("window", &[])),
        "araba" => Some(("car", &[])), "bisiklet" => Some(("bicycle", &[])),
        "tekne" => Some(("boat", &["sea", "ocean", "water", "marina", "harbor"])),
        "gemi" => Some(("ship", &["sea", "ocean", "harbor"])),
        "yat" => Some(("yacht", &["sea", "ocean", "luxury", "marina"])),
        "uçak"|"ucak" => Some(("airplane", &["sky", "airport", "travel"])),
        "tren" => Some(("train", &["railway", "station", "travel"])),
        "kamyon" => Some(("truck", &["vehicle", "road"])),
        "otobüs"|"otobus" => Some(("bus", &["vehicle", "road", "travel"])),
        "telefon" => Some(("phone", &[])), "bilgisayar" => Some(("computer", &[])),
        "kitap" => Some(("book", &[])), "çanta"|"canta" => Some(("bag", &[])),
        "masa" => Some(("table", &["furniture"])), "sandalye" => Some(("chair", &[])),
        "kapı"|"kapi" => Some(("door", &[])), "ayna" => Some(("mirror", &[])),
        "saat" => Some(("clock", &["watch", "time"])),
        "gözlük"|"gozluk" => Some(("glasses", &["eyewear", "sunglasses"])),
        "şapka"|"sapka" => Some(("hat", &[])),
        "ayakkabı"|"ayakkabi" => Some(("shoes", &[])), "elbise" => Some(("dress", &[])),
        // Colors
        "kırmızı"|"kirmizi" => Some(("red", &[])), "mavi" => Some(("blue", &[])),
        "yeşil"|"yesil" => Some(("green", &[])), "sarı"|"sari" => Some(("yellow", &[])),
        "siyah" => Some(("black", &[])), "beyaz" => Some(("white", &[])),
        "turuncu" => Some(("orange", &["color"])), "mor" => Some(("purple", &[])),
        "pembe" => Some(("pink", &[])), "kahverengi" => Some(("brown", &[])), "gri" => Some(("gray", &[])),
        // Activities
        "yüzme"|"yuzme" => Some(("swimming", &[])), "koşma"|"kosma" => Some(("running", &[])),
        "yürüyüş"|"yuruyus" => Some(("walking", &[])), "dans" => Some(("dancing", &[])),
        "yemek yapma" => Some(("cooking", &[])), "okuma" => Some(("reading", &[])),
        // Weather & Time
        "gece" => Some(("night", &[])), "gündüz"|"gunduz" => Some(("daytime", &[])),
        "sabah" => Some(("morning", &[])), "akşam"|"aksam" => Some(("evening", &[])),
        "kış"|"kis" => Some(("winter", &[])), "yaz" => Some(("summer", &[])),
        "ilkbahar" => Some(("spring", &["season"])), "sonbahar" => Some(("autumn", &[])),
        // Indoor
        "mutfak" => Some(("kitchen", &[])), "yatak odası"|"yatak odasi" => Some(("bedroom", &[])),
        "banyo" => Some(("bathroom", &[])), "salon" => Some(("living room", &[])), "ofis" => Some(("office", &[])),
        _ => None,
    }
}

/// Large dictionary loaded from embedded TSV (1887 entries)
fn dict_translate(word: &str) -> Option<&'static str> {
    use std::sync::OnceLock;
    use std::collections::HashMap;
    static DICT: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    let map = DICT.get_or_init(|| {
        let tsv: &'static str = include_str!("dictionary_tr.tsv");
        let mut m = HashMap::with_capacity(2000);
        for line in tsv.lines() {
            let mut parts = line.splitn(2, '\t');
            if let (Some(tr), Some(en)) = (parts.next(), parts.next()) {
                m.insert(tr, en);
            }
        }
        m
    });
    map.get(word.to_lowercase().as_str()).copied()
}

/// Quick translate: try contextual match first (has disambiguation), then large dictionary
fn quick_translate(word: &str) -> Option<&'static str> {
    quick_translate_contextual(word)
        .map(|(t, _)| t)
        .or_else(|| dict_translate(word))
}

/// Try to translate multi-word phrases using dictionary
fn quick_translate_phrase(phrase: &str) -> Option<String> {
    // Try whole phrase first
    if let Some(t) = quick_translate(phrase) {
        return Some(t.to_string());
    }
    // Try word-by-word
    let words: Vec<&str> = phrase.split_whitespace().collect();
    if words.len() < 2 { return None; }
    let translated: Vec<&str> = words.iter()
        .filter_map(|w| quick_translate(w))
        .collect();
    if !translated.is_empty() {
        Some(translated.join(" "))
    } else {
        None
    }
}

/// Get context tags for disambiguation (works for both Turkish and English)
fn get_context_tags(phrase: &str) -> Vec<String> {
    let mut ctx = Vec::new();
    for word in phrase.split_whitespace() {
        // Try Turkish dictionary
        if let Some((_, tags)) = quick_translate_contextual(word) {
            ctx.extend(tags.iter().map(|t| t.to_string()));
        }
    }
    // English context: hardcoded for ambiguous English words
    let lower = phrase.to_lowercase();
    let en_ctx: &[(&str, &[&str])] = &[
        ("cup", &["drink", "beverage", "coffee", "tea", "food"]),
        ("glass", &["drink", "beverage", "wine", "water"]),
        ("bat", &["sport", "baseball", "cricket"]),
        ("spring", &["season", "flower", "bloom"]),
        ("bark", &["dog", "tree"]),
        ("mouse", &["computer", "animal"]),
    ];
    for (word, tags) in en_ctx {
        if lower.contains(word) {
            ctx.extend(tags.iter().map(|t| t.to_string()));
        }
    }
    ctx
}

#[tauri::command]
pub async fn translate_for_clip(
    query: String,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let trimmed = query.trim().to_string();

    // 1. Try instant dictionary lookup (0ms) — only return primary translation
    // Context tags are used separately for re-ranking in search_photos, not as search terms
    if let Some((primary, _ctx)) = quick_translate_contextual(&trimmed.to_lowercase()) {
        return Ok(primary.to_string());
    }
    // Multi-word phrase
    if let Some(t) = quick_translate_phrase(&trimmed) {
        return Ok(t);
    }

    // 2. Check cache
    let cache_key = format!("clip:{}", trimmed);
    let cached = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_cached_translation(&conn, &cache_key).ok().flatten()
    };
    if let Some(c) = cached {
        return Ok(c);
    }

    // Translate via Ollama directly with a precise prompt
    let router = SmartRouter::new(&state.db);
    let (provider, api_key) = router.cheapest_text_provider()
        .ok_or("No AI provider available for translation")?;

    let prompt = format!(
        "Translate this phrase to a natural English sentence suitable for image search. \
         Return ONLY the English translation as a short descriptive phrase. \
         Do NOT add extra words, synonyms, or explanations. \
         Examples: \
         plajda çift → a couple at the beach, \
         mutfakta yemek yapan kadın → a woman cooking in the kitchen, \
         gün batımı → sunset, \
         kedi → cat, \
         restoranda akşam yemeği → dinner at a restaurant \
         Translate: \"{}\"",
        trimmed
    );

    // Call the provider directly for a clean single-word translation
    let result = match provider {
        AiProvider::Local => {
            let parts: Vec<&str> = api_key.splitn(2, '|').collect();
            let endpoint = if parts[0].is_empty() { providers::DEFAULT_OLLAMA_URL } else { parts[0] };
            let model = if parts.len() > 1 && !parts[1].is_empty() { parts[1] } else { "gemma3:4b" };
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build().map_err(|e| e.to_string())?;
            let url = format!("{}/api/chat", endpoint.trim_end_matches('/'));
            let body = serde_json::json!({
                "model": model, "stream": false,
                "messages": [{"role":"user","content": prompt}]
            });
            let resp = client.post(&url).json(&body).send().await.map_err(|e| e.to_string())?;
            let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            let raw = json["message"]["content"].as_str().unwrap_or("").trim().to_string();
            // Clean up: remove quotes, brackets, extra text
            raw.trim_matches(|c: char| c == '"' || c == '\'' || c == '[' || c == ']' || c == '.' || c == '\n')
               .trim().to_string()
        }
        _ => {
            // For cloud providers, pass raw text (translate_query adds its own prompt)
            providers::translate_query(&trimmed, provider, &api_key)
                .await.map_err(|e| e.to_string())?
                .join(" ")
        }
    };

    let result = if result.is_empty() { trimmed.clone() } else { result };

    // Cache it
    {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::cache_translation(&conn, &cache_key, None, &result, provider.key_name()).ok();
    }

    Ok(result)
}

/// AI-powered search expansion: given a query (any language), AI returns relevant English search tags.
#[tauri::command]
pub async fn ai_search_expand(
    query: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<String>, String> {
    let trimmed = query.trim().to_string();
    if trimmed.is_empty() { return Ok(vec![]); }

    // Check cache
    let cache_key = format!("aisearch:{}", trimmed);
    let cached = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_cached_translation(&conn, &cache_key).ok().flatten()
    };
    if let Some(c) = cached {
        return Ok(serde_json::from_str::<Vec<String>>(&c).unwrap_or_else(|_|
            c.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
        ));
    }

    let router = SmartRouter::new(&state.db);
    let (provider, api_key) = router.cheapest_text_provider()
        .ok_or("No AI provider available")?;

    let prompt = format!(
        "You are a photo search assistant. The user wants to find photos matching: \"{}\"\n\
         Return a JSON array of 5-15 English tags that would appear in matching photos.\n\
         Think about: what objects, people, scenes, activities would be IN the photo?\n\
         Be specific and contextual. Return ONLY the JSON array.\n\
         Example: \"bardak\" → [\"cup\",\"mug\",\"drink\",\"beverage\",\"coffee\",\"tea\"]\n\
         Example: \"plajda çift\" → [\"couple\",\"beach\",\"sand\",\"ocean\",\"summer\",\"romantic\"]",
        trimmed
    );

    let result = match provider {
        AiProvider::Local => {
            let parts: Vec<&str> = api_key.splitn(2, '|').collect();
            let endpoint = if parts[0].is_empty() { providers::DEFAULT_OLLAMA_URL } else { parts[0] };
            let model = if parts.len() > 1 && !parts[1].is_empty() { parts[1] } else { "gemma3:4b" };
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build().map_err(|e| e.to_string())?;
            let url = format!("{}/api/chat", endpoint.trim_end_matches('/'));
            let body = serde_json::json!({
                "model": model, "stream": false,
                "messages": [{"role":"user","content": prompt}]
            });
            let resp = client.post(&url).json(&body).send().await.map_err(|e| e.to_string())?;
            let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            json["message"]["content"].as_str().unwrap_or("[]").to_string()
        }
        _ => {
            providers::translate_query(&trimmed, provider, &api_key)
                .await.map_err(|e| e.to_string())?
                .join(",")
        }
    };

    let tags: Vec<String> = if let Ok(arr) = serde_json::from_str::<Vec<String>>(&result) {
        arr
    } else if let (Some(start), Some(end)) = (result.find('['), result.rfind(']')) {
        serde_json::from_str::<Vec<String>>(&result[start..=end]).unwrap_or_default()
    } else {
        result.split(',').map(|s| s.trim().trim_matches('"').to_string())
            .filter(|s| !s.is_empty() && s.len() < 40).collect()
    };

    if !tags.is_empty() {
        let json = serde_json::to_string(&tags).unwrap_or_default();
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::cache_translation(&conn, &cache_key, None, &json, provider.key_name()).ok();
    }

    Ok(tags)
}

// ── 12b. HEIC codec check ───────────────────────────────────────────────────

#[tauri::command]
pub async fn check_heic_support() -> Result<bool, String> {
    // Try to decode a tiny 1x1 HEIC-like file via PowerShell WIC — if it errors, codec missing
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "Add-Type -AssemblyName PresentationCore; try { [System.Windows.Media.Imaging.BitmapDecoder]::Create([System.Uri]::new('file:///C:/nonexistent.heic'), [System.Windows.Media.Imaging.BitmapCreateOptions]::None, [System.Windows.Media.Imaging.BitmapCacheOption]::None) } catch { if($_.Exception.InnerException -and $_.Exception.InnerException.Message -like '*codec*'){exit 1} else {exit 0} }"])
            .creation_flags(0x08000000)
            .output()
            .map_err(|e| e.to_string())?;
        // If PowerShell can load WIC HEIC decoder class without "codec" error, it's installed
        // A "file not found" error means codec is present but file doesn't exist (good)
        Ok(output.status.success())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(false)
    }
}

#[tauri::command]
pub async fn install_heic_codec() -> Result<(), String> {
    // Open Microsoft Store to HEIF Image Extensions
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/c", "start", "ms-windows-store://pdp/?ProductId=9pmmsr1cgpwg"])
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── 12c. File date fallback ──────────────────────────────────────────────────

#[tauri::command]
pub async fn get_file_date(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    // v1.5.143 — Was calling std::fs::metadata(&path) directly on the
    // tokio worker. JS invokes this for every photo in the grid (per-
    // card date display). If even one photo lives on a slow/stale
    // network share, the metadata() syscall blocks that worker; with
    // a few cards on stale paths, the entire Tauri IPC mutex
    // (respond_async_serialized_inner) stalls behind them and every
    // subsequent invoke from the frontend hangs — symptoms range from
    // "settings dialog freezes" to full-app deadlock. Same class as
    // the Mac freeze that took the iPhone-import session a day to
    // bisect. Move the blocking syscall into spawn_blocking so the
    // tokio runtime keeps responding while the share resolves
    // (or fails).
    let path = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let (p, _) = db::get_photo_path_and_hash(&conn, photo_id)
            .map_err(|e| e.to_string())?;
        p
    };
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        // Prefer created time, fallback to modified
        let time = meta.created().or_else(|_| meta.modified()).map_err(|e| e.to_string())?;
        let dt: chrono::DateTime<chrono::Local> = time.into();
        Ok(dt.format("%d/%m/%Y %H:%M").to_string())
    })
    .await
    .map_err(|e| format!("join error: {}", e))?
}

// ── 13. Version Check (simple update notification) ──────────────────────────

#[derive(serde::Serialize)]
pub struct VersionInfo {
    pub current: String,
    pub latest: Option<String>,
    pub update_available: bool,
    pub download_url: Option<String>,
}

#[tauri::command]
pub async fn check_for_updates() -> Result<VersionInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    match client
        .get("https://api.github.com/repos/burskozbekov/RetinaTag/releases/latest")
        .header("User-Agent", &format!("RetinaTag/{}", current))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            let latest = json["tag_name"].as_str().unwrap_or("").trim_start_matches('v').to_string();
            let download_url = json["html_url"].as_str().map(|s| s.to_string());
            let update_available = !latest.is_empty() && latest != current;
            Ok(VersionInfo { current, latest: Some(latest), update_available, download_url })
        }
        _ => Ok(VersionInfo { current, latest: None, update_available: false, download_url: None }),
    }
}

// ── Retry & Stats ─────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn retry_failed_photos(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::reset_error_photos(&conn).map_err(|e| e.to_string())
}

/// Clear all tags and reset photos to pending for re-tagging
#[tauri::command]
pub async fn clear_all_tags(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    db::clear_all_tags(&conn).map_err(|e| e.to_string())
}

/// Re-tag a single photo: delete its tags and set status to 'pending'
#[tauri::command]
pub async fn retag_photo(photo_id: i64, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    db::retag_photo(&conn, photo_id).map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
pub struct LibraryStats {
    pub total: usize,
    pub pending: usize,
    pub tagged: usize,
    pub error: usize,
    pub total_faces: usize,
}

#[tauri::command]
pub async fn get_library_stats(state: tauri::State<'_, AppState>) -> Result<LibraryStats, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let (total, pending, tagged, error) = db::get_status_counts(&conn).map_err(|e| e.to_string())?;
    let total_faces: usize = conn.query_row("SELECT COUNT(*) FROM face_regions", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0) as usize;
    Ok(LibraryStats { total, pending, tagged, error, total_faces })
}

// ── Local Model Presets ───────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_local_model_presets() -> Result<Vec<LocalModelPreset>, String> {
    Ok(crate::models::local_model_presets())
}

// ── Ollama Service Control ─────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_ollama_status(state: tauri::State<'_, AppState>) -> Result<OllamaStatus, String> {
    let endpoint = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_setting(&conn, "ollama_endpoint")
            .ok().flatten()
            .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string())
    };
    let model = {
        let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
        db::get_setting(&conn, "model_local")
            .ok().flatten()
            .unwrap_or_else(|| "gemma3:4b".to_string())
    };

    let (running, model_loaded, models) = providers::check_ollama_status(&model, &endpoint).await;

    let message = if !running {
        "Ollama is not running".to_string()
    } else if !model_loaded {
        format!("Ollama is running but '{}' model is not loaded", model)
    } else {
        format!("Ready — {} model loaded", model)
    };

    Ok(OllamaStatus { running, model_loaded, models, message })
}

#[tauri::command]
pub async fn start_ollama_service() -> Result<String, String> {
    // Try to start Ollama in the background
    #[cfg(target_os = "windows")]
    {
        let result = std::process::Command::new("cmd")
            .args(["/C", "start", "/B", "ollama", "serve"])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .spawn();

        match result {
            Ok(_) => {
                // Wait a moment for it to start
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                Ok("Starting Ollama...".to_string())
            }
            Err(_) => {
                // Try alternate path
                let alt = std::process::Command::new("powershell")
                    .args(["-WindowStyle", "Hidden", "-Command", "Start-Process ollama -ArgumentList serve -WindowStyle Hidden"])
                    .spawn();
                match alt {
                    Ok(_) => {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        Ok("Starting Ollama...".to_string())
                    }
                    Err(e) => Err(format!("Ollama not found. Please download from ollama.com. Error: {}", e))
                }
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let result = std::process::Command::new("ollama")
            .arg("serve")
            .spawn();
        match result {
            Ok(_) => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                Ok("Starting Ollama...".to_string())
            }
            Err(e) => Err(format!("Ollama not found: {}", e))
        }
    }
}

#[tauri::command]
pub async fn stop_ollama_service() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "taskkill", "/F", "/IM", "ollama.exe"])
            .creation_flags(0x08000000)
            .spawn()
            .map_err(|e| e.to_string())?;
        Ok("Ollama stopped".to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("pkill")
            .arg("ollama")
            .spawn()
            .map_err(|e| e.to_string())?;
        Ok("Ollama stopped".to_string())
    }
}

/// Test Ollama with a real photo from the library
#[tauri::command]
pub async fn test_ollama_raw(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let endpoint = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_setting(&conn, "ollama_endpoint").ok().flatten()
            .unwrap_or_else(|| providers::DEFAULT_OLLAMA_URL.to_string())
    };
    let model = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_setting(&conn, "model_local").ok().flatten()
            .unwrap_or_else(|| "gemma3:4b".to_string())
    };

    // Get first real photo from library
    let photo_path: Option<String> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        conn.query_row("SELECT path FROM photos LIMIT 1", [], |r| r.get(0)).ok()
    };

    let photo_path = photo_path.ok_or("No photos in the library")?;

    // Prepare image same way tagger does
    let image_b64 = tokio::task::spawn_blocking({
        let p = photo_path.clone();
        move || thumbnail::prepare_for_api_local(&p)
    }).await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("Image could not be prepared: {}", e))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build().map_err(|e| e.to_string())?;

    let url = format!("{}/api/chat", endpoint.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "stream": false,
        "options": { "num_ctx": 1024, "temperature": 0.1 },
        "messages": [{
            "role": "user",
            "content": "Look at this image. List 10-15 descriptive tags as a JSON array of strings. Output ONLY the JSON array, nothing else. Example: [\"person\",\"outdoor\",\"sunset\"]",
            "images": [image_b64]
        }]
    });

    match client.post(&url).json(&body).send().await {
        Err(e) => Err(format!("Connection error: {}", e)),
        Ok(resp) => {
            let status = resp.status().as_u16();
            let raw = resp.text().await.unwrap_or_default();
            // Parse and extract the content field if possible
            let content = serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|j| j["message"]["content"].as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| raw.clone());
            Ok(format!("HTTP {}\nPhoto: {}\n\n--- RAW RESPONSE ---\n{}\n\n--- CONTENT ---\n{}",
                status, photo_path, &raw[..raw.len().min(500)], &content[..content.len().min(500)]))
        }
    }
}

#[tauri::command]
pub async fn pull_ollama_model(model: String) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "cmd", "/K", &format!("ollama pull {}", model)])
            .spawn()
            .map_err(|e| format!("Failed to start ollama pull: {}", e))?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("sh")
            .args(["-c", &format!("ollama pull {}", model)])
            .spawn()
            .map_err(|e| format!("Failed to start ollama pull: {}", e))?;
    }
    Ok(format!("'{}' model is downloading — follow the terminal window", model))
}

// ── 14. Face Recognition ─────────────────────────────────────────────────────

fn models_dir_for(app: &tauri::AppHandle) -> std::path::PathBuf {
    // Store models in AppData/Local/RetinaTag/models (persists across updates)
    app.path()
        .app_local_data_dir()
        .map(|d: std::path::PathBuf| d.join("models"))
        .unwrap_or_else(|_| {
            // Fallback: next to the exe
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|p| p.join("models")))
                .unwrap_or_else(|| std::path::PathBuf::from("models"))
        })
}

fn face_thumb_b64(faces_dir: &std::path::Path, face_id: i64) -> Option<String> {
    let p = faces_dir.join(format!("face_{}.jpg", face_id));
    std::fs::read(&p)
        .ok()
        .map(|b| base64::engine::general_purpose::STANDARD.encode(b))
}

// ─────────────────────────────────────────────────────────────────────────────
// Face-vs-art filter — shared helpers used by detect_faces_background and
// detect_faces_in_photo. Two layers working together:
//
//   LEVEL 1 (pre-filter): skip whole photos that are already CLIP-tagged as
//   "painting" / "cartoon" / "sculpture" / etc. — we never even decode them.
//   Covers the common case where the tagging pass already told us this is
//   art, not a photograph.
//
//   LEVEL 2 (per-face filter): for each detected face, run CLIP zero-shot
//   classification with one "real face" prompt vs. four "art" prompts.
//   Argmax wins — if any art prompt beats "real face", we reject the face.
//   This catches edge cases that Level 1 misses (untagged artwork, a photo
//   of a person next to a painting where the painting's face fires the
//   detector, etc.) AND replaces the flaky skin-colour heuristic that
//   was mis-rejecting B&W / dark-skin / low-light faces.
// ─────────────────────────────────────────────────────────────────────────────

/// Art-style tag tokens (English + Turkish) that should exclude a photo from
/// face detection entirely. Case-folded; matched via SQL `LOWER(tag) IN (...)`.
///
/// Keep this list CONSERVATIVE: each entry must be an unambiguous art/graphic
/// tag that would never appear on a normal photograph of a person. Level 2
/// catches anything this list misses, so the cost of being conservative is
/// just a few extra CLIP forward passes — not a correctness bug.
const ART_TAG_BLOCKLIST: &[&str] = &[
    // English
    "painting", "drawing", "cartoon", "illustration", "anime", "manga",
    "comic", "comic book", "sculpture", "statue", "artwork", "digital art",
    "concept art", "pencil drawing", "ink drawing", "oil painting",
    "watercolor", "sketch", "poster", "graffiti",
    // Turkish
    "resim", "çizim", "karikatür", "illüstrasyon", "heykel", "tablo",
    "sanat eseri", "çizgi film", "çizgi roman", "suluboya", "yağlı boya",
    "eskiz",
];

/// Build a SQL literal fragment like `('painting','drawing',...)` suitable
/// for `WHERE LOWER(tag) IN <fragment>`. We interpolate rather than bind
/// because the list is a compile-time constant and IN-clauses with bound
/// params are awkward in rusqlite. Each entry is single-quoted and any
/// apostrophes are escaped by doubling.
fn art_tag_sql_list() -> String {
    let mut s = String::from("(");
    for (i, t) in ART_TAG_BLOCKLIST.iter().enumerate() {
        if i > 0 { s.push(','); }
        s.push('\'');
        // SQL-escape single quotes by doubling (SQLite standard)
        for ch in t.chars() {
            if ch == '\'' { s.push('\''); }
            s.push(ch);
        }
        s.push('\'');
    }
    s.push(')');
    s
}

/// Text prompts for CLIP zero-shot face-vs-art classification.
/// Index 0 is the positive (real-face) prompt — any other index winning
/// means the crop is flagged as non-photographic.
const FACE_CLIP_LABELS: &[&str] = &[
    "a real photograph of a human face",
    "a cartoon or anime drawing of a face",
    "a painted portrait on canvas",
    "a sculpture statue or mannequin face",
    "a printed poster or magazine advertisement face",
];

/// Return the first CLIP tier that has both visual.onnx and textual.onnx on
/// disk, preferring smaller tiers so we don't blow through RAM alongside the
/// face models. Returns None if no CLIP is installed — caller should
/// gracefully skip Level 2 and rely on Level 1 + heuristic only.
fn pick_available_clip_tier(
    app: &tauri::AppHandle,
) -> Option<crate::clip::ClipTier> {
    let base = clip_models_dir(app);
    for tier in [
        crate::clip::ClipTier::Fast,
        crate::clip::ClipTier::Balanced,
        crate::clip::ClipTier::Best,
    ] {
        let dir = base.join(tier.dir_name());
        if dir.join("visual.onnx").exists() && dir.join("textual.onnx").exists() {
            return Some(tier);
        }
    }
    None
}

/// Load a CLIP engine and pre-compute text embeddings for FACE_CLIP_LABELS.
/// Returns Some((engine, label_embeddings)) on success, None if anything
/// failed (missing models, load error, text-encode error). Callers treat
/// None as "CLIP filter unavailable, let faces pass".
fn try_load_clip_face_filter(
    app: &tauri::AppHandle,
) -> Option<(crate::clip::ClipEngine, Vec<Vec<f32>>)> {
    let tier = pick_available_clip_tier(app)?;
    let base = clip_models_dir(app);
    let mut engine = match crate::clip::load_engine(&base, tier) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[face] CLIP filter: load_engine failed ({}), skipping Level 2", e);
            return None;
        }
    };
    let mut label_embs: Vec<Vec<f32>> = Vec::with_capacity(FACE_CLIP_LABELS.len());
    for prompt in FACE_CLIP_LABELS {
        match crate::clip::encode_text(&mut engine, prompt) {
            Ok(e) => label_embs.push(e),
            Err(e) => {
                eprintln!("[face] CLIP filter: encode_text('{}') failed: {}", prompt, e);
                return None;
            }
        }
    }
    eprintln!("[face] CLIP filter active (tier: {})", tier.label());
    Some((engine, label_embs))
}

/// Returns true if `crop_img` looks like a REAL photographic face according to
/// CLIP zero-shot classification.
///
/// **Margin-based policy** (v1.4.21): rather than pure argmax, we only reject
/// when ONE of the "art" prompts beats "real photograph" by at least
/// `REJECT_MARGIN`. Reason: CLIP's per-prompt cosine similarities are often
/// within 0.02 of each other on photographic portraits, so bare argmax rejects
/// ~30% of real faces at random depending on lighting/angle. The margin gate
/// pushes the decision to "clearly looks more like art than a photo" instead
/// of "any art prompt edged out a photo prompt by 0.003."
///
/// Falls back to `true` (permissive) on any internal error — a CLIP hiccup
/// must never silently blackhole every face in the library.
fn clip_face_is_real(
    engine: &mut crate::clip::ClipEngine,
    label_embs: &[Vec<f32>],
    crop_img: &image::DynamicImage,
) -> bool {
    // Require the best-art prompt to beat the real-photo prompt by this much
    // in cosine-similarity space before we'll reject a face. Tuned from 0.0
    // (pure argmax) after observing ~30% false-reject rate on real portraits
    // with complex lighting.
    const REJECT_MARGIN: f32 = 0.035;

    if label_embs.is_empty() || label_embs[0].is_empty() {
        return true; // CLIP filter disabled — don't block
    }
    let img_emb = match crate::clip::encode_image(engine, crop_img) {
        Ok(e) => e,
        Err(_) => return true,
    };
    if label_embs[0].is_empty() { return true; }
    let real_sim = crate::clip::cosine_similarity(&img_emb, &label_embs[0]);
    let mut best_art_sim = f32::NEG_INFINITY;
    for le in label_embs.iter().skip(1) {
        if le.is_empty() { continue; }
        let sim = crate::clip::cosine_similarity(&img_emb, le);
        if sim > best_art_sim { best_art_sim = sim; }
    }
    // Reject ONLY if an art prompt clearly beats the real-photo prompt.
    // Ties and near-ties → accept (better to keep a borderline painting
    // than lose a real face).
    best_art_sim - real_sim < REJECT_MARGIN
}

/// Crop a face region with generous padding (20% of the longer bbox side),
/// resized to CLIP-friendly dimensions. Returns a DynamicImage ready for
/// `clip::encode_image`. Padding is CLAMPED to image bounds, so faces near
/// the edge get a smaller pad rather than being rejected.
fn crop_face_for_clip(
    img: &image::DynamicImage,
    face: &crate::face::DetectedFace,
) -> image::DynamicImage {
    let iw = img.width() as i32;
    let ih = img.height() as i32;
    let bw = (face.x2 - face.x1).max(1);
    let bh = (face.y2 - face.y1).max(1);
    let pad = (bw.max(bh) as f32 * 0.20) as i32;
    let x1 = (face.x1 - pad).max(0) as u32;
    let y1 = (face.y1 - pad).max(0) as u32;
    let x2 = (face.x2 + pad).min(iw) as u32;
    let y2 = (face.y2 + pad).min(ih) as u32;
    let w = x2.saturating_sub(x1).max(1);
    let h = y2.saturating_sub(y1).max(1);
    img.crop_imm(x1, y1, w, h)
}

/// Returns `true` if the detected face region looks like a real photographed human face.
///
/// Rejects:
///  1. Stone/bronze statues  → too dark overall + no sclera-white pixels
///  2. Yellow/green/blue cartoons → insufficient warm skin colour (wrong hue)
///  3. Ink drawings / cross-hatched cartoons → bimodal luminance
///     (lots of very-bright *paper* pixels AND lots of very-dark *ink* pixels)
///  4. Flat cream/pastel cartoons → skin pixels too uniform (no lighting variation)
///
/// Uses a tight crop of the face bbox (no background padding) to avoid
/// surrounding colours contaminating the analysis.
#[allow(dead_code)] // v1.5.19: intentionally kept for a future opt-in Settings toggle, see caller
fn photo_face_is_real(img: &image::DynamicImage, face: &crate::face::DetectedFace) -> bool {
    let iw = img.width() as i32;
    let ih = img.height() as i32;
    let tx1 = face.x1.max(0) as u32;
    let ty1 = face.y1.max(0) as u32;
    let tx2 = face.x2.min(iw) as u32;
    let ty2 = face.y2.min(ih) as u32;
    if tx2 <= tx1 || ty2 <= ty1 { return true; }  // degenerate box → allow

    let tight = img.crop_imm(tx1, ty1, tx2 - tx1, ty2 - ty1);
    let small = tight.resize(48, 48, image::imageops::FilterType::Nearest);
    let frgb  = small.to_rgb8();
    let n_px  = (frgb.width() * frgb.height()) as f32;

    let mut val_sum      = 0.0f64;
    let mut skin_warm    = 0u32;   // H 0-45°, S≥0.18, V≥0.38
    let mut white_px     = 0u32;   // V>0.72, S<0.18 (sclera / highlights)
    let mut bright_px    = 0u32;   // V>0.70 (paper-white in ink drawings)
    let mut dark_px      = 0u32;   // V<0.20 (ink lines)
    let mut skin_lum_sum = 0.0f32;
    let mut skin_lum_sq  = 0.0f32;

    for px in frgb.pixels() {
        let r = px[0] as f32 / 255.0;
        let g = px[1] as f32 / 255.0;
        let b = px[2] as f32 / 255.0;
        let cmax = r.max(g).max(b);
        let cmin = r.min(g).min(b);
        let delta = cmax - cmin;
        let s = if cmax > 0.001 { delta / cmax } else { 0.0 };
        let v = cmax;
        let h = if delta < 0.01 {
            0.0f32
        } else if cmax == r {
            60.0 * ((g - b) / delta).rem_euclid(6.0)
        } else if cmax == g {
            60.0 * ((b - r) / delta + 2.0)
        } else {
            60.0 * ((r - g) / delta + 4.0)
        };

        val_sum += v as f64;
        if v > 0.72 && s < 0.18  { white_px  += 1; }
        if v > 0.70               { bright_px += 1; }
        if v < 0.20               { dark_px   += 1; }
        if h <= 45.0 && s >= 0.18 && v >= 0.38 {
            skin_warm += 1;
            let lum = 0.299 * r + 0.587 * g + 0.114 * b;
            skin_lum_sum += lum;
            skin_lum_sq  += lum * lum;
        }
    }

    let avg_val     = (val_sum / n_px as f64) as f32;
    let warm_ratio  = skin_warm  as f32 / n_px;
    let white_ratio = white_px   as f32 / n_px;
    let bright_ratio = bright_px as f32 / n_px;
    let dark_ratio   = dark_px   as f32 / n_px;

    // Detect grayscale / black-and-white images by sampling overall saturation.
    // If the whole face crop is desaturated, the skin-warm check is meaningless
    // (B&W portraits, sepia, heavy stylisation) and we must NOT reject on it.
    // We recompute a cheap mean-saturation estimate here rather than thread a
    // second accumulator through the main pixel loop.
    let mut sat_sum = 0.0f32;
    for px in frgb.pixels() {
        let r = px[0] as f32 / 255.0;
        let g = px[1] as f32 / 255.0;
        let b = px[2] as f32 / 255.0;
        let cmax = r.max(g).max(b);
        let cmin = r.min(g).min(b);
        let s = if cmax > 0.001 { (cmax - cmin) / cmax } else { 0.0 };
        sat_sum += s;
    }
    let mean_sat = sat_sum / n_px;
    let is_grayscale = mean_sat < 0.08;

    // Luminance std-dev within skin-warm pixels only.
    // Real faces: ~0.05-0.15 (natural lighting).  Flat cartoons: ~0.00-0.03.
    let skin_lum_std = if skin_warm > 10 {
        let n = skin_warm as f32;
        let mean = skin_lum_sum / n;
        (skin_lum_sq / n - mean * mean).max(0.0).sqrt()
    } else {
        1.0  // insufficient skin pixels — let other checks decide
    };

    // HEURISTIC POLICY (v1.4.21): this fallback runs ONLY when CLIP isn't
    // installed. CLIP is the real filter — the heuristic just blocks the
    // most obvious cartoons. We keep it deliberately permissive because its
    // historical failure mode was rejecting real dark-skin / low-light /
    // desaturated faces, not letting too many cartoons through.
    //
    // 1. Stone / bronze statue (very dark AND no sclera whites).
    //    Skipped for grayscale to protect under-exposed B&W portraits.
    if !is_grayscale && avg_val < 0.35 && white_ratio < 0.02 { return false; }
    // 2. Cross-hatched ink drawing: lots of paper-bright pixels AND lots of
    //    pure-ink-dark pixels. Real photos have a continuous luminance
    //    distribution, drawings have a bimodal one. Tightened thresholds
    //    so normal high-contrast photos (sunlit faces on dark backgrounds)
    //    aren't accidentally flagged.
    if bright_ratio > 0.40 && dark_ratio > 0.22 { return false; }
    // 3. Extremely uniform skin-lum std → flat cream / pastel cartoon.
    //    Only triggers when there ARE enough warm-skin pixels to measure
    //    (skin_warm > 10); otherwise skin_lum_std defaults to 1.0 above.
    //    Threshold lowered from 0.03 → 0.015 to avoid killing smooth-lit
    //    real faces (e.g. studio flash on fair skin).
    if !is_grayscale && skin_warm > 40 && skin_lum_std < 0.015 { return false; }
    // NOTE: we intentionally DROPPED the `warm_ratio < 0.05` check that used
    // to be here. It was the single biggest cause of over-rejection: any
    // photo with cool lighting (overcast, shade, night, blue-hour) had
    // warm_ratio near zero even with a real face in frame. CLIP handles
    // cartoons far better than this ratio ever did.
    let _ = warm_ratio; // suppress unused-variable warning

    true
}

/// Detect faces in a photo and compute embeddings. Overwrites previous detections.
#[tauri::command]
pub async fn detect_faces_in_photo(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<Vec<FaceRegion>, String> {
    let photo_path: String = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        conn.query_row(
            "SELECT path FROM photos WHERE id = ?1",
            rusqlite::params![photo_id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?
    };

    let models_dir = models_dir_for(&app);
    let db_arc = state.db.clone();
    let thumbs_dir = state.thumbnails_dir.clone();
    let app_for_clip = app.clone();

    tokio::task::spawn_blocking(move || -> Result<Vec<FaceRegion>, String> {
        let face_models = crate::face::load_models(&models_dir)
            .map_err(|e| e.to_string())?;

        // Optional CLIP filter (same Level 2 pipeline as batch scan). Loaded
        // once per single-photo call. If CLIP isn't installed, clip_filter
        // stays None and the loop falls back to the heuristic check only.
        let mut clip_filter = try_load_clip_face_filter(&app_for_clip);

        // v1.5.38 — Use `crate::thumbnail::open_image` so EXIF orientation is
        // applied here too. The previous `image::open` left coordinates in
        // raw-pixel space while `get_photo_full` (lightbox display) applied
        // EXIF rotation, causing face boxes to land in completely the wrong
        // place on phone photos with orientation != 1. This was the user's
        // "kutular alakasız yerde çıkıyor" report.
        let img = crate::thumbnail::open_image(&photo_path)
            .map_err(|e| format!("Failed to open photo: {}", e))?;

        // ── Preserve existing NAMED face assignments ──────────────────────────
        // Collect bboxes of faces that already have a person assigned.
        // After re-detection we skip any new detection that overlaps these
        // (IoU > 0.4) so the user's assignments are never silently erased.
        let named_bboxes: Vec<(i32, i32, i32, i32)> = {
            let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
            conn.prepare(
                "SELECT x1, y1, x2, y2 FROM face_regions \
                 WHERE photo_id = ?1 AND person_id > 0"
            )
            .and_then(|mut s| {
                let v: Vec<(i32,i32,i32,i32)> = s.query_map(
                    rusqlite::params![photo_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                )?.filter_map(|r| r.ok()).collect();
                Ok(v)
            })
            .unwrap_or_default()
        };

        // Delete ONLY truly-unassigned faces — keep BOTH named (person_id > 0)
        // AND skipped (person_id = -1) rows.
        //
        // This is the fix for the "skipped faces keep returning" bug: every
        // time detection re-ran on a photo the previous DELETE (which used
        // `person_id IS NULL OR person_id <= 0`) wiped the `-1` sentinels,
        // so when `get_unknown_faces` ran its similarity-propagation against
        // skipped embeddings there was literally nothing left to match
        // against. Lowering the threshold / adding more propagation layers
        // could never fix this — the authoritative skipped row was being
        // deleted between runs. Keep it, and the existing 0.32 per-embedding
        // skip propagation in `get_unknown_faces` handles everything cleanly.
        {
            let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
            conn.execute(
                "DELETE FROM face_regions \
                 WHERE photo_id = ?1 AND person_id IS NULL",
                rusqlite::params![photo_id],
            ).ok();
        }

        let detected = crate::face::detect_faces(&face_models, &img)
            .map_err(|e| format!("Face detection error: {}", e))?;

        let faces_dir = thumbs_dir.join("faces");
        std::fs::create_dir_all(&faces_dir).ok();

        let mut result = Vec::new();

        for face in &detected {
            // v1.4.21: prefer CLIP when available; fall back to heuristic only
            // when CLIP isn't installed. Running both in series was killing
            // real faces — see detect_faces_background for details.
            if let Some((engine, label_embs)) = clip_filter.as_mut() {
                let clip_crop = crop_face_for_clip(&img, face);
                if !clip_face_is_real(engine, label_embs, &clip_crop) { continue; }
            } else if !photo_face_is_real(&img, face) {
                continue;
            }

            // Skip new detection if it overlaps a face the user already named
            let overlaps_named = named_bboxes.iter().any(|&(nx1, ny1, nx2, ny2)| {
                let ix1 = face.x1.max(nx1); let iy1 = face.y1.max(ny1);
                let ix2 = face.x2.min(nx2); let iy2 = face.y2.min(ny2);
                let inter = ((ix2-ix1).max(0) * (iy2-iy1).max(0)) as f32;
                let area_a = ((face.x2-face.x1)*(face.y2-face.y1)) as f32;
                let area_b = ((nx2-nx1)*(ny2-ny1)) as f32;
                let union = area_a + area_b - inter;
                union > 0.0 && inter/union > 0.4
            });
            if overlaps_named { continue; }

            let embedding = crate::face::get_embedding(&face_models, &img, face)
                .unwrap_or_default();
            let emb_bytes = crate::face::embedding_to_bytes(&embedding);

            let face_id = {
                let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                db::insert_face_region(
                    &conn, photo_id,
                    face.x1, face.y1, face.x2, face.y2,
                    face.score, &emb_bytes,
                )
                .map_err(|e| e.to_string())?
            };

            // Save 128×128 crop thumbnail
            let (iw, ih) = (img.width() as i32, img.height() as i32);
            let pad = ((face.x2 - face.x1).max(face.y2 - face.y1) / 5).max(8);
            let cx1 = (face.x1 - pad).max(0) as u32;
            let cy1 = (face.y1 - pad).max(0) as u32;
            let cx2 = (face.x2 + pad).min(iw) as u32;
            let cy2 = (face.y2 + pad).min(ih) as u32;
            let crop = img
                .crop_imm(cx1, cy1, cx2 - cx1, cy2 - cy1)
                .resize(128, 128, image::imageops::FilterType::Triangle);
            let thumb_path = faces_dir.join(format!("face_{}.jpg", face_id));
            crop.save_with_format(&thumb_path, image::ImageFormat::Jpeg).ok();

            let thumb_b64 = std::fs::read(&thumb_path)
                .ok()
                .map(|b| base64::engine::general_purpose::STANDARD.encode(b));

            result.push(FaceRegion {
                id: face_id,
                photo_id,
                x1: face.x1, y1: face.y1,
                x2: face.x2, y2: face.y2,
                score: face.score,
                person_id: None,
                person_name: None,
                thumbnail_b64: thumb_b64,
                cluster_face_ids: vec![face_id],
            });
        }

        Ok(result)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Return previously detected faces for a photo (with thumbnails).
#[tauri::command]
pub async fn get_faces_for_photo(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<FaceRegion>, String> {
    let (rows, thumbs_dir) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let rows = db::get_faces_for_photo(&conn, photo_id).map_err(|e| e.to_string())?;
        (rows, state.thumbnails_dir.clone())
    };
    let faces_dir = thumbs_dir.join("faces");
    Ok(rows
        .into_iter()
        .map(|r| FaceRegion {
            id: r.id,
            photo_id,
            x1: r.x1, y1: r.y1, x2: r.x2, y2: r.y2,
            score: r.score,
            person_id: r.person_id,
            person_name: r.person_name,
            thumbnail_b64: face_thumb_b64(&faces_dir, r.id),
            cluster_face_ids: vec![r.id],
        })
        .collect())
}

/// Create a new named person.
#[tauri::command]
pub async fn create_person(
    name: String,
    state: tauri::State<'_, AppState>,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::create_person(&conn, name.trim()).map_err(|e| e.to_string())
}

/// Persist the list of face IDs that were most recently shown in the popup
/// to the `settings` key-value table. The next call to `get_unknown_faces`
/// will read this list and auto-skip any IDs still marked person_id IS NULL,
/// which would otherwise come back up (DigiKam #444394-style re-prompting).
///
/// Mirrored to DB (not just in-memory) so that closing the app between a
/// scan and the "who is this?" popup doesn't cause previously-skipped faces
/// to be asked about on next launch.
fn persist_shown_face_ids(conn: &rusqlite::Connection, ids: &[i64]) {
    let json = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_string());
    // Upsert: overwrite the single row for this key.
    let _ = conn.execute(
        "INSERT INTO settings(key, value) VALUES('face:last_shown_ids', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![json],
    );
}

/// Get unassigned faces (unique per cluster) with thumbnails for "Who is this?" popup.
///
/// BULLETPROOF SKIP: On each call, any face IDs that were returned in the
/// PREVIOUS call but are still unassigned (person_id IS NULL) are automatically
/// skipped. This guarantees faces are never shown twice, regardless of whether
/// the JS skip handler works.
#[tauri::command]
pub async fn get_unknown_faces(
    folder: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<FaceRegion>, String> {
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    let thumbs_dir = state.thumbnails_dir.clone();
    let faces_dir = thumbs_dir.join("faces");
    let folder_filter: Option<String> = folder
        .as_ref()
        .and_then(|s| if s.trim().is_empty() { None } else { Some(s.clone()) });
    if let Some(f) = &folder_filter {
        eprintln!("[face] get_unknown_faces: scoped to folder = {}", f);
    }

    // ── Step 1: Auto-skip faces from PREVIOUS call that weren't named ────
    //
    // Source the list from BOTH in-memory (same-session) and a DB-persisted
    // mirror (survives app restart). Pre-1.4.4 only used in-memory, which
    // meant: user skips cluster C → quits the app → reopens → cluster C
    // is asked about AGAIN because restart lost the in-memory record.
    // Persisting to the `settings` k/v table fixes that.
    {
        let mut prev_ids: Vec<i64> = state
            .last_shown_face_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if prev_ids.is_empty() {
            if let Ok(json) = conn.query_row::<String, _, _>(
                "SELECT value FROM settings WHERE key = 'face:last_shown_ids'",
                [],
                |r| r.get(0),
            ) {
                if let Ok(ids) = serde_json::from_str::<Vec<i64>>(&json) {
                    prev_ids = ids;
                }
            }
        }
        if !prev_ids.is_empty() {
            let mut skipped = 0usize;
            for fid in prev_ids.iter() {
                let updated = conn.execute(
                    "UPDATE face_regions SET person_id = -1 WHERE id = ?1 AND person_id IS NULL",
                    rusqlite::params![fid],
                ).unwrap_or(0);
                skipped += updated;
            }
            if skipped > 0 {
                eprintln!("[face] Auto-skipped {} previously-shown faces that weren't named", skipped);
            }
        }
    }

    // ── Step 2a: Flat list of EVERY skipped embedding ──────────────────
    // v1.4.28 — SWITCHED FROM CENTROID TO PER-EMBEDDING DIRECT MATCHING.
    //
    // Previous approach (v1.4.21-v1.4.27): cluster the skipped faces, then
    // match new faces against each CLUSTER CENTROID. Intuition was that the
    // centroid is more stable than any single face. But it has a fatal edge
    // case: when the user has only skipped ONE photo of person X, the
    // "cluster" for X is a singleton → centroid = the single embedding. A
    // new photo of the same person X from a different angle/lighting has
    // cosine similarity 0.32-0.42 with that one embedding — not high enough
    // for centroid-based thresholds to catch, so X re-surfaces in the popup.
    //
    // User's bug report (screenshot after v1.4.25 release, still broken):
    // "Skip dediğim kadını ikinci batch'de tekrar sordu! ya çüş artık ya!"
    //
    // Fix: keep ALL skipped embeddings flat (no clustering on our side) and
    // match each new face against EVERY skipped embedding individually,
    // taking the MAX similarity. With a threshold of ~0.32 we reliably
    // catch subsequent photos of the same person — safely above the
    // 0.1-0.25 different-identity baseline and below the 0.40+ same-person
    // floor typical of InsightFace embeddings.
    let skipped_embs: Vec<(i64, Vec<f32>)> = {
        let mut stmt = conn.prepare(
            "SELECT id, embedding FROM face_regions
             WHERE person_id = -1 AND embedding IS NOT NULL"
        ).map_err(|e| e.to_string())?;
        let raw: Vec<(i64, Vec<u8>)> = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        raw.into_iter()
            .map(|(id, bytes)| (id, crate::face::bytes_to_embedding(&bytes)))
            .filter(|(_, e)| e.len() == 512)
            .collect()
    };

    // ── Step 2b: Build per-person CENTROIDS for known persons ───────────
    // Same principle: matching against a single old photo is fragile;
    // matching against the centroid of all named photos is much more
    // stable. immich/PhotoPrism both use centroid-based recognition.
    let known_centroids: Vec<(i64, String, Vec<f32>, usize)> = {
        let mut stmt = conn.prepare(
            "SELECT p.id, p.name, f.embedding
             FROM face_regions f
             JOIN persons p ON p.id = f.person_id
             WHERE f.person_id > 0 AND f.embedding IS NOT NULL"
        ).map_err(|e| e.to_string())?;
        let raw: Vec<(i64, String, Vec<u8>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

        let mut map: std::collections::HashMap<i64, (String, Vec<Vec<f32>>)> =
            std::collections::HashMap::new();
        for (pid, name, bytes) in raw {
            let emb = crate::face::bytes_to_embedding(&bytes);
            if emb.len() != 512 { continue; }
            map.entry(pid).or_insert_with(|| (name, Vec::new())).1.push(emb);
        }
        map.into_iter()
            .map(|(pid, (name, embs))| {
                let n = embs.len();
                let c = crate::face::compute_centroid(&embs);
                (pid, name, c, n)
            })
            .filter(|(_, _, c, _)| c.len() == 512)
            .collect()
    };

    eprintln!(
        "[face] get_unknown_faces: {} skipped embeddings, {} known persons",
        skipped_embs.len(),
        known_centroids.len(),
    );

    // ── Step 3: Get all remaining unassigned faces (optionally folder-scoped)
    let rows: Vec<(i64, i64, Vec<u8>)> = match &folder_filter {
        Some(f) => db::get_unassigned_faces_with_embeddings_in_folder(&conn, f)
            .map_err(|e| e.to_string())?,
        None => db::get_unassigned_faces_with_embeddings(&conn)
            .map_err(|e| e.to_string())?,
    };

    if rows.is_empty() {
        *state.last_shown_face_ids.lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
        persist_shown_face_ids(&conn, &[]);
        return Ok(vec![]);
    }

    // ── Step 4: Per-embedding direct match for skip, centroid for assign ─
    //
    // ASSIGN to known person (silent, no prompt):
    //   - centroid sim ≥ 0.65
    //   - margin over 2nd-best person ≥ 0.10
    //   - known_n ≥ 2 (singleton centroids too fragile to auto-assign)
    //   - must beat best_skip_sim (never override a skip signal)
    //
    // SKIP (silent, no prompt):
    //   - MAX cosine similarity against ANY individual skipped embedding ≥ 0.32
    //   - best_skip_sim ≥ best_known_sim (known person wins ties)
    //
    // v1.4.28 — Direct per-embedding matching replaces clustered centroid.
    // See the comment at Step 2a above for the full rationale. A threshold
    // of 0.32 is intentionally aggressive: different-identity cos sim is
    // typically 0.10-0.25, same-identity 0.40-0.80. 0.32 sits safely in
    // the no-man's-land between, so we catch same-person-different-photo
    // matches that came in at 0.35-0.40 (where the singleton centroid
    // approach failed) without triggering on genuinely different people.
    const ASSIGN_CENTROID: f32 = 0.65;
    const ASSIGN_MARGIN:   f32 = 0.10;
    // v1.5.29 — lowered 0.32 → 0.27 to match the insert-time filter's new
    // 0.25 flat gate (with a slight margin). Same rationale: pose / lighting
    // variation of the same skipped person dips into 0.25–0.31, and a
    // singleton skip's "cluster" is just that one emb so the flat check is
    // the only safety net. 0.27 stays above the 0.10–0.22 different-identity
    // noise floor.
    const SKIP_THRESH:     f32 = 0.27;

    let mut remaining: Vec<(i64, Vec<f32>)> = Vec::new();
    let mut auto_assigned = 0usize;
    let mut auto_skipped  = 0usize;

    for (fid, photo_id, emb_bytes) in &rows {
        let emb = crate::face::bytes_to_embedding(emb_bytes);
        if emb.len() != 512 { continue; }

        // Best known-person centroid match (carry face-count for gating)
        let mut known_scored: Vec<(i64, &str, f32, usize)> = known_centroids.iter()
            .map(|(pid, name, c, n)| (*pid, name.as_str(), crate::face::cosine_similarity(&emb, c), *n))
            .collect();
        known_scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        let best_known_sim  = known_scored.first().map(|s| s.2).unwrap_or(0.0);
        let best_known_n    = known_scored.first().map(|s| s.3).unwrap_or(0);
        let second_known    = known_scored.get(1).map(|s| s.2).unwrap_or(0.0);

        // Max cosine similarity against ANY individual skipped embedding
        // (flat match — no clustering). Catches same-person-different-photo
        // at the 0.32-0.45 range that centroid-based matching missed.
        let mut best_skip_sim = 0.0f32;
        for (_skip_id, skip_emb) in &skipped_embs {
            let sim = crate::face::cosine_similarity(&emb, skip_emb);
            if sim > best_skip_sim { best_skip_sim = sim; }
        }

        // Silent auto-assign requires: strong match, clear margin over
        // second-best person, no competing skip signal, AND at least 2
        // faces in the person centroid (single-photo centroids are too
        // fragile for silent assignment).
        let assign_ok = best_known_sim >= ASSIGN_CENTROID
            && (best_known_sim - second_known) >= ASSIGN_MARGIN
            && best_known_sim > best_skip_sim
            && best_known_n >= 2;
        let skip_ok = best_skip_sim >= SKIP_THRESH
            && best_skip_sim >= best_known_sim;

        match (assign_ok, skip_ok) {
            (true, _) => {
                if let Some(&(pid, name, _, _)) = known_scored.first() {
                    db::assign_face_to_person(&conn, *fid, Some(pid)).ok();
                    db::insert_tags(
                        &conn, *photo_id,
                        &[(name.to_string(), best_known_sim as f64, "face".to_string())],
                    ).ok();
                    auto_assigned += 1;
                    continue;
                }
            }
            (false, true) => {
                conn.execute(
                    "UPDATE face_regions SET person_id = -1 WHERE id = ?1 AND person_id IS NULL",
                    rusqlite::params![fid],
                ).ok();
                auto_skipped += 1;
                continue;
            }
            (false, false) => {}
        }
        remaining.push((*fid, emb));
    }

    if auto_assigned > 0 || auto_skipped > 0 {
        eprintln!("[face] Auto-handled before popup: {} assigned to known persons, {} skipped as similar-to-skipped",
                  auto_assigned, auto_skipped);
    }

    if remaining.is_empty() {
        *state.last_shown_face_ids.lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
        persist_shown_face_ids(&conn, &[]);
        return Ok(vec![]);
    }

    // ── Step 5: Cluster remaining faces ──────────────────────────────────
    let clusters = crate::face::cluster_embeddings(&remaining);

    // ── Step 6: Build result + track shown face IDs ─────────────────────
    let mut result = Vec::new();
    let mut all_shown_ids: Vec<i64> = Vec::new();

    for cluster in clusters.iter().take(20) {
        // Track ALL face IDs in this cluster for auto-skip next time
        all_shown_ids.extend_from_slice(&cluster.face_ids);

        let mut chosen_id = cluster.representative;
        if face_thumb_b64(&faces_dir, chosen_id).is_none() {
            for &fid in &cluster.face_ids {
                if face_thumb_b64(&faces_dir, fid).is_some() {
                    chosen_id = fid;
                    break;
                }
            }
        }
        if let Ok(Some(face)) = db::get_face_region(&conn, chosen_id) {
            result.push(FaceRegion {
                id: face.id,
                photo_id: face.photo_id,
                x1: face.x1, y1: face.y1, x2: face.x2, y2: face.y2,
                score: face.score,
                person_id: None,
                person_name: None,
                thumbnail_b64: face_thumb_b64(&faces_dir, face.id),
                cluster_face_ids: cluster.face_ids.clone(),
            });
        }
    }

    // Save shown IDs — next call will auto-skip any that weren't named.
    // Also mirror to the DB so app restart doesn't forget.
    persist_shown_face_ids(&conn, &all_shown_ids);
    *state.last_shown_face_ids.lock().unwrap_or_else(|e| e.into_inner()) = all_shown_ids;

    eprintln!("[face] get_unknown_faces: returning {} clusters", result.len());
    Ok(result)
}

/// Name a face (and optionally an entire cluster) and aggressively propagate
/// the name to all visually-similar unassigned faces.
///
/// `face_id`            — the face the user clicked Save on (representative)
/// `cluster_face_ids`   — OPTIONAL: every face in the same cluster as the
///                        representative. If provided, ALL of them are
///                        assigned to the person (the clustering already
///                        decided they're the same person), and the union
///                        of their embeddings drives propagation. This stops
///                        the same person from re-appearing in subsequent
///                        batches just because the representative had a
///                        slightly different angle than other cluster members.
#[tauri::command]
pub async fn name_face_and_propagate(
    face_id: i64,
    name: String,
    cluster_face_ids: Option<Vec<i64>>,
    // v1.5.40 — When provided, propagation is restricted to photos whose
    // folder matches (or is a sub-path of) this string. Lets the lightbox
    // pass the current photo's folder so naming someone at a wedding
    // doesn't accidentally tag matches from other events/years too. None
    // keeps the previous library-wide behaviour.
    folder_scope: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Name cannot be empty".into());
    }

    let db = state.db.clone();

    tokio::task::spawn_blocking(move || {
        let conn = db.lock().unwrap_or_else(|e| e.into_inner());

        // Create person if doesn't exist, or get existing
        let (person_id, was_new_person) = match db::find_person_by_name(&conn, &name) {
            Ok(Some(pid)) => (pid, false),
            _ => (db::create_person(&conn, &name).map_err(|e| e.to_string())?, true),
        };

        // Build seed set: representative face + every cluster member.
        // Clustering already grouped them as the same person, so trust it.
        let mut seed_ids: Vec<i64> = vec![face_id];
        if let Some(ref ids) = cluster_face_ids {
            for fid in ids {
                if !seed_ids.contains(fid) { seed_ids.push(*fid); }
            }
        }

        // Assign every seed face to the person + tag its photo
        let mut named_count = 0usize;
        for fid in &seed_ids {
            db::assign_face_to_person(&conn, *fid, Some(person_id)).ok();
            if let Ok(Some(face)) = db::get_face_region(&conn, *fid) {
                db::insert_tags(
                    &conn, face.photo_id,
                    &[(name.clone(), 1.0, "face".to_string())],
                ).ok();
                named_count += 1;
            }
        }

        // Collect embeddings for all seed faces (used for propagation)
        let seed_embs: Vec<Vec<f32>> = {
            let mut out = Vec::new();
            let mut stmt = match conn.prepare(
                "SELECT embedding FROM face_regions WHERE id = ?1 AND embedding IS NOT NULL"
            ) {
                Ok(s) => s,
                Err(_) => return Ok(serde_json::json!({
                    "total": named_count,
                    "person_id": person_id,
                    "was_new_person": was_new_person,
                })),
            };
            for fid in &seed_ids {
                if let Ok(bytes) = stmt.query_row(
                    rusqlite::params![fid], |r| r.get::<_, Vec<u8>>(0)
                ) {
                    let emb = crate::face::bytes_to_embedding(&bytes);
                    if emb.len() == 512 { out.push(emb); }
                }
            }
            out
        };

        if seed_embs.is_empty() {
            return Ok(serde_json::json!({
                "total": named_count,
                "person_id": person_id,
                "was_new_person": was_new_person,
            }));
        }

        // Aggressive propagation: take the MAX similarity to any seed.
        // 0.40 is loose enough to catch the same person across angles/lighting
        // but tight enough to avoid wrong matches. The user explicitly does
        // not want to be re-asked about people they've already named.
        const NAME_PROPAGATE_THRESH: f32 = 0.40;
        // v1.5.40 — Honor the optional folder scope so naming a face at one
        // event doesn't propagate the tag onto unrelated photos in other
        // folders. The folder-scoped DB query joins photos and filters by
        // exact folder match OR path prefix (covers subfolders).
        // v1.5.43 — When folder-scoped, ALSO include faces the user had
        // previously skipped (person_id = -1). The user's explicit name
        // in the lightbox is the strongest signal we get; if those skipped
        // faces happen to look like the named person, they almost certainly
        // ARE that person and should be retagged. The library-wide path
        // (no folder scope, used by the bulk cluster popup) keeps the old
        // NULL-only behavior so skipping in that flow stays sticky.
        let unassigned = match &folder_scope {
            Some(f) if !f.is_empty() => {
                db::get_propagatable_faces_with_embeddings_in_folder(&conn, f)
                    .map_err(|e| e.to_string())?
            }
            _ => db::get_unassigned_faces_with_embeddings(&conn)
                .map_err(|e| e.to_string())?,
        };
        let mut matched = 0usize;
        for (fid, photo_id, emb_bytes) in &unassigned {
            let emb = crate::face::bytes_to_embedding(emb_bytes);
            if emb.len() != 512 { continue; }
            let best_sim = seed_embs.iter()
                .map(|s| crate::face::cosine_similarity(s, &emb))
                .fold(0.0f32, f32::max);
            if best_sim >= NAME_PROPAGATE_THRESH {
                db::assign_face_to_person(&conn, *fid, Some(person_id)).ok();
                db::insert_tags(
                    &conn, *photo_id,
                    &[(name.clone(), best_sim as f64, "face".to_string())],
                ).ok();
                matched += 1;
            }
        }
        eprintln!(
            "[face] name_face_and_propagate '{}': {} seed(s) named + {} propagated (thresh {}, scope={})",
            name, named_count, matched, NAME_PROPAGATE_THRESH,
            folder_scope.as_deref().unwrap_or("<library>")
        );
        Ok(serde_json::json!({
            "total": named_count + matched,
            "person_id": person_id,
            "was_new_person": was_new_person,
        }))
    }).await.map_err(|e| e.to_string()).and_then(|r| r)
}

/// Suggest likely existing persons for an unknown face. Returns up to
/// `max_results` named persons whose max-similarity to this face's
/// embedding is ≥ SUGGEST_SIM_MIN, sorted by similarity desc.
///
/// Rationale: recognize_all_faces uses sim ≥ 0.55 (strict) + margin + 2-of-N
/// agreement to auto-assign silently. That threshold is deliberately high so
/// we don't mis-attribute. But there's a big middle band (0.35–0.55) where
/// the system is "fairly sure" but not sure enough to auto-assign — and in
/// that band the popup currently makes the user retype the name from scratch.
/// Surfacing those as one-click chips above the input cuts a 5–10s typing
/// loop down to one click for every returning face. Huge UX win for any
/// library with more than a handful of named people.
///
/// Returns shape: [{ person_id: i64, name: String, similarity: f32 }]
#[tauri::command]
pub async fn suggest_face_matches(
    face_id: i64,
    max_results: Option<usize>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<serde_json::Value>, String> {
    // Below recognition threshold (0.55) but above noise floor.
    // Cosine ≥ 0.35 means "plausibly the same person" — suggestive, not
    // conclusive. The user makes the final call with a click.
    const SUGGEST_SIM_MIN: f32 = 0.35;
    let max_results = max_results.unwrap_or(3).max(1).min(8);

    let db = state.db.clone();
    tokio::task::spawn_blocking(move || {
        let conn = db.lock().unwrap_or_else(|e| e.into_inner());

        // 1. Load the unknown face's embedding.
        let face_bytes: Vec<u8> = match conn.query_row(
            "SELECT embedding FROM face_regions
             WHERE id = ?1 AND embedding IS NOT NULL",
            rusqlite::params![face_id],
            |r| r.get(0),
        ) {
            Ok(b) => b,
            Err(_) => return Ok(Vec::new()), // no embedding → no suggestions
        };
        let emb = crate::face::bytes_to_embedding(&face_bytes);
        if emb.len() != 512 {
            return Ok(Vec::new());
        }

        // 2. Pull every named face embedding with its person_id + name.
        //    person_id > 0 excludes both NULL (unknown) and -1 (skipped).
        let rows: Vec<(i64, String, Vec<u8>)> = {
            let mut stmt = match conn.prepare(
                "SELECT fr.person_id, p.name, fr.embedding
                 FROM face_regions fr
                 JOIN persons p ON p.id = fr.person_id
                 WHERE fr.person_id > 0
                   AND fr.embedding IS NOT NULL"
            ) {
                Ok(s) => s,
                Err(e) => return Err(e.to_string()),
            };
            let iter = match stmt.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Vec<u8>>(2)?))
            }) {
                Ok(i) => i,
                Err(e) => return Err(e.to_string()),
            };
            iter.filter_map(|r| r.ok()).collect()
        };

        if rows.is_empty() {
            return Ok(Vec::new());
        }

        // 3. Max-sim per person (pose-robust, same logic as recognize_all_faces).
        let mut per_person: std::collections::HashMap<i64, (String, f32)> =
            std::collections::HashMap::new();
        for (pid, pname, bytes) in rows {
            let ne = crate::face::bytes_to_embedding(&bytes);
            if ne.len() != 512 { continue; }
            let sim = crate::face::cosine_similarity(&emb, &ne);
            let entry = per_person.entry(pid).or_insert((pname, f32::NEG_INFINITY));
            if sim > entry.1 { entry.1 = sim; }
        }

        // 4. Filter by SUGGEST_SIM_MIN, sort by similarity desc, cap.
        let mut scored: Vec<(i64, String, f32)> = per_person
            .into_iter()
            .filter(|(_, (_, s))| *s >= SUGGEST_SIM_MIN)
            .map(|(id, (name, s))| (id, name, s))
            .collect();
        scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(max_results);

        Ok(scored.into_iter()
            .map(|(id, name, sim)| serde_json::json!({
                "person_id": id,
                "name": name,
                "similarity": sim,
            }))
            .collect())
    }).await.map_err(|e| e.to_string()).and_then(|r| r)
}

/// List all persons with face counts and representative thumbnail.
#[tauri::command]
pub async fn get_persons(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<Person>, String> {
    let (rows, thumbs_dir) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let rows = db::get_persons(&conn).map_err(|e| e.to_string())?;
        (rows, state.thumbnails_dir.clone())
    };
    let faces_dir = thumbs_dir.join("faces");
    Ok(rows
        .into_iter()
        .map(|r| {
            // Use thumbnail file name stored in DB, or fall back to any face of this person
            let thumbnail = r.thumbnail
                .as_deref()
                .and_then(|t| std::fs::read(faces_dir.join(t)).ok())
                .map(|b| base64::engine::general_purpose::STANDARD.encode(b));
            Person {
                id: r.id,
                name: r.name,
                thumbnail,
                face_count: r.face_count,
            }
        })
        .collect())
}

/// Assign (or unassign) a detected face to a person.
#[tauri::command]
pub async fn assign_face_to_person(
    face_id: i64,
    person_id: Option<i64>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::assign_face_to_person(&conn, face_id, person_id).map_err(|e| e.to_string())?;

    // Update person thumbnail if assigning (use this face as representative)
    if let Some(pid) = person_id {
        let thumb_name = format!("face_{}.jpg", face_id);
        db::update_person_thumbnail(&conn, pid, &thumb_name).ok();
    }
    Ok(())
}

/// Delete a person (faces become unassigned; tags are NOT auto-removed).
#[tauri::command]
pub async fn delete_person(
    person_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::delete_person(&conn, person_id).map_err(|e| e.to_string())
}

/// Rename a person.
#[tauri::command]
pub async fn rename_person(
    person_id: i64,
    new_name: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::rename_person(&conn, person_id, new_name.trim()).map_err(|e| e.to_string())
}

/// Assign a person (by name — created if missing) to every photo in
/// `photo_ids`. Always inserts a name tag (kind="face", conf=1.0) so the
/// photo becomes searchable under that name. If the photo has a face region
/// with no person yet, the largest such region is also linked to the person
/// so the face thumbnails on the person's detail page include this photo.
/// Returns a summary object with counts.
#[tauri::command]
pub async fn batch_assign_person(
    photo_ids: Vec<i64>,
    person_name: String,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let name = person_name.trim().to_string();
    if name.is_empty() {
        return Err("Person name cannot be empty".into());
    }
    if photo_ids.is_empty() {
        return Err("No photos selected".into());
    }

    let conn = state.db.lock().map_err(|_| "db lock")?;

    // Resolve or create the person.
    let (person_id, was_new_person) = match db::find_person_by_name(&conn, &name) {
        Ok(Some(pid)) => (pid, false),
        _ => (
            db::create_person(&conn, &name).map_err(|e| e.to_string())?,
            true,
        ),
    };

    let mut tagged = 0usize;
    let mut faces_assigned = 0usize;
    let first_thumb_name: Option<String> = None;

    for pid in &photo_ids {
        // Always add/update the person-name tag so search works even on
        // photos without detected faces.
        if db::insert_tags(
            &conn,
            *pid,
            &[(name.clone(), 1.0, "face".to_string())],
        ).is_ok() {
            tagged += 1;
        }

        // Try to attach the largest unassigned face region (if any) to this
        // person. Keeps our face index coherent: once a face is bound, the
        // person's detail page will include this photo.
        let face_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM face_regions
                 WHERE photo_id = ?1
                   AND (person_id IS NULL OR person_id <= 0)
                 ORDER BY ((x2 - x1) * (y2 - y1)) DESC
                 LIMIT 1",
                rusqlite::params![pid],
                |r| r.get::<_, i64>(0),
            )
            .ok();

        if let Some(fid) = face_id {
            if db::assign_face_to_person(&conn, fid, Some(person_id)).is_ok() {
                faces_assigned += 1;
            }
        }
    }

    // Silence unused-variable warning — placeholder kept for potential
    // future avatar promotion once face_regions has a thumb column.
    let _ = first_thumb_name;

    Ok(serde_json::json!({
        "person_id": person_id,
        "person_name": name,
        "was_new_person": was_new_person,
        "photos_tagged": tagged,
        "faces_assigned": faces_assigned,
        "total": photo_ids.len(),
    }))
}

/// Overwrite a photo's user-editable description. Empty string clears it.
/// The DB helper keeps the FTS5 index in sync so search works immediately.
#[tauri::command]
pub async fn set_photo_description(
    photo_id: i64,
    description: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    // Trim but preserve interior whitespace — users may write multi-line notes.
    let trimmed = description.trim();
    db::update_photo_description(&conn, photo_id, trimmed).map_err(|e| e.to_string())
}

/// Merge `from_person_id` into `into_person_id`: all faces are reassigned and
/// the source person is deleted. Returns the number of faces moved.
#[tauri::command]
pub async fn merge_persons(
    from_person_id: i64,
    into_person_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::merge_persons(&conn, from_person_id, into_person_id).map_err(|e| e.to_string())
}

/// Chronological timeline for a single person — photo ids sorted by
/// date_taken. Used to render the face-aging timeline UI.
#[tauri::command]
pub async fn get_person_timeline(
    person_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<(i64, Option<String>)>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_person_timeline(&conn, person_id).map_err(|e| e.to_string())
}

/// Auto-recognize all unassigned faces by comparing to known person embeddings.
/// Returns number of faces that were matched.
///
/// ## Why max-sim-to-any-named-face instead of mean-embedding
///
/// Mean-embedding collapses a person's pose/lighting diversity into one vector.
/// If Mom has 5 frontal + 2 profile named faces, the mean drifts toward frontal;
/// a new profile shot has cos sim ~0.48 to the mean, below the 0.60 threshold —
/// so it fails to auto-match and the user gets asked again. Comparing the new
/// face to **each named face individually** and taking the MAX recovers 0.70+
/// sim against the 2 named profiles, giving a clean auto-assign.
///
/// This is the same strategy used in `name_face_and_propagate` (which is the
/// reason explicit naming feels instant — it uses max-sim over per-cluster
/// seeds). Using it here closes the asymmetry where naming works great but
/// subsequent scans re-ask about already-named people.
#[tauri::command]
pub async fn recognize_all_faces(
    folder: Option<String>,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<usize, String> {
    // v1.5.74 — Was a P1 freeze: this fan-out (cosine sim across all faces
    // × all persons, plus per-match db.lock() + insert_tags + emit) ran
    // inline on the async-runtime worker thread. On a 50K-photo / 5K-face
    // library it took minutes and saturated the same worker pool that
    // dispatches every other IPC. The whole UI hung until recognition
    // finished. Now wrapped in spawn_blocking so the runtime keeps
    // dispatching while we churn.
    let folder_filter: Option<String> = folder
        .as_ref()
        .and_then(|s| if s.trim().is_empty() { None } else { Some(s.clone()) });
    let db_arc = state.db.clone();

    tauri::async_runtime::spawn_blocking(move || -> Result<usize, String> {
        let known = {
            let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
            db::get_known_face_embeddings(&conn).map_err(|e| e.to_string())?
        };

        if known.is_empty() {
            // No known persons yet — nothing to recognize against, that's fine.
            return Ok(0);
        }

        // Group all named embeddings by person_id, keeping EVERY embedding
        // individually (no averaging). Each person now has a list of
        // reference vectors covering their pose/lighting variation.
        let mut person_map: std::collections::HashMap<i64, (String, Vec<Vec<f32>>)> =
            std::collections::HashMap::new();
        for (pid, pname, bytes) in &known {
            let emb = crate::face::bytes_to_embedding(bytes);
            if emb.len() != 512 { continue; }
            person_map
                .entry(*pid)
                .or_insert_with(|| (pname.clone(), Vec::new()))
                .1.push(emb);
        }
        let persons: Vec<(i64, String, Vec<Vec<f32>>)> = person_map
            .into_iter()
            .filter_map(|(pid, (name, embs))| {
                if embs.is_empty() { None } else { Some((pid, name, embs)) }
            })
            .collect();

        let unassigned = {
            let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
            match &folder_filter {
                Some(f) => db::get_unassigned_faces_with_embeddings_in_folder(&conn, f)
                    .map_err(|e| e.to_string())?,
                None => db::get_unassigned_faces_with_embeddings(&conn)
                    .map_err(|e| e.to_string())?,
            }
        };

        // Recognition thresholds — calibrated for the max-sim-to-any-named-
        // face strategy. Much more forgiving than the 0.60 mean-embedding
        // threshold (0.60 against a mean corresponds to ~0.70+ against the
        // closest seed for typical 5-10 photo collections).
        //
        //   RECOGNIZE_SIM    — best sim to ANY named face of the winner.
        //   RECOGNIZE_MARGIN — winner must beat runner-up by this much
        //                       (prevents ambiguous assignments).
        //   AGREE_SIM        — "agreement" threshold for the 2-of-N rule.
        //   AGREE_MIN_PERSON — min named-face count before agreement kicks in.
        const RECOGNIZE_SIM:    f32   = 0.55;
        const RECOGNIZE_MARGIN: f32   = 0.08;
        const AGREE_SIM:        f32   = 0.45;
        const AGREE_MIN_PERSON: usize = 3;

        let mut matched = 0usize;

        for (face_id, photo_id, emb_bytes) in &unassigned {
            let emb = crate::face::bytes_to_embedding(emb_bytes);
            if emb.len() != 512 { continue; }

            let mut scored: Vec<(i64, &str, f32, usize, usize)> = persons.iter()
                .map(|(pid, name, embs)| {
                    let mut best = f32::NEG_INFINITY;
                    let mut support = 0usize;
                    for e in embs {
                        let sim = crate::face::cosine_similarity(&emb, e);
                        if sim > best { best = sim; }
                        if sim >= AGREE_SIM { support += 1; }
                    }
                    (*pid, name.as_str(), best, embs.len(), support)
                })
                .collect();
            scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

            let (pid, person_name, best_sim, named_count, support) = match scored.first() {
                Some(&t) => t,
                None => continue,
            };
            let runner_up = scored.get(1).map(|s| s.2).unwrap_or(0.0);

            let agreement_ok = if named_count >= AGREE_MIN_PERSON {
                support >= 2
            } else {
                true
            };

            let ok = best_sim >= RECOGNIZE_SIM
                && (best_sim - runner_up) >= RECOGNIZE_MARGIN
                && agreement_ok;
            if !ok { continue; }

            eprintln!(
                "[face] recognize: face {} -> '{}' sim={:.4} (2nd={:.4}, {}/{} support)",
                face_id, person_name, best_sim, runner_up, support, named_count
            );
            {
                let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
                db::assign_face_to_person(&conn, *face_id, Some(pid)).ok();
                db::insert_tags(
                    &conn,
                    *photo_id,
                    &[(person_name.to_string(), best_sim as f64, "face".to_string())],
                )
                .ok();
            }
            matched += 1;
            app_handle
                .emit(
                    "face-recognized",
                    serde_json::json!({
                        "face_id": face_id,
                        "photo_id": photo_id,
                        "person": person_name,
                        "similarity": best_sim
                    }),
                )
                .ok();
        }

        Ok(matched)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Download face recognition models to the models directory.
#[tauri::command]
pub async fn download_face_models(app: tauri::AppHandle) -> Result<String, String> {
    let models_dir = models_dir_for(&app);
    std::fs::create_dir_all(&models_dir).map_err(|e| e.to_string())?;

    let det_path = models_dir.join("det_500m.onnx");
    let emb_path = models_dir.join("w600k_mbf.onnx");

    if det_path.exists() && emb_path.exists() {
        return Ok("Models already exist.".to_string());
    }

    // Download buffalo_s zip from InsightFace releases
    let zip_url = "https://github.com/deepinsight/insightface/releases/download/v0.7/buffalo_s.zip";
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;

    let zip_bytes = client
        .get(zip_url)
        .header("User-Agent", "RetinaTag/1.1")
        .send()
        .await
        .map_err(|e| format!("Download failed: {}", e))?
        .bytes()
        .await
        .map_err(|e| format!("Download could not be completed: {}", e))?;

    // Extract the two ONNX files from the zip
    let cursor = std::io::Cursor::new(&zip_bytes[..]);
    let mut zip = zip::ZipArchive::new(cursor)
        .map_err(|e| format!("Failed to open zip: {}", e))?;

    let mut det_found = false;
    let mut emb_found = false;

    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = file.name().to_string();
        if name.ends_with("det_500m.onnx") {
            let mut out = std::fs::File::create(&det_path).map_err(|e| e.to_string())?;
            std::io::copy(&mut file, &mut out).map_err(|e| e.to_string())?;
            det_found = true;
        } else if name.ends_with("w600k_mbf.onnx") {
            let mut out = std::fs::File::create(&emb_path).map_err(|e| e.to_string())?;
            std::io::copy(&mut file, &mut out).map_err(|e| e.to_string())?;
            emb_found = true;
        }
        if det_found && emb_found {
            break;
        }
    }

    if det_found && emb_found {
        Ok("Face recognition models downloaded successfully!".to_string())
    } else {
        Err("Model files not found inside zip.".to_string())
    }
}

/// Check if face models are present.
/// Accepts EITHER w600k_r50.onnx (higher-accuracy preferred variant) OR
/// w600k_mbf.onnx (MobileFaceNet fallback) — both are valid embedders.
#[tauri::command]
pub async fn check_face_models(app: tauri::AppHandle) -> Result<bool, String> {
    let models_dir = models_dir_for(&app);
    let has_det = models_dir.join("det_500m.onnx").exists();
    let has_emb = models_dir.join("w600k_r50.onnx").exists()
        || models_dir.join("w600k_mbf.onnx").exists();
    Ok(has_det && has_emb)
}

// ── 14b. Face Clustering ──────────────────────────────────────────────────────

/// Represents one face cluster returned to the frontend.
#[derive(serde::Serialize)]
pub struct FaceClusterResult {
    /// Thumbnails of a few representative faces (base64 JPEG), max 4.
    pub thumbnails: Vec<String>,
    /// face_ids in this cluster.
    pub face_ids: Vec<i64>,
    /// photo_ids corresponding to face_ids.
    pub photo_ids: Vec<i64>,
    /// How many faces are in this cluster.
    pub count: usize,
    /// If already assigned to a person, their name.
    pub person_name: Option<String>,
    /// If already assigned to a person, their id.
    pub person_id: Option<i64>,
}

/// Scan every photo in a folder (or whole library if folder="") for faces,
/// then cluster them. Returns clusters for the user to name.
/// NOTE: only detects faces in photos that don't already have face data.
#[tauri::command]
pub async fn scan_and_cluster_faces(
    folder: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<Vec<FaceClusterResult>, String> {
    let models_dir = models_dir_for(&app);

    // Check models
    if !models_dir.join("det_500m.onnx").exists() {
        return Err("Face recognition models are missing. Please download them first.".to_string());
    }

    // Get photos to process — careful scoping to satisfy borrow checker
    let photos: Vec<(i64, String)> = {
        let result: Result<Vec<(i64, String)>, String> = (|| {
            let conn = state.db.lock().map_err(|_| "db lock".to_string())?;
            let rows: Vec<(i64, String)> = if folder.is_empty() {
                let mut stmt = conn.prepare("SELECT id, path FROM photos")
                    .map_err(|e| e.to_string())?;
                let result: Vec<(i64, String)> = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                    .map_err(|e| e.to_string())?
                    .filter_map(|r| r.ok())
                    .collect();
                result
            } else {
                // Prefix match via substr() — `%`/`_` in the folder path
                // must not be treated as LIKE wildcards.
                let mut stmt = conn.prepare(
                    "SELECT id, path FROM photos
                     WHERE folder = ?1 OR substr(path, 1, length(?1)) = ?1"
                ).map_err(|e| e.to_string())?;
                let result: Vec<(i64, String)> = stmt.query_map(rusqlite::params![&folder], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                    .map_err(|e| e.to_string())?
                    .filter_map(|r| r.ok())
                    .collect();
                result
            };
            Ok(rows)
        })();
        result?
    };

    if photos.is_empty() {
        return Err("No photos found in this folder.".to_string());
    }

    let thumbs_dir = state.thumbnails_dir.clone();
    let db_arc = state.db.clone();
    let app_for_clip = app.clone();

    // Run detection + embedding in blocking thread
    let cluster_results = tokio::task::spawn_blocking(move || -> Result<Vec<FaceClusterResult>, String> {
        let face_models = crate::face::load_models(&models_dir).map_err(|e| e.to_string())?;
        let faces_dir = thumbs_dir.join("faces");
        std::fs::create_dir_all(&faces_dir).ok();

        // Level 2 CLIP filter — same pattern as the other two entry points.
        // See detect_faces_background for the detailed rationale.
        let mut clip_filter = try_load_clip_face_filter(&app_for_clip);

        // Detect faces in all photos (skip already-processed ones)
        let mut all_embeddings: Vec<(i64, i64, Vec<f32>)> = Vec::new(); // (face_id, photo_id, emb)

        for (photo_id, path) in &photos {
            // Check if already has faces
            let already_done = {
                let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                db::photo_has_faces(&conn, *photo_id)
            };

            if already_done {
                // Load existing embeddings
                let rows = {
                    let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                    db::get_faces_for_photo(&conn, *photo_id).unwrap_or_default()
                };
                for r in rows {
                    if let Some(bytes) = r.embedding_bytes {
                        let emb = crate::face::bytes_to_embedding(&bytes);
                        if !emb.is_empty() {
                            all_embeddings.push((r.id, *photo_id, emb));
                        }
                    }
                }
                continue;
            }

            // Detect new faces.
            // v1.5.38 — Use `crate::thumbnail::open_image` so EXIF orientation
            // is applied. Previously `image::open` left detection in raw-pixel
            // space, which mismatched the lightbox display (which goes through
            // `open_image` via `get_photo_full`) and made face boxes land off
            // the actual faces on any photo with orientation != 1.
            let img = match crate::thumbnail::open_image(path) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let detected = match crate::face::detect_faces(&face_models, &img) {
                Ok(d) => d,
                Err(_) => continue,
            };

            // Clear old detections and insert new ones
            {
                let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                db::delete_faces_for_photo(&conn, *photo_id).ok();
            }

            for face in &detected {
                // v1.4.21: prefer CLIP when available; heuristic is a fallback
                // (see detect_faces_background for the detailed rationale —
                // running BOTH in series was silently killing real faces).
                if let Some((engine, label_embs)) = clip_filter.as_mut() {
                    let clip_crop = crop_face_for_clip(&img, face);
                    if !clip_face_is_real(engine, label_embs, &clip_crop) { continue; }
                } else if !photo_face_is_real(&img, face) {
                    continue;
                }

                let embedding = crate::face::get_embedding(&face_models, &img, face)
                    .unwrap_or_default();
                let emb_bytes = crate::face::embedding_to_bytes(&embedding);

                let face_id = {
                    let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                    db::insert_face_region(
                        &conn, *photo_id,
                        face.x1, face.y1, face.x2, face.y2,
                        face.score, &emb_bytes,
                    ).unwrap_or(0)
                };
                if face_id == 0 { continue; }

                // Save face thumbnail
                let (iw, ih) = (img.width() as i32, img.height() as i32);
                let pad = ((face.x2 - face.x1).max(face.y2 - face.y1) / 5).max(8);
                let cx1 = (face.x1 - pad).max(0) as u32;
                let cy1 = (face.y1 - pad).max(0) as u32;
                let cx2 = (face.x2 + pad).min(iw) as u32;
                let cy2 = (face.y2 + pad).min(ih) as u32;
                let crop = img.crop_imm(cx1, cy1, cx2 - cx1, cy2 - cy1)
                    .resize(128, 128, image::imageops::FilterType::Triangle);
                crop.save_with_format(faces_dir.join(format!("face_{}.jpg", face_id)),
                    image::ImageFormat::Jpeg).ok();

                if !embedding.is_empty() {
                    all_embeddings.push((face_id, *photo_id, embedding));
                }
            }
        }

        if all_embeddings.is_empty() {
            return Ok(vec![]);
        }

        // Run clustering
        let face_pairs: Vec<(i64, Vec<f32>)> = all_embeddings
            .iter()
            .map(|(fid, _, emb)| (*fid, emb.clone()))
            .collect();
        let clusters = crate::face::cluster_embeddings(&face_pairs);

        // Build result
        let mut results = Vec::new();
        for cluster in &clusters {
            if cluster.face_ids.is_empty() { continue; }

            // Get up to 4 thumbnails
            let mut thumbs = Vec::new();
            for fid in cluster.face_ids.iter().take(4) {
                if let Some(b64) = face_thumb_b64(&faces_dir, *fid) {
                    thumbs.push(b64);
                }
            }

            // photo_ids for these faces
            let photo_ids: Vec<i64> = cluster.face_ids.iter()
                .filter_map(|fid| all_embeddings.iter().find(|(id,_,_)| id == fid).map(|(_,pid,_)| *pid))
                .collect();

            // Check if already assigned to a person
            let (person_name, person_id) = {
                let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                // Check representative face
                let mut pname = None;
                let mut pid = None;
                if let Ok(rows) = db::get_faces_for_photo(&conn, *photo_ids.first().unwrap_or(&0)) {
                    for r in rows {
                        if cluster.face_ids.contains(&r.id) {
                            pname = r.person_name;
                            pid = r.person_id;
                            break;
                        }
                    }
                }
                (pname, pid)
            };

            results.push(FaceClusterResult {
                thumbnails: thumbs,
                face_ids: cluster.face_ids.clone(),
                photo_ids,
                count: cluster.face_ids.len(),
                person_name,
                person_id,
            });
        }

        Ok(results)
    })
    .await
    .map_err(|e| e.to_string())?;

    cluster_results
}

/// Count photos that DON'T yet have face-region rows (and aren't art-tagged).
///
/// v1.5.28 — companion to the Identify Faces flow. The frontend's batch loop
/// processes 500 photos at a time via `detect_faces_background` but previously
/// had no idea how many batches remained. On a 45 k-photo library the user
/// saw "1520 scanned · 0 kept" and concluded "it's broken" without realizing
/// they'd only seen ~3 % of the queue. This command gives the UI the
/// denominator: "1520 / 45230 unscanned" is a totally different read from
/// a bare "1520 scanned".
///
/// Same WHERE clause as `detect_faces_background` so the count matches the
/// set the scanner will actually walk — no stale numbers, no drift.
#[tauri::command]
pub async fn count_unscanned_faces(
    folder: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let art_sql = art_tag_sql_list();
    let folder_filter: Option<String> = folder
        .as_ref()
        .and_then(|s| if s.trim().is_empty() { None } else { Some(s.clone()) });

    let conn = state.db.lock().map_err(|_| "db lock")?;
    let n: i64 = if let Some(f) = &folder_filter {
        // v1.5.45 — STRICT folder match (no substring/prefix). The previous
        // `OR substr(path, 1, length(?1)) = ?1` clause was scooping up
        // subfolders AND any folder that happened to share a prefix
        // (eg. "Pictures" matched "PicturesArchive"). The user reported
        // the auto-scan was scanning "tüm klasörü" — the entire library
        // — because their photo lived in a top-level folder whose prefix
        // matched 60k+ paths. Folder column is each photo's IMMEDIATE
        // parent, so exact match limits the scan to just the photos in
        // that exact directory.
        let sql = format!(
            "SELECT COUNT(*) FROM photos
             WHERE id NOT IN (SELECT DISTINCT photo_id FROM face_regions)
               AND id NOT IN (SELECT DISTINCT photo_id FROM tags WHERE LOWER(tag) IN {})
               AND folder = ?1",
            art_sql
        );
        conn.query_row(&sql, rusqlite::params![f], |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())?
    } else {
        let sql = format!(
            "SELECT COUNT(*) FROM photos
             WHERE id NOT IN (SELECT DISTINCT photo_id FROM face_regions)
               AND id NOT IN (SELECT DISTINCT photo_id FROM tags WHERE LOWER(tag) IN {})",
            art_sql
        );
        conn.query_row(&sql, [], |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())?
    };
    Ok(n.max(0) as usize)
}

/// Silently detect faces in all photos that don't already have face data.
/// Called automatically after tagging completes (if face models are present).
/// Returns the number of NEW faces detected.
#[tauri::command]
pub async fn detect_faces_background(
    folder: Option<String>,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(usize, usize), String> { // (photos_processed, faces_found)
    let models_dir = models_dir_for(&app);

    // Silently skip if face models are missing — not an error.
    // Accept EITHER w600k_r50.onnx (preferred, higher accuracy) OR w600k_mbf.onnx
    // (fallback). The previous gate hard-required mbf which meant a user with
    // only r50 installed saw the scan start and immediately return 0.
    let has_detector = models_dir.join("det_500m.onnx").exists();
    let has_embedder = models_dir.join("w600k_r50.onnx").exists()
        || models_dir.join("w600k_mbf.onnx").exists();
    if !has_detector || !has_embedder {
        return Ok((0, 0));
    }

    let thumbs_dir = state.thumbnails_dir.clone();
    let db_arc = state.db.clone();

    let folder_filter: Option<String> = folder
        .as_ref()
        .and_then(|s| if s.trim().is_empty() { None } else { Some(s.clone()) });

    // LEVEL 1 pre-filter — exclude photos already CLIP-tagged as art.
    // A photo flagged with `painting`/`drawing`/`heykel`/... is almost
    // never going to contain a real photographic face, and skipping it
    // here saves both an open_image + detector pass AND a wasted
    // "this keeps surfacing in my Unknown queue" round-trip for the user.
    let art_sql = art_tag_sql_list();

    // Collect photos that don't have face detections yet (batch of 500 max).
    // We build the query with the art-tag list interpolated (safe — it's a
    // compile-time constant, not user input).
    // v1.5.28 — ORDER BY id DESC → id ASC. User-reported regression on huge
    // libraries: "All Photos → Identify Faces → 1520 scanned / 0 kept" even
    // though per-folder scans work fine. Root cause: `id DESC` runs through
    // the most-recently-imported 500 photos first, which on a typical library
    // are screenshots, receipts, and product shots — all face-less. Users
    // watched "kept = 0" climb for minutes and gave up before the batch loop
    // got deep enough to reach the old family/event photos where the actual
    // faces live. Flipping to `id ASC` (oldest first) means the first batch
    // hits the content-rich end of the library, users see faces show up
    // quickly, and nobody bails out early. No data semantics change — the
    // loop still eventually scans everything — just the order.
    let photos: Vec<(i64, String)> = {
        let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = &folder_filter {
            // v1.5.45 — STRICT folder match (see count_unscanned_faces note).
            let sql = format!(
                "SELECT id, path FROM photos
                 WHERE id NOT IN (SELECT DISTINCT photo_id FROM face_regions)
                   AND id NOT IN (SELECT DISTINCT photo_id FROM tags WHERE LOWER(tag) IN {})
                   AND folder = ?1
                 ORDER BY CASE WHEN LOWER(filename) LIKE '%.jpg' OR LOWER(filename) LIKE '%.jpeg' OR LOWER(filename) LIKE '%.png' THEN 0 ELSE 1 END, id ASC
                 LIMIT 500",
                art_sql
            );
            let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
            let rows: Vec<(i64, String)> = stmt.query_map(rusqlite::params![f], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            rows
        } else {
            let sql = format!(
                "SELECT id, path FROM photos
                 WHERE id NOT IN (SELECT DISTINCT photo_id FROM face_regions)
                   AND id NOT IN (SELECT DISTINCT photo_id FROM tags WHERE LOWER(tag) IN {})
                 ORDER BY CASE WHEN LOWER(filename) LIKE '%.jpg' OR LOWER(filename) LIKE '%.jpeg' OR LOWER(filename) LIKE '%.png' THEN 0 ELSE 1 END, id ASC
                 LIMIT 500",
                art_sql
            );
            let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
            let rows: Vec<(i64, String)> = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            rows
        }
    };

    if photos.is_empty() {
        return Ok((0, 0));
    }

    let total_photos = photos.len();
    let ah = app.clone();

    // Mark scan as running + reset stop flag. Clone handles for the worker.
    state.face_stop.store(false, std::sync::atomic::Ordering::SeqCst);
    state.face_running.store(true, std::sync::atomic::Ordering::SeqCst);
    let stop_flag_worker = state.face_stop.clone();
    let stop_flag_cleanup = state.face_stop.clone();
    let running_flag_cleanup = state.face_running.clone();
    let stop_flag = stop_flag_worker;

    let result = tokio::task::spawn_blocking(move || -> Result<(usize, usize), String> {
        let face_models = crate::face::load_models(&models_dir).map_err(|e| e.to_string())?;
        let faces_dir = thumbs_dir.join("faces");
        std::fs::create_dir_all(&faces_dir).ok();

        // LEVEL 2 filter — try to load CLIP + pre-compute label embeddings once
        // for the whole batch. Returns None if no tier is installed or load
        // fails; in that case we gracefully fall back to Level 1 + heuristic.
        let mut clip_filter = try_load_clip_face_filter(&ah);
        let clip_available = clip_filter.is_some();
        let mut clip_rejected = 0usize;
        let mut heuristic_rejected = 0usize;
        let mut raw_detected = 0usize; // before any filtering

        let mut total_detected = 0usize;
        let mut auto_skipped_on_insert = 0usize;

        let mut opened = 0usize;
        let mut open_failed = 0usize;

        // ── v1.4.34: Load SKIPPED embeddings + per-cluster centroids ONCE ──
        //
        // The bug we're fixing (reported again and again since v1.4.25):
        //   user skips Person X in batch 1 → new photos of X detected in
        //   batch 2 → X reappears in the popup.
        //
        // Prior fix (v1.4.28) tried to catch this inside `get_unknown_faces`
        // via per-embedding direct matching at SKIP_THRESH=0.32. That worked
        // most of the time but left a hole: same-person cosine similarity
        // across extreme pose/lighting variation can drop to 0.25–0.31 and
        // quietly slip past 0.32. Those faces got clustered, shown to the
        // user, and "why won't you just stop asking about her" ensued.
        //
        // The real fix: check at INSERT time, not at query time. Every face
        // we're about to add to the Unknown queue gets compared against
        // (a) the flat list of -1 embeddings — MAX cos sim threshold 0.30
        //     (slightly lower than get_unknown_faces' 0.32 because catching
        //      a false skip here is mild — the face just silently dies;
        //      user can undo via Persons sidebar if needed), AND
        // (b) per-cluster centroids of the -1 embeddings — MAX cos sim
        //     threshold 0.35. Centroids smooth out per-embedding noise, so
        //     they catch "fuzzy" same-person matches that the flat 0.30
        //     missed.
        // If EITHER condition triggers, the face is inserted with
        // person_id = -1 so it never surfaces again. No threshold knob
        // tuning, no popup persistence — just don't show it.
        //
        // Cost: O(S) per face where S = current -1 count. 1000 skipped embs
        // × 512 floats × 1 pass per face ≈ 0.5 MFLOPs — measured <0.5 ms
        // per new face on a laptop CPU, noise vs. the actual detection work.
        let skipped_embs_flat: Vec<Vec<f32>> = {
            let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
            let mut stmt = match conn.prepare(
                "SELECT embedding FROM face_regions
                 WHERE person_id = -1 AND embedding IS NOT NULL"
            ) {
                Ok(s) => s,
                Err(_) => return Err("db: prepare skipped embs".to_string()),
            };
            let raw: Vec<Vec<u8>> = stmt
                .query_map([], |r| r.get::<_, Vec<u8>>(0))
                .map(|it| it.filter_map(|r| r.ok()).collect())
                .unwrap_or_default();
            raw.into_iter()
                .map(|b| crate::face::bytes_to_embedding(&b))
                .filter(|e| e.len() == 512)
                .collect()
        };
        let skipped_centroids: Vec<Vec<f32>> = if skipped_embs_flat.len() >= 2 {
            // Cluster the -1 embs to find skipped "identities", then take
            // the centroid of every cluster-of-size-≥-2. Singleton centroids
            // are dropped because they'd just duplicate the flat-emb check —
            // the whole point of the centroid path is averaging out noise
            // across multiple same-person views.
            let indexed: Vec<(i64, Vec<f32>)> = skipped_embs_flat
                .iter()
                .enumerate()
                .map(|(i, e)| (i as i64, e.clone()))
                .collect();
            let clusters = crate::face::cluster_embeddings(&indexed);
            clusters.into_iter()
                .filter(|c| c.face_ids.len() >= 2 && c.centroid.len() == 512)
                .map(|c| c.centroid)
                .collect()
        } else {
            Vec::new()
        };
        eprintln!(
            "[face] insert-time skip cache: {} flat embs, {} cluster centroids",
            skipped_embs_flat.len(), skipped_centroids.len()
        );

        eprintln!(
            "[face] detect_faces_background start: {} photos, CLIP filter: {}",
            total_photos, if clip_available { "ACTIVE" } else { "unavailable (heuristic fallback)" }
        );

        for (idx, (photo_id, path)) in photos.iter().enumerate() {
            // Cooperative cancellation — break cleanly if user pressed stop.
            if stop_flag.load(std::sync::atomic::Ordering::SeqCst) {
                eprintln!(
                    "[face] detect_faces_background: stop flag set, aborting at {}/{} (raw={}, heuristic_rejected={}, clip_rejected={}, kept={})",
                    idx, total_photos, raw_detected, heuristic_rejected, clip_rejected, total_detected
                );
                // v1.5.13 event contract: EVERY counter field in this payload is
                // scoped to a SINGLE detect_faces_background invocation (one batch
                // of up to 500 photos). The UI is responsible for accumulating
                // across batches. The batch_* prefix makes this scope explicit so
                // a future listener can't accidentally mix per-batch counters with
                // its own cumulative totals (the v1.5.12 "0 detected" bug).
                ah.emit("face-scan-progress", serde_json::json!({
                    "done": idx, "total": total_photos, "faces": total_detected,
                    "opened": opened, "failed": open_failed, "stopped": true,
                    "batch_raw_detected": raw_detected,
                    "batch_heuristic_rejected": heuristic_rejected,
                    "batch_clip_rejected": clip_rejected,
                    "batch_auto_skipped_on_insert": auto_skipped_on_insert,
                    // Legacy aliases — kept for one release cycle so older UI
                    // bundles (eg. users who skipped a version) still display
                    // something rather than undefined. Remove in v1.6.
                    "raw_detected": raw_detected,
                    "heuristic_rejected": heuristic_rejected,
                    "clip_rejected": clip_rejected,
                    "auto_skipped_on_insert": auto_skipped_on_insert,
                    "clip_active": clip_available,
                })).ok();
                return Ok((idx, total_detected));
            }
            // Progress every 20 photos
            if idx % 20 == 0 {
                // v1.5.13 event contract: see the stop-flag emit above — all
                // counter fields here are per detect_faces_background call.
                ah.emit("face-scan-progress", serde_json::json!({
                    "done": idx, "total": total_photos, "faces": total_detected,
                    "opened": opened, "failed": open_failed,
                    "batch_raw_detected": raw_detected,
                    "batch_heuristic_rejected": heuristic_rejected,
                    "batch_clip_rejected": clip_rejected,
                    "batch_auto_skipped_on_insert": auto_skipped_on_insert,
                    // Legacy aliases (remove in v1.6).
                    "raw_detected": raw_detected,
                    "heuristic_rejected": heuristic_rejected,
                    "clip_rejected": clip_rejected,
                    "auto_skipped_on_insert": auto_skipped_on_insert,
                    "clip_active": clip_available,
                })).ok();
            }

            // Use open_image which supports RAW/HEIC/video
            let img = match crate::thumbnail::open_image(path) {
                Ok(i) => { opened += 1; i },
                Err(_) => { open_failed += 1; continue; },
            };
            let detected = match crate::face::detect_faces(&face_models, &img) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[face] detect error on {}: {}", path, e);
                    continue;
                },
            };
            raw_detected += detected.len();

            for face in &detected {
                // Crop for thumbnail save (also handy for some filter paths).
                let (iw, ih) = (img.width() as i32, img.height() as i32);
                let pad = ((face.x2 - face.x1).max(face.y2 - face.y1) / 5).max(8);
                let cx1 = (face.x1 - pad).max(0) as u32;
                let cy1 = (face.y1 - pad).max(0) as u32;
                let cx2 = (face.x2 + pad).min(iw) as u32;
                let cy2 = (face.y2 + pad).min(ih) as u32;
                let crop = img.crop_imm(cx1, cy1, cx2 - cx1, cy2 - cy1)
                    .resize(128, 128, image::imageops::FilterType::Triangle);

                // ── Filtering policy (v1.4.21) ───────────────────────────────
                // Prefer CLIP when available (much more accurate, no false
                // rejects for dark-skin / low-light / B&W). When CLIP isn't
                // installed, fall back to the skin-colour heuristic.
                // Running BOTH in series was silently killing real faces
                // because the heuristic has multiple known-flaky checks.
                if let Some((engine, label_embs)) = clip_filter.as_mut() {
                    let clip_crop = crop_face_for_clip(&img, face);
                    if !clip_face_is_real(engine, label_embs, &clip_crop) {
                        clip_rejected += 1;
                        continue;
                    }
                } else {
                    // v1.5.19 — CLIP-off fallback DISABLED.
                    //
                    // Until v1.5.18 we ran `photo_face_is_real` here as a
                    // cartoon/statue guard. The heuristic (skin-colour hue +
                    // luminance distribution checks) turned out to be wildly
                    // over-aggressive in practice: user reported "1520 scanned
                    // · 0 kept · 72 detected · 72 heur-rej" in v1.5.18, i.e.
                    // 100% false-reject rate on a normal photo library. The
                    // thresholds (avg_val < 0.35, bright/dark bimodal, low
                    // skin-lum std) looked sane individually but compound into
                    // "nothing real gets through" on indoor / mixed-lighting
                    // shots.
                    //
                    // Trust SCRFD's own score_threshold=0.45 + NMS instead.
                    // That's a learned-model filter trained on millions of
                    // real face crops; it already rejects obvious non-faces.
                    // Edge cases (statues, cartoon portraits) will slip into
                    // the Unknown queue, where the user can skip-once and the
                    // v1.4.34 insert-time dedup keeps sibling detections from
                    // re-appearing.
                    //
                    // If a future user's library IS cartoon-heavy and the
                    // Unknown queue floods, the right fix is a Settings toggle
                    // (off by default) that re-enables the heuristic — not
                    // defaulting everyone back into 0-kept scans. The
                    // `photo_face_is_real` function is intentionally kept
                    // around for that future opt-in path.
                }

                let embedding = crate::face::get_embedding(&face_models, &img, face)
                    .unwrap_or_default();
                let emb_bytes = crate::face::embedding_to_bytes(&embedding);

                let face_id = {
                    let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                    db::insert_face_region(
                        &conn, *photo_id,
                        face.x1, face.y1, face.x2, face.y2,
                        face.score, &emb_bytes,
                    ).unwrap_or(0)
                };
                if face_id == 0 { continue; }

                // ── v1.4.34: Insert-time skip-match check ───────────────────
                // If this new face matches anything the user has ALREADY
                // skipped, mark it -1 right away so it never surfaces in the
                // Unknown queue. See the big comment near the skipped_embs_flat
                // initialization above for the full rationale.
                //
                // Two independent signals, either one triggers the skip:
                //   (1) Flat: MAX cos sim vs. every -1 embedding ≥ 0.30
                //   (2) Centroid: MAX cos sim vs. every -1 cluster centroid
                //       (clusters of size ≥ 2) ≥ 0.35
                //
                // 0.30 for the flat check is lower than get_unknown_faces'
                // 0.32 on purpose — by the time we're here the user has
                // already told us "don't ask about this person again", so
                // leaning slightly more aggressive is the right error.
                if embedding.len() == 512
                    && (!skipped_embs_flat.is_empty() || !skipped_centroids.is_empty())
                {
                    // v1.5.29 — lowered flat 0.30→0.25 and centroid 0.35→0.30.
                    // User report: "Döngüye giriyor. 'skip' dediklerimi her
                    // batch'de tekrar soruyor." Same-person cosine sim across
                    // pose / lighting variation sat in the 0.25–0.30 dead zone,
                    // below the old flat gate. Different-identity baseline is
                    // 0.10–0.22, so 0.25 still has margin; the centroid path
                    // (needs ≥2 skipped embs) stays safer at 0.30. False
                    // positives are recoverable (user can un-silence via the
                    // Persons sidebar); another popup round-trip is not.
                    const INSERT_SKIP_FLAT:     f32 = 0.25;
                    const INSERT_SKIP_CENTROID: f32 = 0.30;

                    let mut best_flat = 0.0f32;
                    for se in &skipped_embs_flat {
                        let sim = crate::face::cosine_similarity(&embedding, se);
                        if sim > best_flat { best_flat = sim; }
                    }
                    let mut best_centroid = 0.0f32;
                    for sc in &skipped_centroids {
                        let sim = crate::face::cosine_similarity(&embedding, sc);
                        if sim > best_centroid { best_centroid = sim; }
                    }
                    if best_flat >= INSERT_SKIP_FLAT || best_centroid >= INSERT_SKIP_CENTROID {
                        let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                        let updated = conn.execute(
                            "UPDATE face_regions SET person_id = -1 \
                             WHERE id = ?1 AND person_id IS NULL",
                            rusqlite::params![face_id],
                        ).unwrap_or(0);
                        if updated > 0 {
                            auto_skipped_on_insert += 1;
                            eprintln!(
                                "[face] insert-skip: face {} (photo {}) auto-skipped — flat={:.3} centroid={:.3}",
                                face_id, photo_id, best_flat, best_centroid
                            );
                        }
                    }
                }

                // Save face thumbnail
                crop.save_with_format(
                    faces_dir.join(format!("face_{}.jpg", face_id)),
                    image::ImageFormat::Jpeg,
                ).ok();

                total_detected += 1;
            }
        }

        // Final progress flush so the UI shows the last numbers.
        // v1.5.13 event contract: see tick emits above — all counter fields are
        // per detect_faces_background call. batch_* prefix is authoritative;
        // unprefixed legacy aliases are kept for one release cycle.
        ah.emit("face-scan-progress", serde_json::json!({
            "done": total_photos, "total": total_photos, "faces": total_detected,
            "opened": opened, "failed": open_failed,
            "batch_raw_detected": raw_detected,
            "batch_heuristic_rejected": heuristic_rejected,
            "batch_clip_rejected": clip_rejected,
            "batch_auto_skipped_on_insert": auto_skipped_on_insert,
            // Legacy aliases (remove in v1.6).
            "raw_detected": raw_detected,
            "heuristic_rejected": heuristic_rejected,
            "clip_rejected": clip_rejected,
            "auto_skipped_on_insert": auto_skipped_on_insert,
            "clip_active": clip_available,
            "finished": true,
        })).ok();

        eprintln!(
            "[face] detect_faces_background done: photos={}, raw_detected={}, heuristic_rejected={}, clip_rejected={}, kept={}, auto_skipped_on_insert={} (CLIP {})",
            total_photos, raw_detected, heuristic_rejected, clip_rejected, total_detected, auto_skipped_on_insert,
            if clip_available { "ACTIVE" } else { "unavailable" }
        );
        Ok((total_photos, total_detected))
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    // Always clear the running flag, regardless of success/stop/error.
    running_flag_cleanup.store(false, std::sync::atomic::Ordering::SeqCst);
    stop_flag_cleanup.store(false, std::sync::atomic::Ordering::SeqCst);
    result
}

/// v1.5.43 — Force-redetect faces in every photo of a folder, throwing away
/// any UNNAMED face_regions (person_id IS NULL or person_id = -1) and
/// rebuilding them from scratch with the current detector + embedder. The
/// reason this exists: pre-v1.5.38 face detection ran on raw pixel bytes
/// (no EXIF orientation), so face boxes AND embeddings on iPhone-style
/// photos with orientation != 1 are stored in the wrong coordinate space.
/// Propagation against those stale embeddings always misses, even for
/// obvious matches. Calling this command refreshes everything in one go so
/// "name a face → tag everyone in folder" actually works the second time
/// the user tries.
///
/// Named faces (person_id > 0) are PRESERVED — the user has explicitly
/// claimed them, and the displayed bounding box might be a few pixels off
/// but the assignment to a person is what matters.
///
/// Returns (photos_processed, faces_found_after_redetect).
#[tauri::command]
pub async fn redetect_unnamed_faces_in_folder(
    folder: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(usize, usize), String> {
    if folder.trim().is_empty() {
        return Err("folder must not be empty".into());
    }
    let models_dir = models_dir_for(&app);
    let has_detector = models_dir.join("det_500m.onnx").exists();
    let has_embedder = models_dir.join("w600k_r50.onnx").exists()
        || models_dir.join("w600k_mbf.onnx").exists();
    if !has_detector || !has_embedder {
        return Err("face models not installed".into());
    }
    let db_arc = state.db.clone();
    let app_for_clip = app.clone();

    // Collect every photo in the folder (image type).
    // v1.5.45 — STRICT folder match.
    let photos: Vec<(i64, String)> = {
        let conn = db_arc.lock().map_err(|_| "db lock")?;
        let mut stmt = conn.prepare(
            "SELECT id, path FROM photos
             WHERE media_type = 'image'
               AND folder = ?1
             ORDER BY id ASC",
        ).map_err(|e| e.to_string())?;
        let rows: Vec<(i64, String)> = stmt
            .query_map(rusqlite::params![&folder], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    if photos.is_empty() {
        return Ok((0, 0));
    }

    let total_photos = photos.len();
    let result = tokio::task::spawn_blocking(move || -> Result<(usize, usize), String> {
        let face_models = crate::face::load_models(&models_dir)
            .map_err(|e| e.to_string())?;
        let mut clip_filter = try_load_clip_face_filter(&app_for_clip);
        let mut total_kept = 0usize;
        for (photo_id, path) in &photos {
            // Wipe everything that isn't a real named assignment. Skipped
            // (-1) faces are dropped because their wrong-space embeddings
            // are precisely what we're trying to fix.
            {
                let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                conn.execute(
                    "DELETE FROM face_regions
                     WHERE photo_id = ?1
                       AND (person_id IS NULL OR person_id = -1)",
                    rusqlite::params![photo_id],
                ).ok();
            }
            // Use open_image so EXIF orientation is applied (the whole
            // point of this command).
            let img = match crate::thumbnail::open_image(path) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let detected = match crate::face::detect_faces(&face_models, &img) {
                Ok(d) => d,
                Err(_) => continue,
            };
            // Preserve any still-named faces' bboxes so we don't double up.
            let named_bboxes: Vec<(i32, i32, i32, i32)> = {
                let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                conn.prepare(
                    "SELECT x1, y1, x2, y2 FROM face_regions
                     WHERE photo_id = ?1 AND person_id > 0",
                )
                .and_then(|mut s| {
                    let v: Vec<(i32,i32,i32,i32)> = s.query_map(
                        rusqlite::params![photo_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    )?.filter_map(|r| r.ok()).collect();
                    Ok(v)
                })
                .unwrap_or_default()
            };

            for face in &detected {
                if let Some((engine, label_embs)) = clip_filter.as_mut() {
                    let clip_crop = crop_face_for_clip(&img, face);
                    if !clip_face_is_real(engine, label_embs, &clip_crop) { continue; }
                } else if !photo_face_is_real(&img, face) {
                    continue;
                }
                let overlaps_named = named_bboxes.iter().any(|&(nx1, ny1, nx2, ny2)| {
                    let ix1 = face.x1.max(nx1); let iy1 = face.y1.max(ny1);
                    let ix2 = face.x2.min(nx2); let iy2 = face.y2.min(ny2);
                    let inter = ((ix2-ix1).max(0) * (iy2-iy1).max(0)) as f32;
                    let area_a = ((face.x2-face.x1)*(face.y2-face.y1)) as f32;
                    let area_b = ((nx2-nx1)*(ny2-ny1)) as f32;
                    let union = area_a + area_b - inter;
                    union > 0.0 && inter/union > 0.4
                });
                if overlaps_named { continue; }
                let embedding = crate::face::get_embedding(&face_models, &img, face)
                    .unwrap_or_default();
                if embedding.is_empty() { continue; }
                let emb_bytes = crate::face::embedding_to_bytes(&embedding);
                let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                let _ = db::insert_face_region(
                    &conn, *photo_id,
                    face.x1, face.y1, face.x2, face.y2,
                    face.score, &emb_bytes,
                );
                total_kept += 1;
            }
        }
        Ok((total_photos, total_kept))
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    result
}

/// Stop an in-progress face scan. Sets the cooperative stop flag — the current
/// photo will finish detection, then the loop exits cleanly and returns how
/// many photos were processed before the stop.
#[tauri::command]
pub async fn stop_face_scan(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.face_stop.store(true, std::sync::atomic::Ordering::SeqCst);
    eprintln!("[face] stop_face_scan: flag set");
    Ok(())
}

/// Assign all faces in a cluster to a person (create if name given, use existing if id given).
#[tauri::command]
pub async fn assign_cluster_to_person(
    face_ids: Vec<i64>,
    photo_ids: Vec<i64>,
    person_id: Option<i64>,
    person_name: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;

    // Resolve or create the person
    let pid = if let Some(id) = person_id {
        id
    } else if let Some(ref name) = person_name {
        // Try find existing person with this name first
        let existing: Option<i64> = conn.query_row(
            "SELECT id FROM persons WHERE name = ?1",
            rusqlite::params![name.trim()],
            |r| r.get(0),
        ).ok();
        if let Some(id) = existing {
            id
        } else {
            db::create_person(&conn, name.trim()).map_err(|e| e.to_string())?
        }
    } else {
        return Err("person_id or person_name is required".to_string());
    };

    // Assign every face in the cluster
    for face_id in &face_ids {
        db::assign_face_to_person(&conn, *face_id, Some(pid)).ok();
    }

    // Set representative thumbnail (first face)
    if let Some(first_id) = face_ids.first() {
        let thumb_name = format!("face_{}.jpg", first_id);
        db::update_person_thumbnail(&conn, pid, &thumb_name).ok();
    }

    // Add person name as a tag to all involved photos.
    // IMPORTANT: keep canonical casing — insert_tags handles case-insensitive
    // dedup via Unicode-aware to_lowercase, so "Buğra" and "buğra" don't
    // create separate tag rows.
    let tag = if let Some(ref name) = person_name {
        name.trim().to_string()
    } else {
        // Look up name from DB
        conn.query_row(
            "SELECT name FROM persons WHERE id = ?1",
            rusqlite::params![pid],
            |r| r.get::<_, String>(0),
        ).unwrap_or_default()
    };

    let unique_photo_ids: std::collections::HashSet<i64> = photo_ids.into_iter().collect();
    if !tag.is_empty() {
        for photo_id in unique_photo_ids {
            db::insert_tags(&conn, photo_id, &[(tag.clone(), 1.0, "face".to_string())]).ok();
        }
    }

    // Aggressive propagation: tag any unassigned face that's similar to any
    // of the seed cluster faces. Mirrors name_face_and_propagate so the user
    // is not re-asked about the same person in the next batch.
    {
        let seed_embs: Vec<Vec<f32>> = {
            let mut out = Vec::new();
            if let Ok(mut stmt) = conn.prepare(
                "SELECT embedding FROM face_regions WHERE id = ?1 AND embedding IS NOT NULL"
            ) {
                for fid in &face_ids {
                    if let Ok(bytes) = stmt.query_row(
                        rusqlite::params![fid], |r| r.get::<_, Vec<u8>>(0)
                    ) {
                        let emb = crate::face::bytes_to_embedding(&bytes);
                        if emb.len() == 512 { out.push(emb); }
                    }
                }
            }
            out
        };
        if !seed_embs.is_empty() {
            const NAME_PROPAGATE_THRESH: f32 = 0.40;
            if let Ok(unassigned) = db::get_unassigned_faces_with_embeddings(&conn) {
                let mut propagated = 0usize;
                for (fid, photo_id, emb_bytes) in &unassigned {
                    let emb = crate::face::bytes_to_embedding(emb_bytes);
                    if emb.len() != 512 { continue; }
                    let best = seed_embs.iter()
                        .map(|s| crate::face::cosine_similarity(s, &emb))
                        .fold(0.0f32, f32::max);
                    if best >= NAME_PROPAGATE_THRESH {
                        db::assign_face_to_person(&conn, *fid, Some(pid)).ok();
                        if !tag.is_empty() {
                            db::insert_tags(
                                &conn, *photo_id,
                                &[(tag.clone(), best as f64, "face".to_string())],
                            ).ok();
                        }
                        propagated += 1;
                    }
                }
                if propagated > 0 {
                    eprintln!("[face] assign_cluster_to_person '{}': {} extra propagated",
                              tag, propagated);
                }
            }
        }
    }

    Ok(pid)
}

// ── 15. CLIP Semantic Search ──────────────────────────────────────────────────

fn clip_models_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    app.path()
        .app_local_data_dir()
        .map(|d: std::path::PathBuf| d.join("models"))
        .unwrap_or_else(|_| std::path::PathBuf::from("models"))
}

/// Check which CLIP tiers are downloaded.
#[tauri::command]
pub async fn get_clip_status(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let base = clip_models_dir(&app);
    let tiers = [crate::clip::ClipTier::Fast, crate::clip::ClipTier::Balanced, crate::clip::ClipTier::Best];
    let statuses: Vec<serde_json::Value> = tiers.iter().map(|t| {
        let dir = base.join(t.dir_name());
        let downloaded = dir.join("visual.onnx").exists() && dir.join("textual.onnx").exists();
        serde_json::json!({
            "tier": t,
            "label": t.label(),
            "size_mb": t.size_mb(),
            "downloaded": downloaded,
        })
    }).collect();
    Ok(serde_json::json!(statuses))
}

/// Download models for the given tier.
#[tauri::command]
pub async fn download_clip_models(
    tier: crate::clip::ClipTier,
    app: tauri::AppHandle,
) -> Result<String, String> {
    let base = clip_models_dir(&app);
    let dir  = base.join(tier.dir_name());
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let (vis_url, txt_url, vocab_url, merges_url) = tier.urls();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;

    async fn dl(client: &reqwest::Client, url: &str, dest: &std::path::Path) -> Result<(), String> {
        // Skip only if file exists AND is larger than 1KB (not corrupted)
        if dest.exists() {
            if let Ok(meta) = std::fs::metadata(dest) {
                if meta.len() > 1024 { return Ok(()); }
            }
            // Remove corrupted/tiny file
            let _ = std::fs::remove_file(dest);
        }
        let resp = client.get(url)
            .header("User-Agent", "RetinaTag/1.2")
            .send().await.map_err(|e| format!("Download error {}: {}", url, e))?;
        if !resp.status().is_success() {
            return Err(format!("Download failed (HTTP {}): {}", resp.status(), url));
        }
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() < 1024 {
            return Err(format!("Downloaded file too small ({} bytes): {}", bytes.len(), url));
        }
        std::fs::write(dest, &bytes).map_err(|e| e.to_string())?;
        Ok(())
    }

    dl(&client, &vis_url,    &dir.join("visual.onnx"    )).await?;
    dl(&client, &txt_url,    &dir.join("textual.onnx"   )).await?;
    dl(&client, &vocab_url,  &dir.join("clip_vocab.json")).await?;
    dl(&client, &merges_url, &dir.join("clip_merges.txt")).await?;

    Ok(format!("{} model downloaded!", tier.label()))
}

/// Index all photos: compute CLIP image embeddings and store in DB.
#[tauri::command]
pub async fn index_clip_embeddings(
    tier: crate::clip::ClipTier,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<usize, String> {
    let base = clip_models_dir(&app);

    // Photos that haven't been indexed with this tier yet
    let photos: Vec<(i64, String)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_photos_without_clip_emb(&conn, tier.dir_name()).map_err(|e| e.to_string())?
    };

    if photos.is_empty() {
        return Ok(0);
    }

    let db_arc      = state.db.clone();
    let tier_name   = tier.dir_name().to_string();
    let app_clone   = app.clone();

    let indexed = tokio::task::spawn_blocking(move || -> Result<usize, String> {
        let mut engine = crate::clip::load_engine(&base, tier).map_err(|e| e.to_string())?;
        let total  = photos.len();
        let mut done = 0usize;

        for (photo_id, path) in &photos {
            let img = match image::open(path) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let emb = match crate::clip::encode_image(&mut engine, &img) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let bytes = crate::clip::embedding_to_bytes(&emb);
            {
                let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                db::save_clip_embedding(&conn, *photo_id, &bytes, &tier_name).ok();
            }
            done += 1;
            if done % 20 == 0 {
                app_clone.emit("clip-index-progress", serde_json::json!({
                    "done": done, "total": total
                })).ok();
            }
        }
        Ok(done)
    })
    .await
    .map_err(|e| e.to_string())?;

    indexed
}

/// Semantic search: encode query text with CLIP, return top-N most similar photos.
#[tauri::command]
pub async fn semantic_search(
    query: String,
    tier: crate::clip::ClipTier,
    limit: usize,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<Vec<PhotoSummary>, String> {
    if query.trim().is_empty() {
        return Ok(vec![]);
    }

    let base = clip_models_dir(&app);

    // Load all embeddings from DB
    let photo_embs: Vec<(i64, Vec<u8>)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_photos_with_clip_emb(&conn, tier.dir_name()).map_err(|e| e.to_string())?
    };

    if photo_embs.is_empty() {
        return Err("Please index your photos first using the 'Semantic Index' button.".to_string());
    }

    let query_owned = query.trim().to_string();
    let db_arc = state.db.clone();
    let tier_key = tier.dir_name().to_string();

    let results = tokio::task::spawn_blocking(move || -> Result<Vec<PhotoSummary>, String> {
        // Fast path: reuse a cached text embedding for this exact query+tier.
        // Typical interactive use — user types "beach sunset" many times —
        // turns the 200-800 ms CLIP text encoder call into a 0.2 ms lookup.
        let cached_bytes = {
            let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
            db::get_cached_text_embedding(&conn, &tier_key, &query_owned).ok().flatten()
        };

        let query_emb: Vec<f32> = if let Some(bytes) = cached_bytes {
            crate::clip::bytes_to_embedding(&bytes)
        } else {
            let mut engine = crate::clip::load_engine(&base, tier).map_err(|e| e.to_string())?;
            let emb = crate::clip::encode_text(&mut engine, &query_owned).map_err(|e| e.to_string())?;
            // Persist for next time. Best-effort; cache miss is non-fatal.
            let bytes = crate::clip::embedding_to_bytes(&emb);
            let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
            let _ = db::put_cached_text_embedding(&conn, &tier_key, &query_owned, &bytes);
            emb
        };

        // Compute cosine similarity for all indexed photos
        let mut scored: Vec<(i64, f32)> = photo_embs
            .iter()
            .map(|(pid, bytes)| {
                let emb = crate::clip::bytes_to_embedding(bytes);
                let sim = crate::clip::cosine_similarity(&query_emb, &emb);
                (*pid, sim)
            })
            .collect();

        // Sort by similarity descending
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit.max(1).min(200));

        // Filter: only show results above a reasonable threshold
        let threshold = 0.20f32;
        let top_ids: Vec<i64> = scored
            .into_iter()
            .filter(|(_, s)| *s >= threshold)
            .map(|(id, _)| id)
            .collect();

        let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
        db::get_photos_by_ids(&conn, &top_ids).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?;

    results
}

/// How many photos have been CLIP-indexed for a given tier.
#[tauri::command]
pub async fn get_clip_index_count(
    tier: crate::clip::ClipTier,
    state: tauri::State<'_, AppState>,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    Ok(db::count_clip_indexed(&conn, tier.dir_name()))
}

/// Check GPU (DirectML) availability for ONNX sessions.
#[tauri::command]
pub fn get_gpu_status() -> Result<serde_json::Value, String> {
    let available = crate::clip::is_directml_available();
    Ok(serde_json::json!({
        "gpu_available": available,
        "backend": if available { "DirectML" } else { "CPU" }
    }))
}

/// Update tray tooltip to reflect current work ("Scanning… 42%"). Frontend
/// calls this as progress events fire so the minimized app still shows live
/// status without stealing focus.
#[tauri::command]
pub fn set_tray_progress(
    label: String,
    app: tauri::AppHandle,
) -> Result<(), String> {
    crate::tray::set_tray_tooltip(&app, &label);
    Ok(())
}

/// Hide / show the main window (used by the tray "Hide to tray" menu).
#[tauri::command]
pub fn hide_to_tray(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("main") {
        w.hide().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Ensure every named face has its person name inserted as a tag on the photo.
#[tauri::command]
pub async fn sync_person_tags(
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    // Find all face_regions that have a person_id assigned
    let mut stmt = conn.prepare(
        "SELECT fr.photo_id, pe.name
         FROM face_regions fr
         JOIN persons pe ON pe.id = fr.person_id
         WHERE fr.person_id IS NOT NULL"
    ).map_err(|e| e.to_string())?;

    let pairs: Vec<(i64, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();

    let mut synced = 0usize;
    for (photo_id, name) in &pairs {
        if db::insert_tags(&conn, *photo_id, &[(name.clone(), 1.0, "face".to_string())]).is_ok() {
            synced += 1;
        }
    }
    Ok(synced)
}

// ── Rating & Favorites ─────────────────────────────────────────────────────

#[tauri::command]
pub async fn set_rating(photo_id: i64, rating: i32, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::set_rating(&conn, photo_id, rating).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_favorite(photo_id: i64, favorite: bool, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::set_favorite(&conn, photo_id, favorite).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn batch_set_rating(photo_ids: Vec<i64>, rating: i32, state: tauri::State<'_, AppState>) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::batch_set_rating(&conn, &photo_ids, rating).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn batch_set_favorite(photo_ids: Vec<i64>, favorite: bool, state: tauri::State<'_, AppState>) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::batch_set_favorite(&conn, &photo_ids, favorite).map_err(|e| e.to_string())
}

/// Bulk-move photos into (or out of) the private vault. Mirrors
/// `toggle_photo_private` but for a selection — the batch-toolbar "🔒 Vault"
/// button uses this so the user can hide sensitive sets in one shot.
///
/// v1.5.73 — was a P0 leak: previously this just flipped the `private`
/// flag in the DB without going through `vault_files::encrypt_in_place`,
/// so a user multi-selecting "🔒 Vault" got photos marked private while
/// the originals stayed readable in Explorer. Now we route every id
/// through the same encrypt+mark logic as `toggle_photo_private`, and
/// hard-fail if the vault is locked (the user has to unlock first so
/// the KEK is available).
#[tauri::command]
pub async fn batch_set_private(
    photo_ids: Vec<i64>,
    private: bool,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    if photo_ids.is_empty() {
        return Ok(0);
    }
    let kek_opt: Option<[u8; 32]> = {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        *g
    };
    if kek_opt.is_none() {
        return Err("Vault is locked — unlock it first before moving photos into the vault.".into());
    }
    let db_arc = state.db.clone();
    let thumbs_dir = state.thumbnails_dir.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<usize, String> {
        let kek = kek_opt.unwrap();
        let mut n = 0usize;
        for id in &photo_ids {
            match move_photo_private(&db_arc, *id, private, &kek, &thumbs_dir) {
                Ok(_) => n += 1,
                Err(e) => eprintln!("batch_set_private: id {} failed: {}", id, e),
            }
        }
        Ok(n)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// v1.5.73 — Shared "move one photo in/out of vault" helper. Used by
/// `batch_set_private`, `toggle_photo_private`, and `auto_hide_nsfw`,
/// so every code path that flips the private flag also encrypts the
/// underlying file + thumbnail (or decrypts on un-vault).
///
/// Caller must have verified the vault is unlocked. Acquires the db
/// mutex internally for short windows so we don't hold the lock across
/// the (slow) AES-GCM seal/open.
fn move_photo_private(
    db_arc: &std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>,
    photo_id: i64,
    new_private: bool,
    kek: &[u8; 32],
    thumbs_dir: &std::path::Path,
) -> Result<(), String> {
    // Snapshot row state.
    let (was_private, hash, current_path, saved_orig_path) = {
        let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
        conn.query_row(
            "SELECT private, COALESCE(hash, ''), path, original_path
             FROM photos WHERE id = ?1",
            rusqlite::params![photo_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? == 1,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .map_err(|e| e.to_string())?
    };
    // No-op if already in the target state.
    if was_private == new_private {
        return Ok(());
    }
    let cache_name = if !hash.is_empty() {
        thumbnail::thumb_cache_name(&hash)
    } else {
        String::new()
    };
    let thumb_path = if !cache_name.is_empty() {
        thumbs_dir.join(&cache_name)
    } else {
        std::path::PathBuf::new()
    };
    if new_private {
        let orig_path_pb = std::path::PathBuf::from(&current_path);
        if !crate::vault_files::is_encrypted_path(&orig_path_pb) && orig_path_pb.is_file() {
            crate::vault_files::cleanup_partial(&orig_path_pb);
            // v1.5.154 — encrypt_in_place no longer deletes the source.
            // We commit DB first, then delete the original on success
            // OR delete the freshly-written .rtenc on failure so the
            // source remains the single source of truth.
            let enc_path = crate::vault_files::encrypt_in_place(&orig_path_pb, kek)?;
            let enc_path_str = enc_path.to_string_lossy().to_string();
            let db_ok = {
                let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
                match db::mark_photo_encrypted(&conn, photo_id, &enc_path_str, &current_path) {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!("toggle_private mark_photo_encrypted: {}", e);
                        false
                    }
                }
            };
            if db_ok {
                // Commit the on-disk side: remove the plaintext.
                if let Err(e) = crate::vault_files::remove_file_with_fallback(&orig_path_pb) {
                    // Leaked plaintext file. The .rtenc + DB row are
                    // already correct so the photo IS in the vault;
                    // we just couldn't delete the source. Surface so
                    // the caller (and the user) know to investigate.
                    return Err(format!("encrypted + DB OK but failed to remove original: {}", e));
                }
            } else {
                // Roll back: drop the .rtenc, keep the original.
                if let Err(e) = crate::vault_files::remove_file_with_fallback(&enc_path) {
                    eprintln!("toggle_private rollback failed to remove rtenc: {}", e);
                }
                return Err("DB write failed, rolled back — original kept on disk".to_string());
            }
        }
        if thumb_path.is_file() {
            if let Ok(bytes) = std::fs::read(&thumb_path) {
                let sealed = crate::vault_crypto::seal(kek, &bytes).map_err(|e| e.to_string())?;
                // v1.5.154 — Was: store_encrypted_thumb result was `?`'d
                // but `let _ = std::fs::remove_file(&thumb_path)` silently
                // swallowed remove errors. Surface those too — same class.
                {
                    let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
                    db::store_encrypted_thumb(&conn, photo_id, &sealed)
                        .map_err(|e| e.to_string())?;
                }
                if let Err(e) = crate::vault_files::remove_file_with_fallback(&thumb_path) {
                    eprintln!("toggle_private remove plaintext thumb: {}", e);
                }
            }
        }
        let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
        db::set_photo_private(&conn, photo_id, true).map_err(|e| e.to_string())?;
    } else {
        let cur = std::path::PathBuf::from(&current_path);
        if crate::vault_files::is_encrypted_path(&cur) && cur.is_file() {
            let dest = saved_orig_path
                .clone()
                .map(std::path::PathBuf::from)
                .or_else(|| crate::vault_files::original_path_for(&cur))
                .ok_or_else(|| "cannot determine restore path".to_string())?;
            if dest.exists() {
                if let Err(e) = crate::vault_files::remove_file_with_fallback(&dest) {
                    return Err(format!("destination already exists and won't unlink: {}", e));
                }
            }
            // v1.5.154 — decrypt_to_file no longer deletes the .rtenc.
            // Same atomicity dance: DB first, then remove .rtenc on
            // success, or remove the freshly-written plaintext on
            // failure so the .rtenc remains ground truth.
            crate::vault_files::decrypt_to_file(&cur, &dest, kek)?;
            let db_ok = {
                let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
                match db::mark_photo_decrypted(&conn, photo_id, &dest.to_string_lossy()) {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!("toggle_private mark_photo_decrypted: {}", e);
                        false
                    }
                }
            };
            if db_ok {
                if let Err(e) = crate::vault_files::remove_file_with_fallback(&cur) {
                    return Err(format!("decrypted + DB OK but failed to remove rtenc: {}", e));
                }
            } else {
                if let Err(e) = crate::vault_files::remove_file_with_fallback(&dest) {
                    eprintln!("toggle_private rollback failed to remove plaintext: {}", e);
                }
                return Err("DB write failed, rolled back — .rtenc kept on disk".to_string());
            }
        }
        let blob_opt: Option<Vec<u8>> = {
            let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
            db::get_encrypted_thumb(&conn, photo_id).map_err(|e| e.to_string())?
        };
        if let Some(blob) = blob_opt {
            if !thumb_path.as_os_str().is_empty() {
                match crate::vault_crypto::open(kek, &blob) {
                    Ok(plain) => {
                        // v1.5.154 — surface write/clear errors instead of `let _`.
                        if let Err(e) = std::fs::write(&thumb_path, &plain) {
                            eprintln!("toggle_private restore thumb write: {}", e);
                        } else {
                            let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
                            if let Err(e) = db::clear_encrypted_thumb(&conn, photo_id) {
                                eprintln!("toggle_private clear_encrypted_thumb: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("toggle_private decrypt thumb: {}", e);
                    }
                }
            }
        }
        let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
        db::set_photo_private(&conn, photo_id, false).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn batch_add_tags(photo_ids: Vec<i64>, tags: Vec<String>, state: tauri::State<'_, AppState>) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::batch_add_tags(&conn, &photo_ids, &tags).map_err(|e| e.to_string())
}

/// Remove one or more tags from a batch of photos. Used by the selection
/// toolbar's "Remove tag" action so users can strip a mis-applied AI tag
/// from many photos at once without opening each detail panel. Returns the
/// number of (photo_id, tag) rows actually deleted — a tag not present on a
/// photo is a no-op.
#[tauri::command]
pub async fn batch_remove_tags(
    photo_ids: Vec<i64>,
    tags: Vec<String>,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    if photo_ids.is_empty() || tags.is_empty() {
        return Ok(0);
    }
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let mut total = 0usize;
    // Case-insensitive match since AI and user tags sometimes collide
    // only by case (e.g. "Dog" vs "dog"). LOWER() on both sides is cheap
    // enough — `tags` typically holds ≤3 entries and `photo_ids` ≤ selection.
    let mut stmt = conn
        .prepare("DELETE FROM tags WHERE photo_id = ?1 AND LOWER(tag) = LOWER(?2)")
        .map_err(|e| e.to_string())?;
    for pid in &photo_ids {
        for t in &tags {
            let n = stmt
                .execute(rusqlite::params![pid, t])
                .map_err(|e| e.to_string())?;
            total += n;
        }
    }
    Ok(total)
}

/// Result of batch_add_tags_with_xmp: how many DB rows were inserted and how
/// many XMP sidecars were successfully written.
#[derive(serde::Serialize)]
pub struct BatchTagXmpResult {
    pub tags_added: usize,
    pub xmp_written: usize,
    pub xmp_failed: usize,
}

/// Add tags to many photos *and* write an XMP sidecar next to each photo so
/// the tags are persisted outside the database. This is what users mean when
/// they ask "is the tag actually written to the photo itself?" — XMP is the
/// industry-standard sidecar format (Lightroom, Bridge, DigiKam all read it).
///
/// Writes `<stem>.xmp` next to the original file. The original JPG/RAW is
/// never modified.
#[tauri::command]
pub async fn batch_add_tags_with_xmp(
    photo_ids: Vec<i64>,
    tags: Vec<String>,
    state: tauri::State<'_, AppState>,
) -> Result<BatchTagXmpResult, String> {
    // ── Step 1: write to DB ──────────────────────────────────────────────────
    let tags_added = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::batch_add_tags(&conn, &photo_ids, &tags).map_err(|e| e.to_string())?
    };

    // ── Step 2: gather XmpData for each photo (same shape as write_xmp_all) ──
    let all_xmp: Vec<xmp::XmpData> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let mut result = Vec::with_capacity(photo_ids.len());
        for id in &photo_ids {
            // Core photo fields. If a row is missing (shouldn't happen), skip it.
            let row = conn.query_row(
                "SELECT path, COALESCE(width,0), COALESCE(height,0),
                        COALESCE(rating,0), COALESCE(favorite,0),
                        description, estimated_location
                 FROM photos WHERE id = ?1",
                rusqlite::params![id],
                |r| {
                    let path: String = r.get(0)?;
                    let width: u32 = r.get(1)?;
                    let height: u32 = r.get(2)?;
                    let rating: i32 = r.get(3)?;
                    let favorite: bool = { let v: i32 = r.get(4)?; v != 0 };
                    let description: Option<String> = r.get(5)?;
                    let location: Option<String> = r.get(6)?;
                    Ok((path, width, height, rating, favorite, description, location))
                },
            );
            let (path, width, height, rating, favorite, description, location) = match row {
                Ok(v) => v,
                Err(_) => continue,
            };

            // All tags currently on the photo (including the ones we just added).
            let photo_tags: Vec<String> = {
                let mut s = match conn.prepare_cached(
                    "SELECT tag FROM tags WHERE photo_id = ?1",
                ) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                s.query_map(rusqlite::params![id], |r| r.get(0))
                    .map(|it| it.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
            };

            // Named faces on this photo (for MWG regions block).
            let faces: Vec<xmp::XmpFace> = if width > 0 && height > 0 {
                let mut s = match conn.prepare_cached(
                    "SELECT fr.x1, fr.y1, fr.x2, fr.y2, p.name
                     FROM face_regions fr
                     JOIN persons p ON fr.person_id = p.id
                     WHERE fr.photo_id = ?1 AND fr.person_id > 0",
                ) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let raw: Vec<(i32, i32, i32, i32, String)> = s
                    .query_map(rusqlite::params![id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                    })
                    .map(|it| it.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default();
                raw.into_iter()
                    .map(|(x1, y1, x2, y2, name)| {
                        let w_f = width as f32;
                        let h_f = height as f32;
                        xmp::XmpFace {
                            name,
                            cx: ((x1 + x2) as f32 / 2.0) / w_f,
                            cy: ((y1 + y2) as f32 / 2.0) / h_f,
                            w: (x2 - x1) as f32 / w_f,
                            h: (y2 - y1) as f32 / h_f,
                        }
                    })
                    .collect()
            } else {
                vec![]
            };

            result.push(xmp::XmpData {
                photo_path: path,
                tags: photo_tags,
                rating,
                favorite,
                description,
                location,
                img_width: width,
                img_height: height,
                faces,
            });
        }
        result
    };

    // ── Step 3: write sidecars off the main thread ───────────────────────────
    let (xmp_written, xmp_failed) = tokio::task::spawn_blocking(move || {
        let mut ok = 0usize;
        let mut err = 0usize;
        for data in &all_xmp {
            if xmp::write_xmp_full(data).is_ok() {
                ok += 1;
            } else {
                err += 1;
            }
        }
        (ok, err)
    })
    .await
    .map_err(|e| e.to_string())?;

    Ok(BatchTagXmpResult {
        tags_added,
        xmp_written,
        xmp_failed,
    })
}

// ── Find Similar (CLIP) ───────────────────────────────────────────────────

#[tauri::command]
pub async fn find_similar(photo_id: i64, limit: usize, state: tauri::State<'_, AppState>) -> Result<Vec<SimilarResult>, String> {
    let (target_emb_bytes, all_embs) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let target = db::get_clip_embedding(&conn, photo_id)
            .map_err(|e| e.to_string())?
            .ok_or("Photo has no CLIP embedding. Index CLIP first.")?;
        let all = db::get_all_clip_embeddings_except(&conn, photo_id)
            .map_err(|e| e.to_string())?;
        (target, all)
    };

    let target_emb = clip::bytes_to_embedding(&target_emb_bytes);

    let mut scored: Vec<(i64, f32)> = all_embs.iter()
        .map(|(id, bytes)| {
            let emb = clip::bytes_to_embedding(bytes);
            (*id, clip::cosine_similarity(&target_emb, &emb))
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit.min(100));

    let photo_ids: Vec<i64> = scored.iter().map(|(id, _)| *id).collect();
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let photos = db::get_photos_by_ids(&conn, &photo_ids).map_err(|e| e.to_string())?;

    let mut results: Vec<SimilarResult> = vec![];
    for (id, sim) in &scored {
        if let Some(photo) = photos.iter().find(|p| p.id == *id) {
            results.push(SimilarResult {
                photo: photo.clone(),
                similarity: *sim,
            });
        }
    }
    Ok(results)
}

// ── Find by example ───────────────────────────────────────────────────────
// User drags an arbitrary image (from file-system / clipboard / Explorer)
// onto the UI; we encode it with CLIP and return the library photos whose
// embeddings are closest in cosine distance. Unlike `find_similar`, the
// query image does NOT need to be in the library.

/// v1.5.55 — Find visually similar photos to an image already on disk
/// or to raw bytes the user just pasted from the clipboard. The bytes
/// path writes to a per-process temp file under the OS temp dir, runs
/// the same similarity pipeline, then deletes the temp.
#[tauri::command]
pub async fn find_similar_by_image_bytes(
    bytes: Vec<u8>,
    extension: Option<String>,
    tier: crate::clip::ClipTier,
    limit: usize,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<Vec<SimilarResult>, String> {
    if bytes.is_empty() {
        return Err("Empty image bytes".into());
    }
    let ext = extension.unwrap_or_else(|| "png".to_string());
    let temp = std::env::temp_dir().join(format!(
        "retinatag_paste_{}_{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        ext
    ));
    std::fs::write(&temp, &bytes).map_err(|e| format!("write temp: {}", e))?;
    let temp_str = temp.to_string_lossy().to_string();
    let result = find_similar_by_image_path(temp_str.clone(), tier, limit, state, app).await;
    let _ = std::fs::remove_file(&temp);
    result
}

#[tauri::command]
pub async fn find_similar_by_image_path(
    image_path: String,
    tier: crate::clip::ClipTier,
    limit: usize,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<Vec<SimilarResult>, String> {
    let p = std::path::PathBuf::from(&image_path);
    if !p.exists() {
        return Err(format!("Image not found: {}", image_path));
    }

    let base = clip_models_dir(&app);

    let photo_embs: Vec<(i64, Vec<u8>)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_photos_with_clip_emb(&conn, tier.dir_name()).map_err(|e| e.to_string())?
    };
    if photo_embs.is_empty() {
        return Err("No CLIP-indexed photos. Index your library first.".to_string());
    }

    let db_arc = state.db.clone();
    let img_path_owned = image_path.clone();

    let scored = tokio::task::spawn_blocking(move || -> Result<Vec<(i64, f32)>, String> {
        let img = image::open(&img_path_owned).map_err(|e| format!("open image: {}", e))?;
        let mut engine = crate::clip::load_engine(&base, tier).map_err(|e| e.to_string())?;
        let q_emb = crate::clip::encode_image(&mut engine, &img).map_err(|e| e.to_string())?;

        let mut scored: Vec<(i64, f32)> = photo_embs.iter()
            .map(|(pid, bytes)| {
                let e = crate::clip::bytes_to_embedding(bytes);
                (*pid, crate::clip::cosine_similarity(&q_emb, &e))
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit.max(1).min(200));
        Ok(scored)
    })
    .await
    .map_err(|e| e.to_string())??;

    let photo_ids: Vec<i64> = scored.iter().map(|(id, _)| *id).collect();
    let conn = db_arc.lock().map_err(|_| "db lock")?;
    let photos = db::get_photos_by_ids(&conn, &photo_ids).map_err(|e| e.to_string())?;

    let mut results: Vec<SimilarResult> = vec![];
    for (id, sim) in &scored {
        if let Some(photo) = photos.iter().find(|p| p.id == *id) {
            results.push(SimilarResult {
                photo: photo.clone(),
                similarity: *sim,
            });
        }
    }
    Ok(results)
}

// ── Crop editor ───────────────────────────────────────────────────────────
// Frontend <canvas> produces a Uint8Array of JPEG/PNG bytes. We write it to
// a sibling file (never overwrite the original) and optionally queue it into
// the scanner so it picks up tags & CLIP embedding like any other photo.
// Returns the new file's absolute path so the UI can highlight it.

#[tauri::command]
pub async fn save_cropped_image(
    source_path: String,
    bytes: Vec<u8>,
    suffix: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    use std::path::PathBuf;
    let src = PathBuf::from(&source_path);
    let parent = src.parent().ok_or("source has no parent directory")?.to_path_buf();
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("photo").to_string();
    let ext = src.extension().and_then(|s| s.to_str()).unwrap_or("jpg").to_string();
    let sfx = suffix.unwrap_or_else(|| "-crop".to_string());

    // v1.5.148 — Source photo can live on an SMB share (D:\Fotograflar
    // for the canonical user). Both the .exists() filename probe and
    // the write itself stat() the network drive — direct on a tokio
    // worker, those would starve the IPC mutex the same way the
    // v1.5.143 audit found in get_file_date. Move the whole file-IO
    // tail into spawn_blocking. Sibling filename selection happens on
    // the same dedicated thread so we don't pay 500 round-trips just
    // to pick a free name.
    let out_path: PathBuf = tauri::async_runtime::spawn_blocking(move || -> Result<PathBuf, String> {
        let mut out = parent.join(format!("{}{}.{}", stem, sfx, ext));
        let mut n = 2usize;
        while out.exists() {
            out = parent.join(format!("{}{}-{}.{}", stem, sfx, n, ext));
            n += 1;
            if n > 500 { return Err("could not find free filename".into()); }
        }
        std::fs::write(&out, &bytes).map_err(|e| format!("write crop: {}", e))?;
        Ok(out)
    })
    .await
    .map_err(|e| format!("join error: {}", e))??;
    let out = out_path;

    // Intentionally do NOT insert a DB row here. The folder watcher picks up
    // the new file on its next tick and runs the full scan pipeline on it
    // (hash, metadata, thumbnail, CLIP embedding if indexing). Duplicating
    // partial inserts here just creates an intermediate "ghost" row that the
    // watcher then has to reconcile.
    let _ = &state; // silence unused warning when feature paths change
    Ok(out.to_string_lossy().to_string())
}

// ── Color Extraction ──────────────────────────────────────────────────────

#[tauri::command]
pub async fn extract_colors_batch(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<usize, String> {
    let photos = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_photos_without_colors(&conn, 5000).map_err(|e| e.to_string())?
    };
    let total = photos.len();
    let db_arc = state.db.clone();
    let thumbs_dir = state.thumbnails_dir.clone();

    let count = tokio::task::spawn_blocking(move || {
        let mut done = 0usize;
        for (id, path, hash) in &photos {
            // Try to open image (reuse existing thumbnail pipeline)
            if let Ok(img) = crate::thumbnail::open_image(path) {
                let colors = crate::thumbnail::extract_dominant_colors(&img, 5);
                if !colors.is_empty() {
                    let json = serde_json::to_string(&colors).unwrap_or_default();
                    if let Ok(conn) = db_arc.lock() {
                        db::update_dominant_colors(&conn, *id, &json).ok();
                    }
                    done += 1;
                }
            }
        }
        done
    }).await.map_err(|e| e.to_string())?;

    Ok(count)
}

#[tauri::command]
pub async fn search_by_color(hex_color: String, tolerance: f32, state: tauri::State<'_, AppState>) -> Result<Vec<PhotoSummary>, String> {
    // Parse target color
    let hex = hex_color.trim_start_matches('#');
    if hex.len() != 6 { return Err("Invalid hex color".into()); }
    let tr = u8::from_str_radix(&hex[0..2], 16).map_err(|_| "bad hex")? as f32;
    let tg = u8::from_str_radix(&hex[2..4], 16).map_err(|_| "bad hex")? as f32;
    let tb = u8::from_str_radix(&hex[4..6], 16).map_err(|_| "bad hex")? as f32;
    let tol = tolerance.max(10.0);

    let conn = state.db.lock().map_err(|_| "db lock")?;
    // v1.5.63 — Faz 1: vault filter on color search.
    let mut stmt = conn.prepare(
        "SELECT id, path, filename, status, provider_used,
                (SELECT COUNT(*) FROM tags WHERE photo_id = p.id) AS tag_count,
                COALESCE((SELECT GROUP_CONCAT(tag, '|||') FROM (SELECT tag FROM tags WHERE photo_id = p.id LIMIT 10)), '') AS tag_list,
                p.media_type, p.date_taken, p.duration_secs, p.rating, p.favorite, p.dominant_colors
         FROM photos p WHERE p.dominant_colors IS NOT NULL AND p.private = 0"
    ).map_err(|e| e.to_string())?;

    let results: Vec<PhotoSummary> = stmt.query_map([], |row| {
        let colors_json: String = row.get(12)?;
        // Check if any dominant color matches
        let matches = if let Ok(colors) = serde_json::from_str::<Vec<String>>(&colors_json) {
            colors.iter().any(|c| {
                let h = c.trim_start_matches('#');
                if h.len() != 6 { return false; }
                let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0) as f32;
                let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0) as f32;
                let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0) as f32;
                let dist = ((r-tr).powi(2) + (g-tg).powi(2) + (b-tb).powi(2)).sqrt();
                dist <= tol
            })
        } else { false };

        if matches {
            let tag_list: String = row.get(6)?;
            let tags: Vec<String> = if tag_list.is_empty() { vec![] } else { tag_list.split("|||").map(|s| s.to_string()).collect() };
            Ok(Some(PhotoSummary {
                id: row.get(0)?,
                path: row.get(1)?,
                filename: row.get(2)?,
                status: row.get(3)?,
                provider_used: row.get(4)?,
                tag_count: row.get(5)?,
                tags,
                media_type: row.get::<_, Option<String>>(7)?.unwrap_or_else(|| "image".to_string()),
                date_taken: row.get(8)?,
                duration_secs: row.get(9)?,
                rating: row.get(10)?,
                favorite: row.get::<_, i32>(11)? != 0,
            }))
        } else {
            Ok(None)
        }
    }).map_err(|e| e.to_string())?
    .filter_map(|r| r.ok())
    .filter_map(|r| r)
    .collect();

    Ok(results)
}

// ── Library Analytics ──────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_library_analytics(state: tauri::State<'_, AppState>) -> Result<LibraryAnalytics, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_library_analytics(&conn).map_err(|e| e.to_string())
}

// ── Calendar View ──────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_photos_calendar(year: i32, month: i32, state: tauri::State<'_, AppState>) -> Result<Vec<CalendarDay>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_photos_calendar(&conn, year, month).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_year_month_counts(state: tauri::State<'_, AppState>) -> Result<Vec<(i32, i32, i64)>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_year_month_counts(&conn).map_err(|e| e.to_string())
}

// ── Health Check ───────────────────────────────────────────────────────────

#[tauri::command]
pub async fn run_health_check(state: tauri::State<'_, AppState>) -> Result<HealthReport, String> {
    // v1.5.144 — Two changes versus the previous version:
    //   1. The path.exists() walk happens inside spawn_blocking so a
    //      stale path can't strangle a tokio worker (same class as the
    //      v1.5.143 get_file_date fix).
    //   2. Paths on a network drive that's currently unreachable are
    //      NOT counted as orphans. is_network_path() detects mapped
    //      SMB drives and UNC paths; if the share is offline we'd
    //      otherwise classify every photo as deleted, and the user
    //      could then click "Fix" (fix_health_issues) and wipe every
    //      DB row. Mac shipped the same guard in v1.5.132 using
    //      /sbin/mount; here we use GetDriveTypeW.
    let all_photos = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_all_photo_paths(&conn).map_err(|e| e.to_string())?
    };
    let total_checked = all_photos.len() as i64;
    let thumbs_dir = state.thumbnails_dir.clone();

    let (orphaned, actual_thumb_files): (Vec<(i64, String)>, i64) = tauri::async_runtime::spawn_blocking(move || {
        // Cache reachability per unique share root so a library with
        // 50k photos on one share probes the root ONCE, not 50k times.
        let mut share_status: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
        let orphaned: Vec<(i64, String)> = all_photos.into_iter()
            .filter(|(_, path)| {
                if is_network_path(path) {
                    let root = share_root_of(path);
                    let reachable = *share_status.entry(root.clone())
                        .or_insert_with(|| std::fs::metadata(&root).is_ok());
                    if !reachable {
                        // Share is down — DEFER, don't classify as
                        // orphan. Wrong answer here would let the
                        // user wipe their library's DB rows.
                        return false;
                    }
                    // Share is live → file genuinely missing is a real
                    // orphan. Fall through to the normal exists() check.
                }
                !std::path::Path::new(path).exists()
            })
            .collect();
        let actual = std::fs::read_dir(&thumbs_dir).map(|d| d.count()).unwrap_or(0) as i64;
        (orphaned, actual)
    })
    .await
    .map_err(|e| format!("join error: {}", e))?;

    let conn = state.db.lock().map_err(|_| "db lock")?;
    let missing_thumbs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM photos WHERE thumbnail_path IS NOT NULL AND thumbnail_path != ''",
        [], |r| r.get(0)
    ).unwrap_or(0);
    let missing_thumbnails = (missing_thumbs - actual_thumb_files).max(0);

    Ok(HealthReport {
        orphaned_entries: orphaned,
        missing_thumbnails,
        total_checked,
    })
}

// v1.5.144 — Compute the share root for a path so callers can cache
// reachability per unique root rather than per-row. For UNC paths
// (`\\server\share\folder\file`) the root is `\\server\share`; for
// drive-letter paths (`D:\folder\file`) the root is `D:\`.
//
// Returned as a String so the caller can use it as a HashMap key
// without dealing with PathBuf hashing quirks across Windows/Unix
// case folding. We never mutate the value — just probe it via
// std::fs::metadata().
fn share_root_of(path: &str) -> String {
    if path.starts_with(r"\\") || path.starts_with("//") {
        let parts: Vec<&str> = path.split(|c| c == '\\' || c == '/').filter(|s| !s.is_empty()).collect();
        if parts.len() >= 2 {
            return format!(r"\\{}\{}", parts[0], parts[1]);
        }
    }
    if path.len() >= 3 && path.as_bytes()[1] == b':' {
        return format!("{}:\\", path.chars().next().unwrap());
    }
    // Fall back to the path itself — metadata() will error and the
    // caller will defer the row, which is the safe outcome.
    path.to_string()
}

#[tauri::command]
pub async fn fix_health_issues(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    // v1.5.75 — Was deleting orphan DB rows but leaving their cached
    // thumbnail files (<hash>.jpg) and any .rtenc vault blobs on disk.
    // Over a long-running library that grew → shrank, the thumbnails dir
    // could be many GB of leaked files invisible to the UI. Now we look
    // up the hash + path before delete so we can wipe the thumbnail and
    // any encrypted blob too.
    //
    // v1.5.144 — Two safety fixes:
    //   1. Whole orphan walk now runs inside spawn_blocking so the
    //      path.exists() calls don't strangle the tokio runtime when
    //      any photo lives on a slow share.
    //   2. Network paths on an unreachable share are NOT classified
    //      as orphans. Without this, a momentary SMB outage at the
    //      time the user clicks "Fix" would cause every photo on
    //      that share to be deleted from the DB — irrecoverable
    //      without a backup. Mirrors v1.5.132 on Mac.
    let thumbs_dir = state.thumbnails_dir.clone();
    let all_rows: Vec<(i64, String, String)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let mut stmt = conn
            .prepare("SELECT id, COALESCE(hash, ''), path FROM photos")
            .map_err(|e| e.to_string())?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };
    let thumbs_dir_clone = thumbs_dir.clone();
    let orphan_rows: Vec<(i64, String, String)> = tauri::async_runtime::spawn_blocking(move || {
        let mut share_status: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
        all_rows.into_iter()
            .filter(|(_, _, p)| {
                if is_network_path(p) {
                    let root = share_root_of(p);
                    let reachable = *share_status.entry(root.clone())
                        .or_insert_with(|| std::fs::metadata(&root).is_ok());
                    if !reachable {
                        return false; // defer — share is down
                    }
                }
                !std::path::Path::new(p).exists()
            })
            .collect()
    })
    .await
    .map_err(|e| format!("join error: {}", e))?;

    if orphan_rows.is_empty() {
        return Ok(0);
    }
    // Delete the thumbnail + .rtenc files BEFORE removing the DB rows so a
    // crash mid-delete leaves recoverable state (next health-check will
    // see the orphans again and retry). Also blocking IO — keep it on
    // the same dedicated thread.
    let orphan_for_cleanup = orphan_rows.clone();
    tauri::async_runtime::spawn_blocking(move || {
        for (_id, hash, path) in &orphan_for_cleanup {
            if !hash.is_empty() {
                let cache = thumbnail::thumb_cache_name(hash);
                let tp = thumbs_dir_clone.join(&cache);
                let _ = std::fs::remove_file(&tp);
            }
            // .rtenc vault blob (in case the user un-vaulted then deleted the
            // restored file outside RetinaTag).
            if path.ends_with(".rtenc") {
                let _ = std::fs::remove_file(path);
            }
        }
    })
    .await
    .map_err(|e| format!("join error: {}", e))?;
    let orphaned: Vec<i64> = orphan_rows.iter().map(|(id, _, _)| *id).collect();
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::delete_photos_by_ids(&conn, &orphaned).map_err(|e| e.to_string())
}

// ── Skip Face (single or cluster) ────────────────────────────────────────────

#[tauri::command]
pub async fn skip_face(face_id: i64, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    // Aggressive skip-with-propagation: mark this face AND every visually
    // similar unassigned face as skipped. 0.35 cosine similarity is loose
    // enough to catch the same person across pose/lighting variations but
    // tight enough to avoid wrongly skipping a different person.
    let total = db::mark_face_skipped_propagate(&conn, face_id, 0.30)
        .map_err(|e| e.to_string())?;
    eprintln!("[face] skip_face: marked {} face(s) as skipped (incl. propagation)", total);
    Ok(())
}

/// Undo a Skip on a set of faces: resets `person_id` from -1 back to NULL so
/// the face re-appears in the Unknown queue. Only touches rows currently
/// flagged as skipped (person_id = -1) — never disturbs real assignments.
#[tauri::command]
pub async fn undo_face_skip(
    face_ids: Vec<i64>,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let mut reverted = 0usize;
    let mut stmt = conn
        .prepare("UPDATE face_regions SET person_id = NULL WHERE id = ?1 AND person_id = -1")
        .map_err(|e| e.to_string())?;
    for fid in &face_ids {
        if let Ok(n) = stmt.execute(rusqlite::params![fid]) {
            reverted += n;
        }
    }
    eprintln!("[face] undo_face_skip: reverted {} face(s)", reverted);
    Ok(reverted)
}

/// Mass-revert every skipped face (person_id = -1) back to unassigned
/// (person_id = NULL) so they re-appear in the Unknown queue.
///
/// Recovery path for when the v1.4.34 insert-time auto-skip check has been
/// too aggressive and the user wants to re-review previously skipped faces.
/// Only touches rows currently flagged as skipped — named faces (person_id
/// pointing at a real person) are left completely untouched.
///
/// Also wipes the `last_shown_face_ids` cache (both the in-memory copy and
/// the DB-persisted mirror). Otherwise the next get_unknown_faces call
/// would immediately re-skip every face we just reset, defeating the whole
/// recovery.
#[tauri::command]
pub async fn reset_all_skipped_faces(
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let reverted = conn
        .execute(
            "UPDATE face_regions SET person_id = NULL WHERE person_id = -1",
            [],
        )
        .map_err(|e| e.to_string())?;

    // Clear the "previously shown" tracker so the next popup won't auto-skip
    // the freshly-revived faces. Mirror to DB too in case of app restart.
    *state.last_shown_face_ids.lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
    persist_shown_face_ids(&conn, &[]);

    eprintln!("[face] reset_all_skipped_faces: reverted {} face(s) from skipped to unassigned", reverted);
    Ok(reverted)
}

/// Count how many faces are currently marked as skipped (person_id = -1).
/// Used by the UI to decide whether to expose the "reset skipped faces"
/// recovery button — no point showing it when there's nothing to recover.
#[tauri::command]
pub async fn count_skipped_faces(
    state: tauri::State<'_, AppState>,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM face_regions WHERE person_id = -1",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(n)
}

/// Undo a Save (name) action: unassign every face from the given person, and
/// if `delete_person` is true, delete the person row itself (used when the
/// person was newly created by the Save and the user immediately went Back).
///
/// Also removes the auto-inserted face-source tag from every photo whose
/// faces got unassigned — otherwise the person's name would linger as a
/// regular tag even after the assignment is gone.
#[tauri::command]
pub async fn undo_face_name(
    person_id: i64,
    delete_person: bool,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;

    // 1. Grab the name (for tag cleanup) and the photos that will be affected
    let name: Option<String> = conn
        .query_row(
            "SELECT name FROM persons WHERE id = ?1",
            rusqlite::params![person_id],
            |r| r.get(0),
        )
        .ok();

    let affected_photos: Vec<i64> = conn
        .prepare("SELECT DISTINCT photo_id FROM face_regions WHERE person_id = ?1")
        .ok()
        .and_then(|mut s| {
            s.query_map(rusqlite::params![person_id], |r| r.get::<_, i64>(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
        })
        .unwrap_or_default();

    // 2. Unassign every face pointing at this person
    let reverted = conn
        .execute(
            "UPDATE face_regions SET person_id = NULL WHERE person_id = ?1",
            rusqlite::params![person_id],
        )
        .map_err(|e| e.to_string())?;

    // 3. Remove the face-source tag for this person from those photos
    if let Some(nm) = name.as_deref() {
        let mut del = conn
            .prepare(
                "DELETE FROM tags WHERE photo_id = ?1 AND tag = ?2 AND source = 'face'",
            )
            .map_err(|e| e.to_string())?;
        for pid in &affected_photos {
            del.execute(rusqlite::params![pid, nm]).ok();
        }
    }

    // 4. Optionally delete the person row
    if delete_person {
        conn.execute(
            "DELETE FROM persons WHERE id = ?1",
            rusqlite::params![person_id],
        )
        .ok();
    }

    eprintln!(
        "[face] undo_face_name: unassigned {} face(s) from person {} (delete={})",
        reverted, person_id, delete_person
    );
    Ok(reverted)
}

/// Return the subset of the given face IDs that are STILL unassigned
/// (person_id IS NULL). Used by the Who-is-this popup after Skip, to prune
/// entries from the current batch whose representative face was silently
/// propagated to "skipped" (person_id = -1). Unlike `get_unknown_faces`,
/// this does NOT mutate state or auto-skip anything — it's a pure filter.
#[tauri::command]
pub async fn filter_still_unknown(
    face_ids: Vec<i64>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<i64>, String> {
    if face_ids.is_empty() {
        return Ok(vec![]);
    }
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let placeholders = face_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT id FROM face_regions WHERE id IN ({}) AND person_id IS NULL",
        placeholders
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let params: Vec<&dyn rusqlite::ToSql> = face_ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
    let still: Vec<i64> = stmt
        .query_map(params.as_slice(), |r| r.get::<_, i64>(0))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();
    Ok(still)
}

/// Skip all faces in a cluster at once AND aggressively propagate the skip to
/// every visually-similar unassigned face based on the **cluster centroid**
/// (not per-face). A single photo's embedding can be noisy (bad lighting,
/// angle), but the average of 3-10 photos of the same person is much more
/// stable — so we can safely drop the threshold to 0.40 without false positives.
///
/// This is what the "Skip" button in the Who-is-this popup calls, so pressing
/// Skip really does mean "never show me this person again".
#[tauri::command]
pub async fn skip_face_cluster(face_ids: Vec<i64>, state: tauri::State<'_, AppState>) -> Result<usize, String> {
    // v1.4.34: lowered from 0.40 → 0.32 to match SKIP_THRESH in
    // `get_unknown_faces`. Previous value left a gap where other angles of
    // the same skipped person (cos sim ≈ 0.32–0.39 with the seed centroid)
    // slipped through → reappeared in the next batch. Detection-time
    // insert-skip now also catches this at 0.30 flat / 0.35 centroid, but
    // propagating more aggressively at user-skip time means fewer orphan
    // NULL rows lying around in the DB waiting to surface.
    //
    // v1.5.29: further lowered 0.32 → 0.27 because user reported the loop
    // was *still* happening. When the popup cluster is a singleton the
    // "centroid" is just the one emb, so this is effectively a flat cosine
    // check — at the user-skip boundary we have explicit permission to be
    // aggressive ("don't ask about this person again"), so 0.27 matches the
    // new query-time threshold and closes the gap at skip-click time too.
    const CENTROID_THRESHOLD: f32 = 0.27;
    let conn = state.db.lock().map_err(|_| "db lock")?;

    if face_ids.is_empty() {
        return Ok(0);
    }

    // 1. Mark every seed face in the cluster as skipped (person_id = -1).
    //    This also handles faces that have no embedding.
    {
        let mut upd = conn.prepare(
            "UPDATE face_regions SET person_id = -1 WHERE id = ?1 AND (person_id IS NULL OR person_id = -1)"
        ).map_err(|e| e.to_string())?;
        for fid in &face_ids {
            upd.execute(rusqlite::params![fid]).ok();
        }
    }

    // 2. Gather the embeddings of the seed faces.
    let seed_embs: Vec<Vec<f32>> = {
        let placeholders = face_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT embedding FROM face_regions WHERE id IN ({}) AND embedding IS NOT NULL",
            placeholders
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let params: Vec<&dyn rusqlite::ToSql> = face_ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
        let rows: Vec<Vec<u8>> = stmt
            .query_map(params.as_slice(), |r| r.get::<_, Vec<u8>>(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        rows.into_iter()
            .map(|b| crate::face::bytes_to_embedding(&b))
            .filter(|e| e.len() == 512)
            .collect()
    };

    if seed_embs.is_empty() {
        eprintln!("[face] skip_face_cluster: {} face(s) skipped (no embeddings for centroid propagation)",
                  face_ids.len());
        return Ok(face_ids.len());
    }

    // 3. Compute the cluster centroid — this is the stable "fingerprint".
    let centroid = crate::face::compute_centroid(&seed_embs);
    if centroid.len() != 512 {
        return Ok(face_ids.len());
    }

    // 4. Pull all unassigned faces that have embeddings, and skip any that
    //    are close enough to the centroid.
    let candidates: Vec<(i64, Vec<u8>)> = {
        let mut stmt = conn.prepare(
            "SELECT id, embedding FROM face_regions
             WHERE person_id IS NULL AND embedding IS NOT NULL"
        ).map_err(|e| e.to_string())?;
        let rows: Vec<(i64, Vec<u8>)> = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    let mut propagated = 0usize;
    {
        let mut upd = conn.prepare(
            "UPDATE face_regions SET person_id = -1 WHERE id = ?1 AND person_id IS NULL"
        ).map_err(|e| e.to_string())?;
        for (cand_id, cand_bytes) in candidates {
            let cand_emb = crate::face::bytes_to_embedding(&cand_bytes);
            if cand_emb.len() != 512 { continue; }
            let sim = crate::face::cosine_similarity(&centroid, &cand_emb);
            if sim >= CENTROID_THRESHOLD {
                if upd.execute(rusqlite::params![cand_id]).unwrap_or(0) > 0 {
                    propagated += 1;
                }
            }
        }
    }

    let total = face_ids.len() + propagated;
    eprintln!(
        "[face] skip_face_cluster: {} seed + {} propagated (centroid thresh {}) = {} total",
        face_ids.len(), propagated, CENTROID_THRESHOLD, total
    );
    Ok(total)
}

/// Batch-skip faces: marks all given face IDs as skipped, but ONLY if they
/// haven't already been assigned to a person (person_id IS NULL).
/// Called after the face popup closes to skip all faces the user didn't name.
/// Each skip propagates to visually-similar unassigned faces.
#[tauri::command]
pub async fn skip_faces_batch(face_ids: Vec<i64>, state: tauri::State<'_, AppState>) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let mut count = 0usize;
    for fid in &face_ids {
        // Check it's still unassigned (don't override a name the user just typed)
        let still_unassigned: bool = conn.query_row(
            "SELECT 1 FROM face_regions WHERE id = ?1 AND person_id IS NULL",
            rusqlite::params![fid],
            |_| Ok(true),
        ).unwrap_or(false);
        if still_unassigned {
            // v1.5.29 — 0.30 → 0.25 to match the insert-time flat gate's new
            // threshold. Keeps the two "I just skipped, kill siblings" code
            // paths (skip_face_cluster with its 0.27 centroid + this batch
            // propagation at 0.25 flat) in sync.
            if let Ok(n) = db::mark_face_skipped_propagate(&conn, *fid, 0.25) {
                count += n;
            }
        }
    }
    eprintln!("[face] skip_faces_batch: total {} face(s) skipped (incl. propagation) from {} seeds",
              count, face_ids.len());
    Ok(count)
}

/// Nuclear option: mark EVERY unassigned face (person_id IS NULL) as skipped.
/// Used when the user is sick of seeing the same unknown faces and wants a
/// clean slate. Returns the number of faces affected.
///
/// Foreign-key enforcement is disabled for the duration of the UPDATE because
/// person_id has a REFERENCES persons(id) constraint and -1 is a sentinel
/// value (not a real person row). Single-row updates elsewhere also use -1
/// and work because FK enforcement happens per-statement; a bulk UPDATE
/// over 40k rows can still trigger validation issues so we explicitly
/// disable it for this one statement.
#[tauri::command]
pub async fn skip_all_unknown_faces(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    // Save current FK state, disable, run UPDATE, restore.
    conn.execute_batch("PRAGMA foreign_keys = OFF;").map_err(|e| e.to_string())?;
    let result = conn.execute(
        "UPDATE face_regions SET person_id = -1 WHERE person_id IS NULL",
        [],
    );
    // Always restore FK enforcement, even on error
    conn.execute_batch("PRAGMA foreign_keys = ON;").ok();
    let n = result.map_err(|e| e.to_string())?;
    eprintln!("[face] skip_all_unknown_faces: marked {} face(s) as skipped", n);
    Ok(n)
}

/// Count of unassigned faces (person_id IS NULL) — used by UI to decide
/// whether to offer the "skip all pending" option.
#[tauri::command]
pub async fn count_unknown_faces(state: tauri::State<'_, AppState>) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM face_regions WHERE person_id IS NULL",
        [],
        |r| r.get(0),
    ).map_err(|e| e.to_string())?;
    Ok(n)
}

// ── Delete Photos (move to recycle bin or remove from DB) ─────────────────

#[tauri::command]
pub async fn delete_photos(photo_ids: Vec<i64>, delete_file: bool, state: tauri::State<'_, AppState>) -> Result<usize, String> {
    // Collect original paths + cached thumbnail paths BEFORE the DB delete.
    // Thumbnails live in app_data/thumbnails and are keyed by content hash —
    // if we don't remove them here they accumulate forever (leak).
    let (orig_paths, thumb_paths): (Vec<(i64, String)>, Vec<String>) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let mut orig = Vec::new();
        let mut thumbs = Vec::new();
        for &id in &photo_ids {
            if delete_file {
                if let Ok((path, _)) = db::get_photo_path_and_hash(&conn, id) {
                    orig.push((id, path));
                }
            }
            if let Ok(Some(tp)) = db::get_photo_thumbnail_path(&conn, id) {
                if !tp.is_empty() {
                    thumbs.push(tp);
                }
            }
        }
        (orig, thumbs)
    };
    // DB lock released here — now do slow file operations.
    // v1.5.46 — Was spawning ONE PowerShell process per file to send each
    // photo to the Recycle Bin. PS startup is 200-500 ms on Windows; for
    // a 100-photo delete that's 20-50 SECONDS of pure process-launch
    // overhead. Now we spawn a SINGLE PS process and feed it all paths
    // through a temp file (avoids quote-escaping nightmares for paths
    // with apostrophes or non-ASCII), so the whole batch finishes in
    // roughly the time of one launch + one VB.FileSystem call per file.
    if delete_file && !orig_paths.is_empty() {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            // Write paths as UTF-8 BOM so PowerShell's Get-Content with
            // -Encoding UTF8 round-trips Turkish characters cleanly.
            let temp = std::env::temp_dir().join(format!(
                "retinatag_recycle_{}_{}.txt",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let mut blob = String::with_capacity(orig_paths.iter().map(|(_, p)| p.len() + 2).sum());
            for (_, p) in &orig_paths {
                blob.push_str(p);
                blob.push('\n');
            }
            // BOM-prefixed write so PowerShell reads it as UTF-8.
            let mut bytes: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
            bytes.extend_from_slice(blob.as_bytes());
            if std::fs::write(&temp, &bytes).is_ok() {
                let temp_str = temp.to_string_lossy().replace('\'', "''");
                let ps_script = format!(
                    "Add-Type -AssemblyName Microsoft.VisualBasic; \
                     $ErrorActionPreference = 'SilentlyContinue'; \
                     Get-Content -LiteralPath '{}' -Encoding UTF8 | ForEach-Object {{ \
                       if ($_ -and (Test-Path -LiteralPath $_)) {{ \
                         [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile($_,'OnlyErrorDialogs','SendToRecycleBin') \
                       }} \
                     }}",
                    temp_str
                );
                // v1.5.75 — log PS failure instead of silently dropping it.
                // If powershell.exe isn't on PATH (locked-down build) or the
                // process exits non-zero, we still proceed to clean up the
                // DB + thumbs (the user asked for delete) but record what
                // failed so support can see why files might still be on
                // disk after a "delete to recycle bin" action.
                match std::process::Command::new("powershell.exe")
                    .args(["-NoProfile", "-NonInteractive", "-Command", &ps_script])
                    .creation_flags(0x08000000)
                    .output()
                {
                    Ok(out) if !out.status.success() => {
                        eprintln!(
                            "[delete] PowerShell recycle exited {:?}: stderr={}",
                            out.status.code(),
                            String::from_utf8_lossy(&out.stderr)
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "[delete] PowerShell recycle failed to spawn: {} — files left on disk",
                            e
                        );
                    }
                    _ => {}
                }
                std::fs::remove_file(&temp).ok();
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            for (_, path) in &orig_paths {
                std::fs::remove_file(path).ok();
            }
        }
    }
    // Remove cached thumbnail files so the app-data thumbnails dir doesn't leak.
    // Best-effort: ignore errors (file might already be gone).
    for tp in &thumb_paths {
        std::fs::remove_file(tp).ok();
    }
    // Now re-acquire lock for DB deletion
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let deleted = db::delete_photos_by_ids(&conn, &photo_ids).map_err(|e| e.to_string())?;
    Ok(deleted)
}

// ── Smart Rename ───────────────────────────────────────────────────────────

#[tauri::command]
pub async fn generate_smart_names(photo_ids: Vec<i64>, state: tauri::State<'_, AppState>) -> Result<Vec<RenamePreview>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let mut previews = vec![];

    for (idx, &id) in photo_ids.iter().enumerate() {
        if let Ok((path, filename, desc, location, date)) = db::get_photo_rename_data(&conn, id) {
            let ext = std::path::Path::new(&filename)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("jpg")
                .to_lowercase();

            // Build smart name parts
            let loc_part = location.as_deref().unwrap_or("Unknown");
            let desc_part = desc.as_deref().unwrap_or("photo")
                .split_whitespace().take(4).collect::<Vec<_>>().join("_");
            let date_part = date.as_deref().unwrap_or("")
                .chars().take(10).collect::<String>()
                .replace('-', "");

            // Sanitize
            let sanitize = |s: &str| -> String {
                s.chars()
                    .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
                    .collect::<String>()
                    .trim_matches('_')
                    .to_string()
            };

            let new_name = format!("{}_{}_{}_{:03}.{}",
                sanitize(loc_part),
                sanitize(&desc_part),
                sanitize(&date_part),
                idx + 1,
                ext
            );

            let parent = std::path::Path::new(&path).parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let new_path = format!("{}{}{}",
                parent,
                std::path::MAIN_SEPARATOR,
                new_name
            );

            previews.push(RenamePreview {
                photo_id: id,
                old_name: filename,
                new_name,
                old_path: path,
                new_path,
            });
        }
    }
    Ok(previews)
}

#[tauri::command]
pub async fn apply_rename(renames: Vec<RenamePreview>, state: tauri::State<'_, AppState>) -> Result<usize, String> {
    // v1.5.74 — Was a P1 split-brain bug: previously each iteration did
    // `fs::rename` then `state.db.lock()` then `update_photo_path` — but
    // if the DB write failed (lock poisoned, DB locked by another writer)
    // the file was on disk under the new name while the DB still pointed
    // at the old path → photo "disappeared" from the gallery and the user
    // thought it was lost. Also re-acquired the lock per iteration, so a
    // 1000-photo rename took 1000 lock round-trips.
    //
    // Fix: hold one transaction across the whole batch. For each rename,
    // try the FS rename first; if the DB update fails, roll back the FS
    // rename (move file back to old path) so the on-disk state matches
    // the DB state. The transaction itself rolls back any pending DB
    // writes on commit failure.
    let db_arc = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<usize, String> {
        let mut conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let mut count = 0usize;
        // Track FS renames so we can undo them if the transaction commit
        // fails. (Inside the loop, a per-row DB error already triggers an
        // immediate FS rollback for that row — see the inner `if let Err`.)
        let mut applied: Vec<(String, String)> = Vec::new(); // (new_path, old_path)
        for r in &renames {
            if std::fs::rename(&r.old_path, &r.new_path).is_err() {
                continue;
            }
            // rusqlite::Transaction Deref's to Connection so &*tx works
            // wherever &Connection is required.
            match db::update_photo_path(&*tx, r.photo_id, &r.new_path, &r.new_name) {
                Ok(_) => {
                    applied.push((r.new_path.clone(), r.old_path.clone()));
                    count += 1;
                }
                Err(_) => {
                    // Undo the FS rename so disk + DB stay in sync.
                    let _ = std::fs::rename(&r.new_path, &r.old_path);
                }
            }
        }
        if let Err(e) = tx.commit() {
            // Transaction rolled back. Undo all FS renames we'd applied
            // so the user's library isn't half-renamed with no DB record.
            for (new_p, old_p) in applied.iter().rev() {
                let _ = std::fs::rename(new_p, old_p);
            }
            return Err(format!("rename transaction failed: {}", e));
        }
        Ok(count)
    })
    .await
    .map_err(|e| e.to_string())?
}

// ── 27. Clear all face data (reset face detection) ───────────────────────────

#[tauri::command]
pub async fn clear_all_faces(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());

    let face_count: i64 = conn.query_row("SELECT COUNT(*) FROM face_regions", [], |r| r.get(0))
        .unwrap_or(0);
    let person_count: i64 = conn.query_row("SELECT COUNT(*) FROM persons", [], |r| r.get(0))
        .unwrap_or(0);
    let face_tag_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tags WHERE source = 'face'", [], |r| r.get(0)
    ).unwrap_or(0);

    // Disable FK during bulk delete:
    //   - face_regions has rows with person_id = -1 (skipped sentinel) which
    //     are NOT valid FK references. Without OFF, SQLite still tolerates
    //     deletes here, but be defensive.
    //   - tags reference photos via ON DELETE CASCADE — disabling FK avoids
    //     any cross-table cascade noise during the wipe.
    conn.execute_batch("PRAGMA foreign_keys = OFF;").ok();

    let result: Result<(), String> = (|| {
        conn.execute("DELETE FROM face_regions", []).map_err(|e| format!("face_regions: {e}"))?;
        conn.execute("DELETE FROM persons",      []).map_err(|e| format!("persons: {e}"))?;
        // Face-derived tags ('Cansın', 'Bob', …) live in `tags` with source='face'.
        // Without this delete, the photo cards still show person names after a
        // "clear all faces" — looks like nothing happened.
        conn.execute("DELETE FROM tags WHERE source = 'face'", [])
            .map_err(|e| format!("face tags: {e}"))?;
        // Reclaim AUTOINCREMENT counters so the next person.id starts at 1
        conn.execute("DELETE FROM sqlite_sequence WHERE name IN ('face_regions','persons')", []).ok();
        Ok(())
    })();

    conn.execute_batch("PRAGMA foreign_keys = ON;").ok();
    result?;

    // Reset in-memory "previously shown" tracker so the next get_unknown_faces
    // call starts with a clean slate (otherwise it would auto-skip face IDs
    // that no longer exist). Also clear the DB-persisted mirror — stale IDs
    // there would make the next popup silently skip unrelated new faces.
    *state.last_shown_face_ids.lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
    persist_shown_face_ids(&conn, &[]);

    eprintln!(
        "[face] clear_all_faces: removed {} faces, {} persons, {} face tags",
        face_count, person_count, face_tag_count
    );
    Ok(format!(
        "Cleared {} faces, {} persons, {} face tags",
        face_count, person_count, face_tag_count
    ))
}

// ── Memories: On This Day ──────────────────────────────────────────────────
//
// Returns every photo whose capture date falls on the given MM-DD across
// all years. The frontend renders this as a "On this day" sidebar entry —
// it's the Apple Photos-style memory feature, zero extra indexing cost
// (filtered at query time via SUBSTR on the already-indexed date).

#[tauri::command]
pub async fn get_on_this_day(
    month_day: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<serde_json::Value>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let rows = db::get_photos_on_this_day(&conn, &month_day).map_err(|e| e.to_string())?;
    let out = rows.into_iter().map(|(date, p)| serde_json::json!({
        "date": date,
        "photo": p,
    })).collect();
    Ok(out)
}

// ── Thumbnail Garbage Collection ───────────────────────────────────────────
//
// Walks the thumbnails directory and deletes any `<hash>.jpg` that no
// longer corresponds to a live photo row. Deleting photos through the UI
// already drops the thumb, but orphans still accumulate from:
//   - manual library.db resets
//   - crashes mid-delete
//   - older builds that forgot to clean up
//
// Returns (files_scanned, files_deleted, bytes_freed).

#[tauri::command]
pub async fn gc_thumbnails(
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let thumbs_dir = state.thumbnails_dir.clone();
    let hashes: Vec<String> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_all_thumbnail_hashes(&conn).map_err(|e| e.to_string())?
    };

    tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        // Build the set of thumbnail filenames the DB currently expects.
        // Matches what `thumb_cache_name` produces: first 24 chars of the
        // hash (stripped of any "xxh3:" / "sha256:" prefix) + ".jpg".
        let live: std::collections::HashSet<String> = hashes
            .iter()
            .map(|h| thumbnail::thumb_cache_name(h))
            .collect();

        let mut scanned = 0u32;
        let mut deleted = 0u32;
        let mut bytes_freed: u64 = 0;

        let entries = match std::fs::read_dir(&thumbs_dir) {
            Ok(e) => e,
            Err(e) => return Err(format!("read thumbs dir: {}", e)),
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jpg") {
                continue;
            }
            scanned += 1;
            let fname = match p.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if live.contains(&fname) {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(&p).is_ok() {
                deleted += 1;
                bytes_freed += size;
            }
        }

        Ok(serde_json::json!({
            "scanned": scanned,
            "deleted": deleted,
            "bytes_freed": bytes_freed,
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

// ── Missing File Detection + Relink ────────────────────────────────────────
//
// `find_missing_files` walks every photo row, stats its path, and returns
// the rows whose files no longer exist on disk (common after the user
// moves their library folder). The frontend can then offer to either
// delete-from-library or relink: given a candidate new path, we verify
// the file still hashes the same and update the photos row.

#[tauri::command]
pub async fn find_missing_files(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<serde_json::Value>, String> {
    let rows: Vec<(i64, String)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_all_id_paths(&conn).map_err(|e| e.to_string())?
    };

    tokio::task::spawn_blocking(move || {
        // Parallel stat is fine — it's pure read-only syscalls, and a 50k
        // library on a slow network share would take forever serially.
        use rayon::prelude::*;
        let missing: Vec<serde_json::Value> = rows
            .par_iter()
            .filter_map(|(id, path)| {
                if std::path::Path::new(path).exists() {
                    None
                } else {
                    let fname = std::path::Path::new(path)
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    Some(serde_json::json!({
                        "id": id,
                        "path": path,
                        "filename": fname,
                    }))
                }
            })
            .collect();
        Ok(missing)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Point a library photo at a new on-disk path. Re-hashes the new file
/// and refuses to relink if the content doesn't match — this prevents
/// silently bonding a photo's tags/ratings/faces to the wrong image.
#[tauri::command]
pub async fn relink_photo(
    photo_id: i64,
    new_path: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let expected_hash: String = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let mut stmt = conn.prepare("SELECT hash FROM photos WHERE id = ?1")
            .map_err(|e| e.to_string())?;
        stmt.query_row(rusqlite::params![photo_id], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
    };

    let np = new_path.clone();
    let new_hash = tokio::task::spawn_blocking(move || {
        crate::scanner::compute_hash(&np)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    if new_hash != expected_hash {
        return Err(format!(
            "Hash mismatch — refusing to relink. This file is not the same photo (was {}, new {}).",
            expected_hash, new_hash
        ));
    }

    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::relink_photo_path(&conn, photo_id, &new_path).map_err(|e| e.to_string())
}

// ══════════════════════════════════════════════════════════════════════════
// Phase 10 commands
// ══════════════════════════════════════════════════════════════════════════

// ── Tag output language (en / tr) ────────────────────────────────────────
#[tauri::command]
pub fn set_tag_language(
    lang: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let code = match lang.as_str() {
        "tr" | "TR" | "Turkish" | "turkish" => 1,
        _ => 0,
    };
    crate::providers::TAG_LANG.store(code, std::sync::atomic::Ordering::Relaxed);
    // Persist so it survives a restart.
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let _ = db::set_setting(&conn, "tag_language", if code == 1 { "tr" } else { "en" });
    Ok(())
}

#[tauri::command]
pub fn get_tag_language(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    Ok(db::get_setting(&conn, "tag_language").ok().flatten().unwrap_or_else(|| "en".to_string()))
}

// ── Tray / notification preferences ──────────────────────────────────────
//
// Stored as simple string settings ("1" / "0"). Frontend reads them on load
// to hydrate the settings panel; the window close handler reads close_to_tray
// on every close event so toggling takes effect immediately.

#[derive(serde::Serialize)]
pub struct TrayPrefs {
    pub close_to_tray: bool,
    pub start_minimized: bool,
    pub notify_on_complete: bool,
    pub notify_on_error: bool,
}

#[tauri::command]
pub fn get_tray_prefs(state: tauri::State<'_, AppState>) -> Result<TrayPrefs, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let read = |k: &str, default: bool| -> bool {
        db::get_setting(&conn, k)
            .ok()
            .flatten()
            .map(|s| s == "1")
            .unwrap_or(default)
    };
    Ok(TrayPrefs {
        close_to_tray: read("close_to_tray", false),
        start_minimized: read("start_minimized", false),
        notify_on_complete: read("notify_on_complete", true),
        notify_on_error: read("notify_on_error", true),
    })
}

#[tauri::command]
pub fn set_tray_prefs(
    close_to_tray: bool,
    start_minimized: bool,
    notify_on_complete: bool,
    notify_on_error: bool,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let _ = db::set_setting(&conn, "close_to_tray", if close_to_tray { "1" } else { "0" });
    let _ = db::set_setting(&conn, "start_minimized", if start_minimized { "1" } else { "0" });
    let _ = db::set_setting(&conn, "notify_on_complete", if notify_on_complete { "1" } else { "0" });
    let _ = db::set_setting(&conn, "notify_on_error", if notify_on_error { "1" } else { "0" });
    Ok(())
}


// ── Private vault ─────────────────────────────────────────────────────────

// v1.5.67 — replaced the sync version of `toggle_photo_private` with
// `toggle_photo_private_async` further down. The IPC entrypoint kept
// the same name so the FE doesn't need to change. See that function
// for the actual implementation.
#[allow(dead_code)]
fn _legacy_sync_toggle_photo_private_unused(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<bool, String> {
    // v1.5.64+ — Faz 2.1: with the vault KEK loaded, flipping INTO
    // private (a) encrypts the thumbnail blob into the DB and (b)
    // encrypts the original photo file in place to `<path>.rtenc`.
    // Flipping OUT reverses both. With no KEK loaded we toggle the
    // flag but skip crypto — the migration on next unlock catches up.
    let kek_opt: Option<[u8; 32]> = {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        *g
    };

    // Snapshot the relevant row state before any mutation.
    let (was_private, hash, current_path, saved_orig_path) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let row: (i64, String, String, Option<String>) = conn
            .query_row(
                "SELECT private, COALESCE(hash, ''), path, original_path
                 FROM photos WHERE id = ?1",
                rusqlite::params![photo_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|e| e.to_string())?;
        (row.0 == 1, row.1, row.2, row.3)
    };

    let new_private = !was_private;
    let thumbs_dir = state.thumbnails_dir.clone();
    let cache_name = if !hash.is_empty() {
        thumbnail::thumb_cache_name(&hash)
    } else {
        String::new()
    };
    let thumb_path = if !cache_name.is_empty() {
        thumbs_dir.join(&cache_name)
    } else {
        std::path::PathBuf::new()
    };

    if new_private {
        // ── Flip INTO private ───────────────────────────────────────
        if let Some(kek) = kek_opt {
            // (a) Encrypt the original file in place → `<path>.rtenc`.
            //     Skip if the path already ends in .rtenc (idempotent —
            //     happens if the previous flip half-succeeded).
            let orig_path_pb = std::path::PathBuf::from(&current_path);
            if !crate::vault_files::is_encrypted_path(&orig_path_pb) {
                crate::vault_files::cleanup_partial(&orig_path_pb);
                if orig_path_pb.is_file() {
                    let enc_path = crate::vault_files::encrypt_in_place(&orig_path_pb, &kek)?;
                    let conn = state.db.lock().map_err(|_| "db lock")?;
                    db::mark_photo_encrypted(
                        &conn,
                        photo_id,
                        &enc_path.to_string_lossy(),
                        &current_path,
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            // (b) Encrypt the on-disk thumbnail.
            if thumb_path.is_file() {
                let bytes = std::fs::read(&thumb_path).map_err(|e| e.to_string())?;
                let sealed = crate::vault_crypto::seal(&kek, &bytes)
                    .map_err(|e| e.to_string())?;
                let conn = state.db.lock().map_err(|_| "db lock")?;
                db::store_encrypted_thumb(&conn, photo_id, &sealed)
                    .map_err(|e| e.to_string())?;
                drop(conn);
                let _ = std::fs::remove_file(&thumb_path);
            }
        }
        // Flip the flag last so a partially-failed encrypt leaves the
        // photo non-private (and recoverable through the regular path).
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::set_photo_private(&conn, photo_id, true).map_err(|e| e.to_string())?;
    } else {
        // ── Flip OUT of private ─────────────────────────────────────
        // (a) Decrypt the original file back to its remembered path.
        if let Some(kek) = kek_opt {
            let cur = std::path::PathBuf::from(&current_path);
            if crate::vault_files::is_encrypted_path(&cur) && cur.is_file() {
                // Where to put it? Prefer the explicitly-saved
                // original_path; fall back to stripping the `.rtenc`
                // suffix if the column is empty (legacy rows).
                let dest = saved_orig_path
                    .clone()
                    .map(std::path::PathBuf::from)
                    .or_else(|| crate::vault_files::original_path_for(&cur))
                    .ok_or_else(|| "cannot determine restore path".to_string())?;
                // If a stray plaintext file exists at the destination
                // (previous half-completed decrypt), keep it: the
                // user's actual data is in the .rtenc, not the stray.
                if dest.exists() {
                    let _ = std::fs::remove_file(&dest);
                }
                crate::vault_files::decrypt_to_file(&cur, &dest, &kek)?;
                let conn = state.db.lock().map_err(|_| "db lock")?;
                db::mark_photo_decrypted(&conn, photo_id, &dest.to_string_lossy())
                    .map_err(|e| e.to_string())?;
            }
        }
        // (b) Decrypt the thumbnail blob back to disk.
        let blob_opt: Option<Vec<u8>> = {
            let conn = state.db.lock().map_err(|_| "db lock")?;
            db::get_encrypted_thumb(&conn, photo_id).map_err(|e| e.to_string())?
        };
        if let (Some(blob), Some(kek)) = (blob_opt, kek_opt) {
            if !thumb_path.as_os_str().is_empty() {
                let plain = crate::vault_crypto::open(&kek, &blob)
                    .map_err(|e| e.to_string())?;
                if let Some(parent) = thumb_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&thumb_path, &plain).map_err(|e| e.to_string())?;
            }
            let conn = state.db.lock().map_err(|_| "db lock")?;
            db::clear_encrypted_thumb(&conn, photo_id).map_err(|e| e.to_string())?;
        }
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::set_photo_private(&conn, photo_id, false).map_err(|e| e.to_string())?;
    }

    Ok(new_private)
}

#[tauri::command]
pub fn vault_has_pin(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    Ok(db::vault_has_pin(&conn))
}

#[tauri::command]
pub fn vault_set_pin(
    pin: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    // v1.5.63 — Faz 1: bumped min length 4 → 6 to match the frontend
    // strength validator. Backend stays the simpler enforcement layer
    // (we don't replicate every common-pattern check here — the FE will
    // refuse weak PINs first), but the floor exists so a malicious
    // direct call to the command still can't store a 1-char PIN.
    if pin.chars().count() < 6 {
        return Err("PIN must be at least 6 characters".into());
    }
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::vault_set_pin(&conn, &pin).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn vault_unlock(
    pin: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    // v1.5.64 — derive the KEK + stash it in AppState so subsequent
    // get_private_thumbnail / toggle_photo_private calls can use it.
    // Returns a JSON object so the FE can distinguish:
    //   { ok:false }                          → wrong PIN
    //   { ok:true, upgraded:false }           → normal unlock
    //   { ok:true, upgraded:true, mnemonic }  → legacy vault upgraded
    //                                            on the fly; show the
    //                                            new recovery phrase to
    //                                            the user, the old one
    //                                            is no longer valid.
    //
    // v1.5.67 — Argon2id is configured for ~250 ms but on slower
    // machines it tops 1 s, which froze the IPC thread when the
    // command was sync. Now we spawn_blocking the Argon2id step so
    // the WebView's invoke()  resolves promptly and the title bar
    // doesn't flip to "(Not Responding)".
    let db_arc = state.db.clone();
    let pin_owned = pin.clone();
    let res: Option<([u8; 32], Option<String>)> = tauri::async_runtime::spawn_blocking(move || {
        let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
        db::vault_unlock_kek(&conn, &pin_owned).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    match res {
        None => Ok(serde_json::json!({"ok": false})),
        Some((mut kek, mut new_phrase)) => {
            // v1.5.68 — KEK schema upgrade. If this vault's KEK was
            // randomly generated (kek_version = 1, the v1.5.64-67
            // model), re-key everything to a deterministic KEK derived
            // from a fresh mnemonic so the vault becomes portable
            // across devices. We do this BEFORE stashing the KEK in
            // AppState because the old KEK is what we need to decrypt
            // the existing blobs.
            let kek_version: u32 = {
                let conn = state.db.lock().map_err(|_| "db lock")?;
                db::vault_kek_version(&conn).map_err(|e| e.to_string())?
            };
            if kek_version < 2 {
                let pin_for_upgrade = pin.clone();
                let db_arc = state.db.clone();
                let old_kek = kek;
                // Run the re-encryption on the blocking pool — it can
                // touch many files. Returns (new_kek, new_phrase) on
                // success or an error string we surface to the FE.
                let upg = tauri::async_runtime::spawn_blocking(move || -> Result<([u8; 32], String), String> {
                    let new_phrase = crate::vault_crypto::generate_recovery_mnemonic()?;
                    let new_kek = crate::vault_crypto::derive_kek_from_mnemonic(&new_phrase)?;

                    // (1) Re-key every .rtenc file. Atomic per file:
                    // decrypt with old → seal with new → write to a
                    // .rtenc.new.tmp → fsync → rename over .rtenc.
                    let files: Vec<(i64, String)> = {
                        let c = db_arc.lock().map_err(|_| "db lock".to_string())?;
                        db::private_photos_with_rtenc(&c).map_err(|e| e.to_string())?
                    };
                    for (_id, path_str) in files.iter() {
                        let p = std::path::PathBuf::from(path_str);
                        if !p.is_file() { continue; }
                        // Decrypt with the OLD KEK we just unlocked with.
                        let plain = crate::vault_files::decrypt_to_bytes(&p, &old_kek)
                            .map_err(|e| format!("decrypt {} (old KEK): {}", p.display(), e))?;
                        // Re-seal under the NEW deterministic KEK.
                        let sealed = crate::vault_crypto::seal(&new_kek, &plain)?;
                        let mut framed = Vec::with_capacity(20 + sealed.len() - 12);
                        framed.extend_from_slice(b"RTNT");
                        framed.push(0x01);
                        framed.extend_from_slice(&[0u8; 3]);
                        framed.extend_from_slice(&sealed);
                        // Atomic replace: write tmp, fsync, rename.
                        let mut tmp = p.as_os_str().to_os_string();
                        tmp.push(".new.tmp");
                        let tmp = std::path::PathBuf::from(tmp);
                        {
                            use std::io::Write;
                            let mut f = std::fs::File::create(&tmp)
                                .map_err(|e| format!("create {}: {}", tmp.display(), e))?;
                            f.write_all(&framed)
                                .map_err(|e| format!("write {}: {}", tmp.display(), e))?;
                            let _ = f.sync_all();
                        }
                        std::fs::rename(&tmp, &p)
                            .map_err(|e| format!("rename {}→{}: {}", tmp.display(), p.display(), e))?;
                    }

                    // (2) Re-key every thumbnail blob. Read with old
                    // KEK, seal with new, UPDATE in DB.
                    let thumb_ids: Vec<i64> = {
                        let c = db_arc.lock().map_err(|_| "db lock".to_string())?;
                        db::private_photos_with_thumb_blob(&c).map_err(|e| e.to_string())?
                    };
                    for id in thumb_ids {
                        let blob: Option<Vec<u8>> = {
                            let c = db_arc.lock().map_err(|_| "db lock".to_string())?;
                            db::get_encrypted_thumb(&c, id).map_err(|e| e.to_string())?
                        };
                        let blob = match blob {
                            Some(b) => b,
                            None => continue,
                        };
                        let plain = crate::vault_crypto::open(&old_kek, &blob)
                            .map_err(|e| format!("decrypt thumb {} (old KEK): {}", id, e))?;
                        let resealed = crate::vault_crypto::seal(&new_kek, &plain)?;
                        let c = db_arc.lock().map_err(|_| "db lock".to_string())?;
                        db::store_encrypted_thumb(&c, id, &resealed).map_err(|e| e.to_string())?;
                    }

                    // (3) Swap the vault row over to the new KEK +
                    // bump kek_version to 2. Atomic UPDATE inside
                    // write_vault_row via vault_complete_upgrade.
                    {
                        let c = db_arc.lock().map_err(|_| "db lock".to_string())?;
                        db::vault_complete_upgrade(&c, &pin_for_upgrade, &new_kek, &new_phrase)
                            .map_err(|e| e.to_string())?;
                    }
                    Ok((new_kek, new_phrase))
                })
                .await
                .map_err(|e| e.to_string())?;

                match upg {
                    Ok((nk, nph)) => {
                        kek = nk;
                        new_phrase = Some(nph);
                    }
                    Err(e) => {
                        // Migration failed mid-flight. Leave kek_version
                        // alone (still 1) so the next unlock retries.
                        // The user can keep using their vault on this
                        // machine with the old KEK.
                        return Err(format!("KEK upgrade failed (vault still works locally): {}", e));
                    }
                }
            }
            {
                let mut g = state.vault_kek.lock().map_err(|_| "kek lock")?;
                *g = Some(kek);
            }
            // Faz 2.1 migration: encrypt anything that's flagged
            // private but isn't yet sealed. Two passes:
            //   (a) thumbnails (v1.5.64 and earlier left these on disk)
            //   (b) ORIGINAL FILES (v1.5.65 and earlier left these in
            //       plaintext at `path` — Explorer could open them)
            //
            // Both run in a single blocking task so the unlock IPC
            // returns immediately. The task emits `vault-migration`
            // events the FE listens for to show progress.
            let db_arc = state.db.clone();
            let thumbs_dir = state.thumbnails_dir.clone();
            let kek_owned = kek;
            let app_handle = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                use tauri::Emitter;
                // ── (a) thumbnails ──────────────────────────────
                let thumb_work: Vec<(i64, String)> = match db_arc.lock() {
                    Ok(c) => db::private_photos_needing_thumb_enc(&c).unwrap_or_default(),
                    Err(_) => Vec::new(),
                };
                for (photo_id, hash) in thumb_work {
                    if hash.is_empty() { continue; }
                    let cache_name = thumbnail::thumb_cache_name(&hash);
                    let thumb_path = thumbs_dir.join(&cache_name);
                    if !thumb_path.is_file() { continue; }
                    let plain = match std::fs::read(&thumb_path) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    let sealed = match crate::vault_crypto::seal(&kek_owned, &plain) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if let Ok(c) = db_arc.lock() {
                        if db::store_encrypted_thumb(&c, photo_id, &sealed).is_ok() {
                            let _ = std::fs::remove_file(&thumb_path);
                        }
                    }
                }
                // ── (b) original files ──────────────────────────
                // File-level migration is NOT auto-run — it's
                // destructive (lose PIN + recovery = lose files), so
                // the FE shows a consent modal first and then calls
                // `vault_run_file_migration` explicitly. We just emit
                // an info event with the pending count so the FE can
                // raise the modal.
                let pending_files: usize = match db_arc.lock() {
                    Ok(c) => db::private_photos_needing_file_enc(&c)
                        .map(|v| v.len())
                        .unwrap_or(0),
                    Err(_) => 0,
                };
                if pending_files > 0 {
                    let _ = app_handle.emit(
                        "vault-migration",
                        serde_json::json!({
                            "phase":"file-pending",
                            "pending": pending_files,
                        }),
                    );
                }
                // Suppress unused-warning under the new flow.
                let _ = &kek_owned;
            });

            match new_phrase {
                Some(p) => Ok(serde_json::json!({
                    "ok": true,
                    "upgraded": true,
                    "mnemonic": p
                })),
                None => Ok(serde_json::json!({
                    "ok": true,
                    "upgraded": false
                })),
            }
        }
    }
}

/// v1.5.64 — Faz 2.1: clear the in-memory KEK. Called from the FE
/// auto-lock timer, the manual Lock button, and on app shutdown via
/// the tray menu. Idempotent.
///
/// v1.5.155 — Also wipes every plaintext temp file we materialised for
/// the video lightbox. Without this an unlock-watch-relock cycle would
/// leave plaintext .mp4 / .mov copies sitting in the temp dir until the
/// OS got around to clearing it, which defeats the vault entirely. We
/// drop the lock before touching the filesystem so a slow `remove_file`
/// doesn't hold off other vault calls.
#[tauri::command]
pub fn vault_lock(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut g = state.vault_kek.lock().map_err(|_| "kek lock")?;
    if let Some(mut k) = g.take() {
        // Best-effort zeroize — Rust's compiler can technically optimize
        // this away without a real `zeroize` crate, but the volatile
        // overwrite is cheap and more careful than nothing.
        for byte in k.iter_mut() { *byte = 0; }
    }
    drop(g);
    let to_wipe: Vec<std::path::PathBuf> = {
        let mut tf = state.vault_temp_files.lock().map_err(|_| "temp lock")?;
        std::mem::take(&mut *tf)
    };
    for p in to_wipe {
        if let Err(e) = crate::vault_files::remove_file_with_fallback(&p) {
            eprintln!("[vault_lock] could not remove temp {}: {}", p.display(), e);
        }
    }
    Ok(())
}

/// True if the vault is currently unlocked AND has a KEK loaded. The FE
/// uses this on cold-start to figure out whether to show the unlock
/// screen vs. the open vault.
#[tauri::command]
pub fn vault_kek_loaded(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
    Ok(g.is_some())
}

/// v1.5.155 — Materialise a vault video so the WebView2 lightbox can
/// play it. WebView2's `convertFileSrc` only points at real files on
/// disk; it can't stream from a `.rtenc` blob, and the IPC base64 path
/// we use for vault images is unworkable for video (a 200 MB clip
/// would blow up the IPC channel and the JS heap).
///
/// We decrypt the row's `.rtenc` to a plaintext file inside
/// `%LOCALAPPDATA%\com.retinatag.app\vault-temp\` and return the path.
/// The file lives only as long as the vault is unlocked: `vault_lock`
/// walks `AppState.vault_temp_files` and shreds every entry. Re-opening
/// the same video re-decrypts — that's intentional, the temp file is
/// cheap to write and we'd rather not race with `vault_lock` running on
/// the lock timer.
///
/// Filename shape: `rt_vault_<photo_id>_<rand>.<orig_ext>`. The random
/// segment keeps two concurrent decrypts from colliding; the extension
/// is preserved so WebView2 / mpv pick the right demuxer.
///
/// Errors: vault locked, row not found, row's `path` isn't a `.rtenc`,
/// I/O failure, or the auth-tag mismatch surfaced by `decrypt_to_file`.
#[tauri::command]
pub async fn vault_decrypt_to_temp(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<String, String> {
    let kek: [u8; 32] = {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        match *g {
            Some(k) => k,
            None => return Err("Vault is locked".into()),
        }
    };

    let enc_path: std::path::PathBuf = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let p: String = conn
            .query_row(
                "SELECT path FROM photos WHERE id = ?1",
                rusqlite::params![photo_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("photo {} not found: {}", photo_id, e))?;
        std::path::PathBuf::from(p)
    };

    if !crate::vault_files::is_encrypted_path(&enc_path) {
        return Err(format!(
            "photo {} is not in the vault (path: {})",
            photo_id,
            enc_path.display()
        ));
    }
    if !enc_path.is_file() {
        return Err(format!(
            "encrypted file missing on disk: {}",
            enc_path.display()
        ));
    }

    // Derive the original extension from `<name>.<ext>.rtenc` so the
    // temp file has the right extension. `original_path_for` strips
    // only the `.rtenc` suffix, leaving `<name>.<ext>`.
    let ext = crate::vault_files::original_path_for(&enc_path)
        .as_ref()
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .unwrap_or("bin")
        .to_string();

    let temp_root = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("app_local_data_dir: {}", e))?
        .join("vault-temp");
    std::fs::create_dir_all(&temp_root)
        .map_err(|e| format!("create vault-temp: {}", e))?;

    // Cheap unique suffix without pulling in `rand`: ns timestamp xor'd
    // with photo_id. Collisions would require two decrypts in the same
    // nanosecond for the same photo, which doesn't happen.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix = format!("{:016x}", (nanos as u64) ^ (photo_id as u64));
    let dest = temp_root.join(format!("rt_vault_{}_{}.{}", photo_id, suffix, ext));

    // `decrypt_to_file` refuses to overwrite an existing dest, so the
    // suffix above guarantees a clean target.
    let dest_clone = dest.clone();
    let enc_clone = enc_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::vault_files::decrypt_to_file(&enc_clone, &dest_clone, &kek)
    })
    .await
    .map_err(|e| e.to_string())??;

    // Track for vault_lock shred. We do this AFTER the file is on disk
    // so a failed decrypt doesn't leave a phantom path in the list.
    {
        let mut tf = state
            .vault_temp_files
            .lock()
            .map_err(|_| "temp lock")?;
        tf.push(dest.clone());
    }

    Ok(dest.to_string_lossy().into_owned())
}

/// v1.5.162 — Folder-vault step 5/5 (decrypt half).
///
/// Temporarily reveal a vault folder to Explorer: recreate the original
/// directory on disk and decrypt every .rtenc photo whose vault_folder_id
/// chains up to this folder back to a plaintext file. The user can then
/// browse / copy / share / edit those files with any Windows program,
/// and call `vault_relock_folder` later to re-seal the contents.
///
/// We do NOT modify the DB rows during reveal — `photos.path` still
/// points at the .rtenc blob, `private` is still 1, the vault still
/// owns the encrypted bytes. The plaintext is a temporary mirror;
/// re-locking just deletes the mirror.
///
/// Walks all descendants of `folder_id` (so a single reveal at root
/// unsealsthe whole tree). Returns the root directory path so the FE
/// can pop Explorer at the right place.
///
/// Errors: vault locked, folder row missing, original_path parent does
/// not exist (user deleted the desktop?), I/O failure mid-decrypt.
#[tauri::command]
pub async fn vault_decrypt_folder_to_explorer(
    folder_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let kek: [u8; 32] = {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        match *g {
            Some(k) => k,
            None => return Err("Vault is locked — unlock it first.".into()),
        }
    };

    let db_arc = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<serde_json::Value, String> {
        // Collect: this folder + every descendant (BFS through parent_id).
        let conn = db_arc.lock().map_err(|_| "db lock")?;
        let root: (String, String) = conn.query_row(
            "SELECT name, original_path FROM vault_folders WHERE id = ?1",
            rusqlite::params![folder_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        ).map_err(|e| format!("folder {} not found: {}", folder_id, e))?;

        let mut all_folder_ids: Vec<i64> = vec![folder_id];
        let mut frontier: Vec<i64> = vec![folder_id];
        while let Some(pid) = frontier.pop() {
            let mut stmt = conn.prepare("SELECT id FROM vault_folders WHERE parent_id = ?1")
                .map_err(|e| e.to_string())?;
            let kids: Vec<i64> = stmt.query_map(rusqlite::params![pid], |r| r.get::<_, i64>(0))
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            for k in &kids {
                all_folder_ids.push(*k);
                frontier.push(*k);
            }
        }

        // For each folder, mkdir original_path + decrypt every photo whose
        // vault_folder_id matches it. We rely on photos.path being the
        // .rtenc location, and we derive the plaintext destination from
        // photos.original_path (set at vault_add_paths time). That makes
        // the decrypt fully content-addressed: even if the user moved
        // their library, we still rebuild at the *original* location.
        let mut decrypted = 0usize;
        let mut errors: Vec<String> = Vec::new();
        for fid in &all_folder_ids {
            let f_orig_path: String = conn.query_row(
                "SELECT original_path FROM vault_folders WHERE id = ?1",
                rusqlite::params![fid],
                |r| r.get(0),
            ).map_err(|e| e.to_string())?;
            if let Err(e) = std::fs::create_dir_all(&f_orig_path) {
                errors.push(format!("mkdir {}: {}", f_orig_path, e));
                continue;
            }

            let mut stmt = conn.prepare(
                "SELECT path, original_path FROM photos
                  WHERE vault_folder_id = ?1 AND private = 1"
            ).map_err(|e| e.to_string())?;
            let rows: Vec<(String, Option<String>)> = stmt
                .query_map(rusqlite::params![fid], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            for (enc_path, original_path) in rows {
                let enc_pb = std::path::PathBuf::from(&enc_path);
                if !crate::vault_files::is_encrypted_path(&enc_pb) {
                    errors.push(format!("not a .rtenc: {}", enc_path));
                    continue;
                }
                let dest_pb = match original_path
                    .map(std::path::PathBuf::from)
                    .or_else(|| crate::vault_files::original_path_for(&enc_pb))
                {
                    Some(p) => p,
                    None => {
                        errors.push(format!("can't derive plaintext path for {}", enc_path));
                        continue;
                    }
                };
                // If a stale plaintext from a previous reveal is sitting
                // there, skip — overwriting silently would lose user
                // edits. The FE shows this as "already revealed".
                if dest_pb.exists() { continue; }
                if let Err(e) = crate::vault_files::decrypt_to_file(&enc_pb, &dest_pb, &kek) {
                    errors.push(format!("decrypt {}: {}", enc_path, e));
                    continue;
                }
                decrypted += 1;
            }
        }

        Ok(serde_json::json!({
            "root_path": &root.1,
            "root_name": &root.0,
            "decrypted": decrypted,
            "errors": errors,
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// v1.5.162 — Folder-vault step 5/5 (re-seal half).
///
/// Reverse of `vault_decrypt_folder_to_explorer`. Walks the same folder
/// tree and removes the plaintext mirror — the .rtenc blobs stay
/// untouched, so the vault content is unchanged. After this call,
/// Explorer can no longer open any of those files.
///
/// We deliberately do NOT re-encrypt edited content: that would race
/// against any open file handle (Word doc still in use, mpv playing
/// a video), and the user expectation here is "I'm done looking,
/// hide it again", not "I edited these and want the edits preserved".
/// If we ever want to support edits, that's a separate "commit edits
/// to vault" verb.
///
/// Errors: vault locked, folder row missing, I/O failure on remove.
/// Best-effort throughout — a single file with a permission denied
/// doesn't abort the whole sweep.
#[tauri::command]
pub async fn vault_relock_folder(
    folder_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        if g.is_none() {
            return Err("Vault is locked — unlock it first.".into());
        }
    }

    let db_arc = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let conn = db_arc.lock().map_err(|_| "db lock")?;

        // BFS the tree just like the decrypt verb so we cover all
        // descendants in one call.
        let mut all_folder_ids: Vec<i64> = vec![folder_id];
        let mut frontier: Vec<i64> = vec![folder_id];
        while let Some(pid) = frontier.pop() {
            let mut stmt = conn.prepare("SELECT id FROM vault_folders WHERE parent_id = ?1")
                .map_err(|e| e.to_string())?;
            let kids: Vec<i64> = stmt.query_map(rusqlite::params![pid], |r| r.get::<_, i64>(0))
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            for k in &kids {
                all_folder_ids.push(*k);
                frontier.push(*k);
            }
        }

        let mut removed_files = 0usize;
        let mut removed_dirs = 0usize;
        let mut errors: Vec<String> = Vec::new();

        // Phase A — delete every plaintext mirror file.
        for fid in &all_folder_ids {
            let mut stmt = conn.prepare(
                "SELECT path, original_path FROM photos
                  WHERE vault_folder_id = ?1 AND private = 1"
            ).map_err(|e| e.to_string())?;
            let rows: Vec<(String, Option<String>)> = stmt
                .query_map(rusqlite::params![fid], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            for (enc_path, original_path) in rows {
                let enc_pb = std::path::PathBuf::from(&enc_path);
                let dest_pb = match original_path
                    .map(std::path::PathBuf::from)
                    .or_else(|| crate::vault_files::original_path_for(&enc_pb))
                {
                    Some(p) => p,
                    None => continue,
                };
                if !dest_pb.exists() { continue; }
                if let Err(e) = crate::vault_files::remove_file_with_fallback(&dest_pb) {
                    errors.push(format!("remove plaintext {}: {}", dest_pb.display(), e));
                } else {
                    removed_files += 1;
                }
            }
        }

        // Phase B — remove empty mirror directories deepest-first.
        // Collect each folder's original_path, walk it, rm any dir that's empty.
        // We're looking at the directories the user would see in Explorer;
        // if anything other than our just-removed plaintext sits there
        // (user dropped a new note, took a screenshot during reveal),
        // remove_dir returns ENOTEMPTY and we leave it alone.
        let mut dirs: Vec<std::path::PathBuf> = Vec::new();
        for fid in &all_folder_ids {
            let p: String = conn.query_row(
                "SELECT original_path FROM vault_folders WHERE id = ?1",
                rusqlite::params![fid],
                |r| r.get(0),
            ).map_err(|e| e.to_string())?;
            dirs.push(std::path::PathBuf::from(p));
        }
        dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        for d in dirs {
            if !d.exists() { continue; }
            if let Err(e) = std::fs::remove_dir(&d) {
                let msg = e.to_string();
                if !msg.contains("not empty") && !msg.contains("non-empty") {
                    errors.push(format!("rmdir {}: {}", d.display(), msg));
                }
            } else {
                removed_dirs += 1;
            }
        }

        Ok(serde_json::json!({
            "removed_files": removed_files,
            "removed_dirs": removed_dirs,
            "errors": errors,
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// v1.5.64 — Faz 2.3: True if Windows Hello is set up AND the vault
/// has a stored bio_blob the FE can attempt to use. The FE shows the
/// "Use Windows Hello" button only when both are true.
///
/// v1.5.67 — async + spawn_blocking. UserConsentVerifier::CheckAvailabilityAsync
/// is a WinRT call we then `.get()` on. On thread without WinRT init
/// the wait could hang the IPC thread; running it on the blocking
/// pool isolates that.
#[tauri::command]
pub async fn vault_biometric_status(
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let available = tauri::async_runtime::spawn_blocking(|| {
        crate::vault_biometric::is_available()
    })
    .await
    .map_err(|e| e.to_string())?;
    let enrolled = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::vault_get_bio_blob(&conn).map_err(|e| e.to_string())?.is_some()
    };
    Ok(serde_json::json!({
        "available": available,
        "enrolled": enrolled,
    }))
}

/// v1.5.64 — Faz 2.3: enrolment. Requires the vault to already be
/// unlocked (KEK in memory). We DPAPI-wrap the KEK and store the blob,
/// after a Hello consent prompt — that pairs the bio_blob with a
/// successful biometric verification at enrolment time.
///
/// v1.5.67 — async. The Hello prompt blocks until the user verifies,
/// cancels, or times out (can be 30+ s); doing this synchronously
/// would obviously freeze the WebView.
#[tauri::command]
pub async fn vault_biometric_enroll(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let avail = tauri::async_runtime::spawn_blocking(|| {
        crate::vault_biometric::is_available()
    })
    .await
    .map_err(|e| e.to_string())?;
    if !avail {
        return Err("Windows Hello is not available on this device".into());
    }
    let kek: [u8; 32] = {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        match *g {
            Some(k) => k,
            None => return Err("Unlock the vault with your PIN first".into()),
        }
    };
    let consent = tauri::async_runtime::spawn_blocking(|| {
        crate::vault_biometric::request_consent(
            "Enable Windows Hello unlock for RetinaTag Vault?"
        )
    })
    .await
    .map_err(|e| e.to_string())??;
    if !consent {
        return Ok(false);
    }
    let blob = tauri::async_runtime::spawn_blocking(move || {
        crate::vault_biometric::dpapi_protect(&kek)
    })
    .await
    .map_err(|e| e.to_string())??;
    {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::vault_set_bio_blob(&conn, Some(&blob)).map_err(|e| e.to_string())?;
    }
    Ok(true)
}

/// v1.5.64 — Faz 2.3: drop the stored bio_blob. Future unlocks fall
/// back to PIN until the user re-enrols. Idempotent.
#[tauri::command]
pub fn vault_biometric_disable(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::vault_set_bio_blob(&conn, None).map_err(|e| e.to_string())
}

/// v1.5.64 — Faz 2.3: biometric unlock. Hello prompt → DPAPI unwrap →
/// stash KEK in AppState, exactly like a successful PIN unlock.
///
/// v1.5.67 — async, same reason as enroll.
#[tauri::command]
pub async fn vault_biometric_unlock(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let avail = tauri::async_runtime::spawn_blocking(|| {
        crate::vault_biometric::is_available()
    })
    .await
    .map_err(|e| e.to_string())?;
    if !avail {
        return Err("Windows Hello is not available on this device".into());
    }
    let blob: Vec<u8> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        match db::vault_get_bio_blob(&conn).map_err(|e| e.to_string())? {
            Some(b) => b,
            None => return Err("Biometric unlock is not enabled — set it up after a PIN unlock".into()),
        }
    };
    let consent = tauri::async_runtime::spawn_blocking(|| {
        crate::vault_biometric::request_consent("Unlock the RetinaTag Vault")
    })
    .await
    .map_err(|e| e.to_string())??;
    if !consent {
        return Ok(false);
    }
    let kek_bytes = tauri::async_runtime::spawn_blocking(move || {
        crate::vault_biometric::dpapi_unprotect(&blob)
    })
    .await
    .map_err(|e| e.to_string())??;
    if kek_bytes.len() != 32 {
        return Err("Vault: corrupt biometric blob — re-enrol".into());
    }
    let mut kek = [0u8; 32];
    kek.copy_from_slice(&kek_bytes);
    {
        let mut g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        *g = Some(kek);
    }
    Ok(true)
}

/// v1.5.66 — Faz 2.1 file-level: count of private photos whose original
/// file isn't encrypted yet. The FE polls this on unlock to decide
/// whether to surface the migration consent modal.
#[tauri::command]
pub fn vault_pending_file_migration_count(
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::private_photos_needing_file_enc(&conn)
        .map(|v| v.len())
        .map_err(|e| e.to_string())
}

/// v1.5.66 — Faz 2.1 file-level: run the file-encryption migration.
/// Called only after the user has confirmed the consent modal. Emits
/// `vault-migration` events with `phase` ∈ {"start","progress","done","error"}
/// so the FE can render a progress bar.
#[tauri::command]
pub async fn vault_run_file_migration(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<usize, String> {
    let kek = {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        match *g {
            Some(k) => k,
            None => return Err("Vault is locked".into()),
        }
    };
    let work: Vec<(i64, String)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::private_photos_needing_file_enc(&conn).map_err(|e| e.to_string())?
    };
    if work.is_empty() {
        return Ok(0);
    }
    let total = work.len();
    let db_arc = state.db.clone();
    let app_handle = app.clone();
    let n = tauri::async_runtime::spawn_blocking(move || -> usize {
        use tauri::Emitter;
        let _ = app_handle.emit(
            "vault-migration",
            serde_json::json!({"phase":"start","total":total}),
        );
        let mut done = 0usize;
        for (photo_id, path_str) in work.iter() {
            let p = std::path::PathBuf::from(path_str);
            if !p.is_file() {
                continue;
            }
            crate::vault_files::cleanup_partial(&p);
            let enc_path = match crate::vault_files::encrypt_in_place(&p, &kek) {
                Ok(ep) => ep,
                Err(e) => {
                    let _ = app_handle.emit(
                        "vault-migration",
                        serde_json::json!({
                            "phase":"error",
                            "photo_id": *photo_id,
                            "message": e,
                        }),
                    );
                    continue;
                }
            };
            // v1.5.154 — Atomicity: commit the DB UPDATE FIRST, then
            // delete the original. If the DB step fails we roll back
            // by removing the freshly-written .rtenc so the original
            // stays as the ground truth (rather than the previous
            // behaviour where encrypt_in_place silently deleted the
            // original before any DB work).
            let db_ok = if let Ok(c) = db_arc.lock() {
                match db::mark_photo_encrypted(
                    &c,
                    *photo_id,
                    &enc_path.to_string_lossy(),
                    path_str,
                ) {
                    Ok(()) => true,
                    Err(db_err) => {
                        eprintln!("vault_migrate mark_photo_encrypted({}): {}", photo_id, db_err);
                        false
                    }
                }
            } else {
                eprintln!("vault_migrate: db lock poisoned for photo {}", photo_id);
                false
            };
            if db_ok {
                if let Err(e) = crate::vault_files::remove_file_with_fallback(&p) {
                    // Rare on Windows but possible if another app
                    // (Explorer preview, Adobe Bridge) holds a handle.
                    // Original-still-on-disk is a leaked plaintext —
                    // the .rtenc + DB row are already correct, so the
                    // photo IS in the vault. Just surface a warning.
                    let _ = app_handle.emit(
                        "vault-migration",
                        serde_json::json!({
                            "phase":"error",
                            "photo_id": *photo_id,
                            "message": format!("Encrypted OK but couldn't remove original: {}", e),
                        }),
                    );
                }
            } else {
                // Rollback: drop the .rtenc, original survives.
                if let Err(e) = crate::vault_files::remove_file_with_fallback(&enc_path) {
                    eprintln!("vault_migrate rollback failed to remove rtenc: {}", e);
                }
                let _ = app_handle.emit(
                    "vault-migration",
                    serde_json::json!({
                        "phase":"error",
                        "photo_id": *photo_id,
                        "message": "DB commit failed — kept original, removed rtenc",
                    }),
                );
                continue;
            }
            done += 1;
            let _ = app_handle.emit(
                "vault-migration",
                serde_json::json!({
                    "phase":"progress",
                    "done": done,
                    "total": total,
                }),
            );
        }
        let _ = app_handle.emit(
            "vault-migration",
            serde_json::json!({"phase":"done","done":done,"total":total}),
        );
        done
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(n)
}

/// v1.5.67 — async replacement of the previous sync command. File-level
/// AES seal/open on big RAW or HEIC files used to freeze the IPC thread.
/// Wrapping the heavy work in spawn_blocking lets the IPC ack the FE
/// immediately and the UI stays responsive while the file is encrypted.
#[tauri::command]
pub async fn toggle_photo_private(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<bool, String> {
    // v1.5.73 — Was a P0 leak: when the vault was locked (kek_opt = None)
    // the encryption block was skipped but db::set_photo_private(true) ran
    // anyway, leaving the original file readable on disk while the UI
    // marked it private. Now we reject up-front when locked so the user
    // sees the failure and unlocks first.
    let kek_opt: Option<[u8; 32]> = {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        *g
    };
    let kek = kek_opt
        .ok_or_else(|| "Vault is locked — unlock it first to change a photo's private state.".to_string())?;
    let db_arc = state.db.clone();
    let thumbs_dir = state.thumbnails_dir.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<bool, String> {
        // Read the current private flag so we can return the new one.
        let was_private: bool = {
            let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
            conn.query_row(
                "SELECT private FROM photos WHERE id = ?1",
                rusqlite::params![photo_id],
                |r| Ok(r.get::<_, i64>(0)? == 1),
            )
            .map_err(|e| e.to_string())?
        };
        let new_private = !was_private;
        move_photo_private(&db_arc, photo_id, new_private, &kek, &thumbs_dir)?;
        Ok(new_private)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// v1.5.66 — Faz 2.1: decrypt a private photo's full original file
/// and return it as a base64 data URL the FE can plug into an `<img>`.
/// Used by the vault lightbox. Refuses if:
///   - vault is locked (no KEK in memory)
///   - the file at `photos.path` doesn't end in `.rtenc` (i.e. it
///     hasn't been migrated yet — fall back to the regular asset URL)
///   - decryption fails (wrong KEK / corrupt blob / missing file)
///
/// MIME type sniffing is best-effort from the original extension we
/// stored in `photos.filename`. JPG/PNG/WEBP/HEIC are surfaced
/// correctly; anything else gets `application/octet-stream` (which
/// `<img>` will refuse to render — but that's fine, the lightbox
/// already restricts to image media types).
#[tauri::command]
pub async fn get_private_photo_data(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let kek = {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        match *g {
            Some(k) => k,
            None => return Err("Vault is locked".into()),
        }
    };
    let (path_str, filename): (String, String) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        conn.query_row(
            "SELECT path, COALESCE(filename, '') FROM photos WHERE id = ?1",
            rusqlite::params![photo_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| e.to_string())?
    };
    let path = std::path::PathBuf::from(&path_str);
    if !crate::vault_files::is_encrypted_path(&path) {
        return Err(
            "photo is not yet migrated to encrypted storage — try locking and re-unlocking the vault"
                .into(),
        );
    }
    // v1.5.67 — heavy bits (whole-file decrypt + base64) on the
    // blocking pool. A 30 MB JPEG turns into a ~40 MB data URL after
    // base64 — encoding that on the IPC thread froze the WebView.
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let plain = crate::vault_files::decrypt_to_bytes(&path, &kek)?;
        let mime = match std::path::Path::new(&filename)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
        {
            Some(ref e) if e == "jpg" || e == "jpeg" => "image/jpeg",
            Some(ref e) if e == "png"  => "image/png",
            Some(ref e) if e == "webp" => "image/webp",
            Some(ref e) if e == "heic" || e == "heif" => "image/heic",
            Some(ref e) if e == "gif"  => "image/gif",
            Some(ref e) if e == "bmp"  => "image/bmp",
            _ => "application/octet-stream",
        };
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        Ok(format!("data:{};base64,{}", mime, STANDARD.encode(&plain)))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// v1.5.64 — Faz 2.1: decrypt a private photo's thumbnail blob and
/// return it as a `data:image/jpeg;base64,...` URL. Errors if the
/// vault is locked (no KEK) or the photo isn't private.
#[tauri::command]
pub fn get_private_thumbnail(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let kek = {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        match *g {
            Some(k) => k,
            None => return Err("Vault is locked".into()),
        }
    };
    let blob_opt: Option<Vec<u8>> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_encrypted_thumb(&conn, photo_id).map_err(|e| e.to_string())?
    };
    let blob = match blob_opt {
        Some(b) => b,
        None => return Err("No encrypted thumbnail".into()),
    };
    let plain = crate::vault_crypto::open(&kek, &blob).map_err(|e| e.to_string())?;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    Ok(format!("data:image/jpeg;base64,{}", STANDARD.encode(&plain)))
}

/// Drop the stored PIN. Used by the BIP39 recovery flow (Faz 2) and by
/// the "wipe vault" UI after 10 wrong PIN attempts. Photos keep their
/// `private` flag — see `vault_reset_full` for the destructive variant.
#[tauri::command]
pub fn vault_clear_pin(
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::vault_clear_pin(&conn).map_err(|e| e.to_string())
}

/// v1.5.149 — Drag-drop folders/files straight into the vault. Mac
/// shipped the matching `vault_add_paths` command in v1.5.142; this
/// port is the Windows-side equivalent so a user who has both
/// machines on the shared SMB volume sees the same dropzone UX on
/// either side.
///
/// Walks each input path:
///   - File: process directly if it's a media file (not already .rtenc)
///   - Folder: WalkDir max_depth 20, same filter
///
/// For each candidate:
///   1. Hash. Already-vaulted (private=1) by hash → already_in_vault++.
///   2. Insert/upsert the DB row with the original metadata.
///   3. encrypt_in_place — atomic write to `.rtenc`, remove plaintext.
///   4. mark_photo_encrypted (DB path → .rtenc, original_path stashed)
///      + set_photo_private(true).
///
/// Emits `vault-add-progress` events:
///   - { phase: "scanning",   total }                              (once)
///   - { phase: "encrypting", done, total }                        (every 5)
///   - { phase: "done", total, encrypted, already_in_vault, … }    (once)
#[tauri::command]
pub async fn vault_add_paths(
    paths: Vec<String>,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    // Reject up-front when locked — same guard pattern as
    // toggle_photo_private (v1.5.73 P0 leak fix).
    let kek: [u8; 32] = {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        match *g {
            Some(k) => k,
            None => return Err("Vault is locked — unlock it first before adding paths.".into()),
        }
    };
    let db_arc = state.db.clone();
    let ah = app_handle.clone();

    let result: serde_json::Value = tauri::async_runtime::spawn_blocking(move || -> serde_json::Value {
        use walkdir::WalkDir;
        use tauri::Emitter;

        // Phase 1 — scan input roots to a flat candidate list.
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        for p_str in &paths {
            let p = std::path::Path::new(p_str);
            if !p.exists() { continue; }
            if p.is_file() {
                if crate::scanner::is_media_file(p)
                    && !crate::vault_files::is_encrypted_path(p)
                {
                    candidates.push(p.to_path_buf());
                }
            } else if p.is_dir() {
                for entry in WalkDir::new(p)
                    .max_depth(20)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    let ep = entry.path();
                    if ep.is_file()
                        && crate::scanner::is_media_file(ep)
                        && !crate::vault_files::is_encrypted_path(ep)
                    {
                        candidates.push(ep.to_path_buf());
                    }
                }
            }
        }
        let _ = ah.emit("vault-add-progress", serde_json::json!({
            "phase": "scanning",
            "total": candidates.len(),
        }));

        // Counters used by Phase 1.5 (folder inserts) and Phase 2 (encrypt).
        // Declared up here so the folder-insert pass can record errors
        // without forward-declaration gymnastics.
        let mut encrypted = 0usize;
        let mut already_in_vault = 0usize;
        let mut skipped = 0usize;
        let mut errors: Vec<String> = Vec::new();

        // Phase 1.5 — v1.5.159 folder-vault step 3/5. For every directory
        // root the user dropped, insert one `vault_folders` row per
        // directory under (and including) the root. Build a map
        // path → row_id so Phase 2 can attach each encrypted photo to
        // its leaf folder via `photos.vault_folder_id`.
        //
        // We sort dirs shallow-first so a parent's row exists before
        // its children try to look up parent_id. Each drop session is
        // a fresh tree: re-dropping the same folder makes a NEW row set
        // and gets a new id — that's intentional (the old set is what
        // step 4/5 "decrypt to Explorer" walks back).
        //
        // Standalone-file drops produce no folder rows. Those photos
        // keep `vault_folder_id = NULL` and show flat in the vault UI,
        // exactly like pre-v1.5.158 behaviour.
        let mut folder_map: std::collections::HashMap<std::path::PathBuf, i64> =
            std::collections::HashMap::new();
        for p_str in &paths {
            let root = std::path::Path::new(p_str);
            if !root.is_dir() { continue; }
            // Collect every dir from this root, shallow-first.
            let mut dirs: Vec<std::path::PathBuf> = WalkDir::new(root)
                .max_depth(20)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_dir())
                .map(|e| e.into_path())
                .collect();
            dirs.sort_by_key(|p| p.components().count());

            // Insert each dir as a vault_folders row. Done in one txn
            // per root so a mid-walk DB hiccup rolls back the whole
            // tree of this drop — cleaner than half-built hierarchies.
            let now = chrono::Utc::now().to_rfc3339();
            let conn_res = db_arc.lock();
            let conn_ok = match conn_res {
                Ok(mut conn) => match conn.transaction() {
                    Ok(tx) => {
                        let mut ok = true;
                        for d in &dirs {
                            let name = d.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("")
                                .to_string();
                            let original_path = d.to_string_lossy().to_string();
                            let parent_id: Option<i64> = if d == root {
                                None
                            } else {
                                d.parent().and_then(|pp| folder_map.get(pp).copied())
                            };
                            let ins = tx.execute(
                                "INSERT INTO vault_folders (name, parent_id, original_path, created_at)
                                 VALUES (?1, ?2, ?3, ?4)",
                                rusqlite::params![&name, parent_id, &original_path, &now],
                            );
                            match ins {
                                Ok(_) => {
                                    folder_map.insert(d.clone(), tx.last_insert_rowid());
                                }
                                Err(e) => {
                                    errors.push(format!(
                                        "vault_folders insert {}: {}", d.display(), e
                                    ));
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if ok {
                            match tx.commit() {
                                Ok(()) => true,
                                Err(e) => {
                                    errors.push(format!(
                                        "vault_folders commit for {}: {}", root.display(), e
                                    ));
                                    false
                                }
                            }
                        } else {
                            // tx drops with rollback — clear the partial folder_map
                            // entries we may have inserted for this root so they don't
                            // point at rows that got rolled back.
                            for d in &dirs { folder_map.remove(d); }
                            false
                        }
                    }
                    Err(e) => {
                        errors.push(format!("vault_folders begin tx for {}: {}", root.display(), e));
                        false
                    }
                },
                Err(_) => {
                    errors.push(format!("db lock for vault_folders {}", root.display()));
                    false
                }
            };
            // If folder insertion failed, encryption still proceeds —
            // photos just won't get vault_folder_id set. Better than
            // aborting the whole drop.
            let _ = conn_ok;
        }

        // Phase 2 — encrypt each candidate.
        let total = candidates.len();

        for (i, path) in candidates.iter().enumerate() {
            let done = i + 1;
            // Throttled progress: every 5 files + at completion. Keeps
            // the IPC channel calm even on a 5,000-file vault import.
            if done % 5 == 0 || done == total {
                let _ = ah.emit("vault-add-progress", serde_json::json!({
                    "phase": "encrypting",
                    "done": done,
                    "total": total,
                }));
            }

            // 1) Hash. Failure here usually means the share went away
            //    mid-walk; we record and continue with the next file.
            let hash = match crate::scanner::compute_hash(&path.to_string_lossy()) {
                Ok(h) => h,
                Err(e) => {
                    errors.push(format!("hash {}: {}", path.display(), e));
                    continue;
                }
            };

            // 2) Hash-match lookup. If this file is already in the
            //    library and already private, we treat it as a no-op
            //    (drag-drop is idempotent — drop the same folder twice
            //    safely).
            let existing: Option<(i64, i32)> = {
                let conn = match db_arc.lock() {
                    Ok(c) => c,
                    Err(_) => { errors.push("db lock".into()); continue; }
                };
                conn.query_row(
                    "SELECT id, private FROM photos WHERE hash = ?1 LIMIT 1",
                    rusqlite::params![&hash],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i32>(1)?)),
                ).ok()
            };
            if let Some((_id, is_private)) = existing {
                if is_private == 1 {
                    already_in_vault += 1;
                    skipped += 1;
                    continue;
                }
            }

            // 3) Collect metadata for the (possibly new) DB row.
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").to_string();
            let folder = path.parent().and_then(|p| p.to_str()).unwrap_or("").to_string();
            let size = std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
            let media_type = crate::scanner::media_type_for_path(path).to_string();
            let (width, height) = if media_type == "image" {
                image::image_dimensions(path)
                    .ok()
                    .map(|(w, h)| (Some(w as i32), Some(h as i32)))
                    .unwrap_or((None, None))
            } else {
                (None, None)
            };

            let original_path = path.to_string_lossy().to_string();

            // v1.5.154 — Atomicity pass (matches Mac's v1.5.159):
            //   1. Encrypt to .rtenc (no source delete yet).
            //   2. Run insert + UPDATE path/private inside a single
            //      SQLite transaction. If ANY step fails, rollback
            //      and remove the .rtenc — original stays put.
            //   3. Only after the transaction commits do we remove
            //      the plaintext source.
            // Pre-1.5.154 the source was deleted by encrypt_in_place
            // and the two UPDATEs ran as `let _ =` — a silent commit
            // failure left orphan .rtenc files with no DB row, and
            // the photo was effectively lost.
            let enc_path = match crate::vault_files::encrypt_in_place(path, &kek) {
                Ok(ep) => ep,
                Err(e) => {
                    errors.push(format!("encrypt {}: {}", path.display(), e));
                    continue;
                }
            };

            let enc_path_str = enc_path.to_string_lossy().to_string();
            let db_ok = match db_arc.lock() {
                Err(_) => {
                    errors.push(format!("db lock for {}", path.display()));
                    false
                }
                Ok(mut conn) => match conn.transaction() {
                    Err(e) => {
                        errors.push(format!("begin tx {}: {}", path.display(), e));
                        false
                    }
                    Ok(tx) => {
                        // Insert (or fetch existing id on UNIQUE conflict).
                        let now = chrono::Utc::now().to_rfc3339();
                        let inserted = tx.execute(
                            "INSERT OR IGNORE INTO photos
                                (path, filename, folder, hash, size, width, height,
                                 created_at, status, media_type, date_taken, duration_secs)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9, NULL, NULL)",
                            rusqlite::params![
                                &original_path, &filename, &folder, &hash, size,
                                width, height, now, media_type,
                            ],
                        );
                        let photo_id_res: Result<i64, rusqlite::Error> = match inserted {
                            Ok(rows) if rows > 0 => Ok(tx.last_insert_rowid()),
                            Ok(_) => tx.query_row(
                                "SELECT id FROM photos WHERE path = ?1",
                                rusqlite::params![&original_path],
                                |r| r.get(0),
                            ),
                            Err(e) => Err(e),
                        };
                        match photo_id_res {
                            Err(e) => {
                                errors.push(format!("insert {}: {}", path.display(), e));
                                false
                            }
                            Ok(pid) => {
                                let upd_path = tx.execute(
                                    "UPDATE photos SET path = ?1, original_path = ?2 WHERE id = ?3",
                                    rusqlite::params![&enc_path_str, &original_path, pid],
                                );
                                let upd_priv = tx.execute(
                                    "UPDATE photos SET private = 1 WHERE id = ?1",
                                    rusqlite::params![pid],
                                );
                                // v1.5.159 — Link the photo to its leaf folder
                                // row inserted in Phase 1.5. NULL for standalone
                                // file drops (folder_map has no entry for the
                                // file's parent, lookup returns None).
                                let vault_folder_id_opt: Option<i64> = path.parent()
                                    .and_then(|pp| folder_map.get(pp).copied());
                                let upd_folder = tx.execute(
                                    "UPDATE photos SET vault_folder_id = ?1 WHERE id = ?2",
                                    rusqlite::params![vault_folder_id_opt, pid],
                                );
                                match (upd_path, upd_priv, upd_folder) {
                                    (Ok(_), Ok(_), Ok(_)) => match tx.commit() {
                                        Ok(()) => true,
                                        Err(e) => {
                                            errors.push(format!("commit {}: {}", path.display(), e));
                                            false
                                        }
                                    },
                                    (path_res, priv_res, folder_res) => {
                                        // tx drops with rollback automatically.
                                        errors.push(format!(
                                            "update {}: path={:?} private={:?} folder={:?}",
                                            path.display(),
                                            path_res.err(),
                                            priv_res.err(),
                                            folder_res.err()
                                        ));
                                        false
                                    }
                                }
                            }
                        }
                    }
                }
            };

            if db_ok {
                // Commit on-disk: remove the plaintext source. If this
                // fails the photo is still safe (in vault + DB row OK),
                // just leaks a plaintext copy — flag in errors.
                if let Err(e) = crate::vault_files::remove_file_with_fallback(path) {
                    errors.push(format!("encrypted but plaintext not removed for {}: {}", path.display(), e));
                }
                encrypted += 1;
            } else {
                // Rollback: drop the .rtenc. Original is still on disk.
                if let Err(e) = crate::vault_files::remove_file_with_fallback(&enc_path) {
                    errors.push(format!("rollback failed to remove rtenc for {}: {}", path.display(), e));
                }
                // Already pushed a per-step error above.
            }
        }

        // v1.5.157 — Now that every media file inside the dropped folders
        // is sealed as .rtenc and the plaintext copies are gone, the
        // folders are usually empty shells sitting on the user's desktop.
        // The user's complaint: "I encrypted my Vault folder but it's
        // still on the desktop. The whole folder should disappear."
        //
        // We walk each dropped input that's a directory, deepest-first
        // (so child dirs go before parents), and remove anything that
        // has become empty. We DO NOT recursively force-delete:
        //   * Files left behind (non-media, or .rtenc we just wrote — but
        //     we never write .rtenc back into the dropped folder, only
        //     next to the original which is gone now) → the directory
        //     stays.
        //   * remove_dir on a non-empty directory returns an error and
        //     we just record it and move on. So extra content is always
        //     safe.
        //
        // Result: a folder that contained ONLY media files becomes a
        // ghost. A folder that mixed media + a Readme.txt loses the
        // media but keeps the Readme + the folder. That matches user
        // intent — the readme isn't private, no surprise deletions.
        let mut folders_removed = 0usize;
        for p_str in &paths {
            let root = std::path::Path::new(p_str);
            if !root.is_dir() { continue; }
            // Collect every dir under (and including) root, deepest first.
            let mut dirs: Vec<std::path::PathBuf> = walkdir::WalkDir::new(root)
                .max_depth(20)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_dir())
                .map(|e| e.into_path())
                .collect();
            // Sort by depth desc so children are tried before parents.
            dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
            for d in dirs {
                // remove_dir only succeeds on an empty directory — exactly
                // the semantics we want.
                if let Err(e) = std::fs::remove_dir(&d) {
                    // ENOTEMPTY / "directory not empty" is the common,
                    // expected case when the user had other files there.
                    // We only log other errors so a permissions issue
                    // surfaces in the result.
                    let msg = e.to_string();
                    if !msg.contains("not empty") && !msg.contains("non-empty") {
                        errors.push(format!("remove_dir {}: {}", d.display(), msg));
                    }
                } else {
                    folders_removed += 1;
                }
            }
        }

        let _ = ah.emit("vault-add-progress", serde_json::json!({
            "phase": "done",
            "total": total,
            "encrypted": encrypted,
            "already_in_vault": already_in_vault,
            "skipped": skipped,
            "folders_removed": folders_removed,
            "folders_created": folder_map.len(),
            "errors": errors.len(),
        }));

        serde_json::json!({
            "total": total,
            "encrypted": encrypted,
            "already_in_vault": already_in_vault,
            "skipped": skipped,
            "folders_removed": folders_removed,
            "folders_created": folder_map.len(),
            "errors": errors,
        })
    })
    .await
    .map_err(|e| e.to_string())?;

    Ok(result)
}

/// v1.5.63 — Faz 2: PIN setup that ALSO generates a 24-word BIP39
/// recovery phrase. Returns the phrase exactly once. Caller MUST
/// display it to the user and warn them this is the only time it's
/// shown — the DB stores only an AES-GCM ciphertext that proves the
/// phrase is correct, not the phrase itself.
#[tauri::command]
pub async fn vault_set_pin_with_recovery(
    pin: String,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    if pin.chars().count() < 6 {
        return Err("PIN must be at least 6 characters".into());
    }
    // v1.5.67 — async + spawn_blocking. PIN setup runs Argon2id TWICE
    // (PIN-KEK + RKEK derivation) plus a SHA hash and AES seal — call
    // it ~600 ms on a fast laptop, ~2 s on a slow one. Sync would
    // freeze the IPC thread for the full duration.
    let db_arc = state.db.clone();
    let phrase_kek = tauri::async_runtime::spawn_blocking(move || {
        let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
        db::vault_set_pin_with_recovery(&conn, &pin).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let (phrase, kek) = phrase_kek;
    {
        let mut g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        *g = Some(kek);
    }
    Ok(phrase)
}

/// v1.5.63 — Faz 2: validate a typed BIP39 phrase against the vault's
/// recovery_blob. Returns true iff the phrase decrypts the stored
/// ciphertext. The FE uses this to gate "set a new PIN" in the
/// recovery flow without allowing arbitrary phrases to wipe the vault.
#[tauri::command]
pub fn vault_verify_mnemonic(
    phrase: String,
    state: tauri::State<'_, AppState>,
) -> Result<bool, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::vault_verify_mnemonic(&conn, &phrase).map_err(|e| e.to_string())
}

/// v1.5.68 — cross-device restore. On a new machine, the user types
/// the 24-word mnemonic they saved when they first set up the vault on
/// the original machine, plus a NEW PIN for THIS device. We derive the
/// deterministic KEK from the phrase, write a fresh vault row keyed
/// off the new PIN, and stash the KEK in AppState. From here the user
/// is unlocked and can decrypt any `.rtenc` files they brought over.
///
/// Validates the mnemonic format up front so a typo doesn't blow away
/// any existing vault row. Argon2id is heavy; we run it on the
/// blocking pool so the IPC thread stays responsive.
#[tauri::command]
pub async fn vault_restore_from_mnemonic(
    phrase: String,
    new_pin: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    if new_pin.chars().count() < 6 {
        return Err("New PIN must be at least 6 characters".into());
    }
    crate::vault_crypto::validate_mnemonic(&phrase)?;
    let db_arc = state.db.clone();
    let kek = tauri::async_runtime::spawn_blocking(move || {
        let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
        db::vault_restore_from_mnemonic(&conn, &phrase, &new_pin).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    {
        let mut g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        *g = Some(kek);
    }
    Ok(())
}

/// Destructive wipe: drop the PIN AND mark every previously-private
/// photo as public again. Returns the number of photos affected so the
/// FE can surface a confirmation toast.
#[tauri::command]
pub fn vault_reset_full(
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::vault_reset_full(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn list_private_photos(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<serde_json::Value>, String> {
    // v1.5.160 — CRITICAL BUG FIX.
    //
    // The pre-1.5.160 implementation called `db::get_photos_by_ids` to
    // hydrate the result rows. That helper, by design, has
    // `WHERE p.private = 0` baked in so it never leaks vault content into
    // the default gallery — every search / tag / phash codepath funnels
    // through it. So the sequence was:
    //
    //   step 1: SELECT id FROM photos WHERE private = 1   → 6 rows
    //   step 2: get_photos_by_ids with WHERE private = 0  → 0 rows
    //
    // Net: `list_private_photos` ALWAYS returned an empty list, no matter
    // how many photos the user had vaulted. The vault page showed
    // "0 private photos" and a blank grid. The user noticed when they
    // dropped a folder, saw "encrypted N files" in the toast, then opened
    // the vault and saw nothing. They (correctly) thought we'd lost the
    // files — we hadn't, but they had no way to know.
    //
    // Fix: dedicated single-query inline join, NO get_photos_by_ids call.
    // We also return `vault_folder_id` so the FE can group entries by the
    // folder rows v1.5.158/9 introduced (used by v1.5.161 tree-view).
    //
    // Return type is `Vec<serde_json::Value>` instead of
    // `Vec<PhotoSummary>` so we can ship `vault_folder_id` without
    // amending PhotoSummary (10 construction sites; out of scope for a
    // bug-fix release). The FE only reads `id`, `filename`, `tags`, and
    // now `vault_folder_id` — all preserved.
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let mut stmt = conn.prepare(
        "SELECT p.id, p.path, p.filename, p.status, p.provider_used,
                p.media_type, p.date_taken, p.duration_secs,
                p.rating, p.favorite, p.vault_folder_id,
                GROUP_CONCAT(t.tag, ',') AS tags,
                COUNT(t.id) AS tag_count
           FROM photos p
           LEFT JOIN tags t ON t.photo_id = p.id
          WHERE p.private = 1
          GROUP BY p.id
          ORDER BY COALESCE(p.date_taken, p.created_at) DESC
          LIMIT 5000"
    ).map_err(|e| e.to_string())?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            let tags_str: Option<String> = r.get(11)?;
            let tags: Vec<String> = tags_str
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            Ok(serde_json::json!({
                "id":              r.get::<_, i64>(0)?,
                "path":            r.get::<_, String>(1)?,
                "filename":        r.get::<_, String>(2)?,
                "status":          r.get::<_, String>(3)?,
                "provider_used":   r.get::<_, Option<String>>(4)?,
                "media_type":      r.get::<_, Option<String>>(5)?.unwrap_or_else(|| "image".to_string()),
                "date_taken":      r.get::<_, Option<String>>(6)?,
                "duration_secs":   r.get::<_, Option<i32>>(7)?,
                "rating":          r.get::<_, Option<i32>>(8)?.unwrap_or(0),
                "favorite":        r.get::<_, Option<i32>>(9)?.unwrap_or(0) != 0,
                "vault_folder_id": r.get::<_, Option<i64>>(10)?,
                "tags":            tags,
                "tag_count":       r.get::<_, i64>(12)?,
            }))
        })
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// v1.5.160 — Folder-vault step 4/5. List every row in `vault_folders`
/// so the FE can render the tree-view in the vault page. Includes a
/// `photo_count` per folder (encrypted photos directly under that folder,
/// NOT counting subfolders) so the UI can show "Vacation 2024 (42)".
///
/// Refuses to run when the vault is locked — folder names are leaky
/// metadata even though the photo content stays encrypted, so we gate
/// the lookup on having a KEK in memory. The user has to unlock first.
#[tauri::command]
pub fn list_vault_folders(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<serde_json::Value>, String> {
    {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        if g.is_none() {
            return Err("Vault is locked — unlock it first.".into());
        }
    }
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let mut stmt = conn.prepare(
        "SELECT f.id, f.name, f.parent_id, f.original_path, f.created_at,
                (SELECT COUNT(*) FROM photos WHERE vault_folder_id = f.id AND private = 1) AS photo_count
           FROM vault_folders f
          ORDER BY f.parent_id IS NULL DESC, f.name COLLATE NOCASE"
    ).map_err(|e| e.to_string())?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            Ok(serde_json::json!({
                "id":            r.get::<_, i64>(0)?,
                "name":          r.get::<_, String>(1)?,
                "parent_id":     r.get::<_, Option<i64>>(2)?,
                "original_path": r.get::<_, String>(3)?,
                "created_at":    r.get::<_, String>(4)?,
                "photo_count":   r.get::<_, i64>(5)?,
            }))
        })
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

// ── NSFW auto-hide (CLIP-based) ───────────────────────────────────────────
//
// Re-uses the CLIP embeddings we already computed for semantic search.
// For each indexed photo, cosine-similarity its image embedding against
// the CLIP text embedding of a fixed NSFW prompt. Photos above `threshold`
// get `private = 1`, so they drop out of the default gallery and only
// show up in the Private Vault.

#[tauri::command]
pub async fn auto_hide_nsfw(
    tier: crate::clip::ClipTier,
    threshold: f32,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    let base = clip_models_dir(&app);

    let photo_embs: Vec<(i64, Vec<u8>)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_photos_with_clip_emb(&conn, tier.dir_name()).map_err(|e| e.to_string())?
    };
    if photo_embs.is_empty() {
        return Err("No CLIP-indexed photos. Run Semantic Index first.".into());
    }

    let db_arc = state.db.clone();
    let tier_key = tier.dir_name().to_string();
    // v1.5.73 — capture vault state so we can encrypt flagged photos
    // through move_photo_private instead of leaking plaintext via
    // set_photo_private(true).
    let kek_opt: Option<[u8; 32]> = {
        let g = state.vault_kek.lock().map_err(|_| "kek lock")?;
        *g
    };
    let thumbs_dir = state.thumbnails_dir.clone();

    // A compact NSFW prompt set — we average their embeddings so no single
    // phrasing dominates. These stay in English because CLIP was trained
    // on English captions.
    const NSFW_PROMPTS: &[&str] = &[
        "nude photograph of a person",
        "explicit sexual content",
        "naked human body, adult content",
    ];

    let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        // Load or compute the average NSFW prompt embedding (cached like
        // normal semantic search queries).
        let mut engine = crate::clip::load_engine(&base, tier).map_err(|e| e.to_string())?;
        let mut prompt_embs: Vec<Vec<f32>> = vec![];
        for prompt in NSFW_PROMPTS {
            let cached = {
                let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                db::get_cached_text_embedding(&conn, &tier_key, prompt).ok().flatten()
            };
            let emb = if let Some(bytes) = cached {
                crate::clip::bytes_to_embedding(&bytes)
            } else {
                let e = crate::clip::encode_text(&mut engine, prompt).map_err(|e| e.to_string())?;
                let bytes = crate::clip::embedding_to_bytes(&e);
                let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
                let _ = db::put_cached_text_embedding(&conn, &tier_key, prompt, &bytes);
                e
            };
            prompt_embs.push(emb);
        }
        // v1.5.75 — bounds check on prompt_embs[0]. Without this, a model
        // misload / GPU OOM (where encode_text fails for EVERY prompt but
        // we keep going on the cache path) leaves prompt_embs empty and
        // indexing [0] panics the entire spawn_blocking task. The user
        // sees a generic IPC error and has no idea CLIP failed to load.
        if prompt_embs.is_empty() {
            return Err(
                "Could not encode any NSFW prompt — CLIP model may be missing or out of GPU memory. \
                 Try Settings → Re-download CLIP models."
                    .to_string(),
            );
        }
        // Mean-pool into a single query vector.
        let dim = prompt_embs[0].len();
        let mut avg = vec![0f32; dim];
        for emb in &prompt_embs {
            for (i, v) in emb.iter().enumerate() { avg[i] += *v; }
        }
        let n = prompt_embs.len() as f32;
        for v in avg.iter_mut() { *v /= n; }

        // v1.5.73 — score every photo, hide those above the threshold.
        // "Hide" used to call set_photo_private(true) directly, leaking the
        // original file. We now collect ids to hide and route them through
        // move_photo_private which encrypts the file + thumb. Skip
        // encryption (and the whole hide step) if the vault is locked.
        let mut hidden_ids: Vec<i64> = Vec::new();
        let mut scored = 0usize;
        {
            let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
            for (pid, bytes) in &photo_embs {
                let emb = crate::clip::bytes_to_embedding(bytes);
                let sim = crate::clip::cosine_similarity(&avg, &emb);
                let _ = db::set_photo_nsfw_score(&conn, *pid, sim);
                scored += 1;
                if sim >= threshold {
                    hidden_ids.push(*pid);
                }
            }
        }
        // Encrypt the flagged ones (no-op when KEK is None — leave scoring
        // intact so the user can re-run after unlocking).
        let mut hidden = 0usize;
        if let Some(kek) = kek_opt {
            for pid in &hidden_ids {
                if move_photo_private(&db_arc, *pid, true, &kek, &thumbs_dir).is_ok() {
                    hidden += 1;
                }
            }
        }
        Ok(serde_json::json!({
            "scored": scored,
            "hidden": hidden,
            "flagged": hidden_ids.len(),
            "vault_locked": kek_opt.is_none(),
            "threshold": threshold,
        }))
    })
    .await
    .map_err(|e| e.to_string())??;

    Ok(result)
}

// ── Smart GPS collections ─────────────────────────────────────────────────
//
// Simple single-pass spatial clustering on photo GPS coords. For each photo,
// attach it to the nearest existing cluster if within RADIUS_KM, otherwise
// seed a new cluster. Cluster centers are maintained as running means.
// The result lets the Map view group a thousand pins into ~10 travel
// collections ("Istanbul trip", "Kapadokya 2023", …).

const CLUSTER_RADIUS_KM: f64 = 1.5; // tight enough to split neighborhoods

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0_f64;
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();
    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

#[tauri::command]
pub async fn compute_gps_clusters(
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    // Load photos with GPS.
    let photos: Vec<(i64, f64, f64, Option<String>)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let mut stmt = conn.prepare(
            "SELECT id, gps_lat, gps_lon, date_taken FROM photos
             WHERE gps_lat IS NOT NULL AND gps_lon IS NOT NULL AND private = 0"
        ).map_err(|e| e.to_string())?;
        let rows: Vec<(i64, f64, f64, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    if photos.is_empty() {
        return Ok(serde_json::json!({ "clusters": 0, "photos": 0 }));
    }

    // Single-pass clustering with running-mean centers.
    struct Cluster {
        lat: f64, lon: f64, n: i64,
        date_start: Option<String>, date_end: Option<String>,
        photo_ids: Vec<i64>,
    }
    let mut clusters: Vec<Cluster> = vec![];

    for (pid, lat, lon, date) in &photos {
        let mut best: Option<usize> = None;
        let mut best_d = f64::MAX;
        for (i, c) in clusters.iter().enumerate() {
            let d = haversine_km(*lat, *lon, c.lat, c.lon);
            if d < best_d { best_d = d; best = Some(i); }
        }
        if let (Some(idx), true) = (best, best_d <= CLUSTER_RADIUS_KM) {
            let c = &mut clusters[idx];
            let n1 = c.n + 1;
            c.lat = (c.lat * c.n as f64 + *lat) / n1 as f64;
            c.lon = (c.lon * c.n as f64 + *lon) / n1 as f64;
            c.n = n1;
            c.photo_ids.push(*pid);
            if let Some(d) = date {
                if c.date_start.as_deref().map(|s| d.as_str() < s).unwrap_or(true) {
                    c.date_start = Some(d.clone());
                }
                if c.date_end.as_deref().map(|s| d.as_str() > s).unwrap_or(true) {
                    c.date_end = Some(d.clone());
                }
            }
        } else {
            clusters.push(Cluster {
                lat: *lat, lon: *lon, n: 1,
                date_start: date.clone(), date_end: date.clone(),
                photo_ids: vec![*pid],
            });
        }
    }

    // Keep only clusters with 3+ photos — singletons / pairs aren't useful.
    clusters.retain(|c| c.n >= 3);

    // Persist.
    let count = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::clear_gps_clusters(&conn).map_err(|e| e.to_string())?;
        for c in &clusters {
            let cid = db::insert_gps_cluster(
                &conn, c.lat, c.lon, CLUSTER_RADIUS_KM, c.n, None,
                c.date_start.as_deref(), c.date_end.as_deref(),
            ).map_err(|e| e.to_string())?;
            for pid in &c.photo_ids {
                let _ = db::link_photo_to_cluster(&conn, cid, *pid);
            }
        }
        clusters.len()
    };

    Ok(serde_json::json!({ "clusters": count, "photos": photos.len() }))
}

#[tauri::command]
pub fn get_gps_clusters(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<serde_json::Value>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let mut stmt = conn.prepare(
        "SELECT id, center_lat, center_lon, photo_count, label, date_start, date_end
         FROM gps_clusters ORDER BY photo_count DESC"
    ).map_err(|e| e.to_string())?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            Ok(serde_json::json!({
                "id":          r.get::<_, i64>(0)?,
                "lat":         r.get::<_, f64>(1)?,
                "lon":         r.get::<_, f64>(2)?,
                "photo_count": r.get::<_, i64>(3)?,
                "label":       r.get::<_, Option<String>>(4)?,
                "date_start":  r.get::<_, Option<String>>(5)?,
                "date_end":    r.get::<_, Option<String>>(6)?,
            }))
        })
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);
    Ok(rows)
}

/// Promote a GPS cluster to a named manual collection. The new collection
/// gets every photo currently linked to the cluster in `gps_cluster_photos`.
/// Useful after `compute_gps_clusters` for turning "this bundle of 42 photos
/// taken near 41.01,28.97" into a proper album the user can find later.
#[tauri::command]
pub async fn save_gps_cluster_as_collection(
    cluster_id: i64,
    name: String,
    state: tauri::State<'_, AppState>,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    // Collect the cluster's photo ids first so we don't hold the prepared
    // statement open while create_collection inserts.
    let photo_ids: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT photo_id FROM gps_cluster_photos WHERE cluster_id = ?1")
            .map_err(|e| e.to_string())?;
        let ids: Vec<i64> = stmt
            .query_map(rusqlite::params![cluster_id], |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        ids
    };
    if photo_ids.is_empty() {
        return Err("Cluster has no photos".into());
    }
    let coll_id = db::create_collection(&conn, &name, "manual", None)
        .map_err(|e| e.to_string())?;
    for pid in &photo_ids {
        let _ = db::add_photo_to_collection(&conn, coll_id, *pid);
    }
    Ok(coll_id)
}

// ── Duplicate Smart Merge ──────────────────────────────────────────────────
// Consolidate tags/rating/favorite/description from a set of duplicate photos
// onto a chosen keeper, then remove the duplicates. Unlike `delete_photos`,
// this preserves the user's investment in metadata spread across duplicate
// copies — e.g. if one copy has 5★ and another has tags from a different
// scan pass, both land on the keeper.

#[derive(serde::Serialize, Clone, Debug)]
pub struct MergeResult {
    pub keeper_id: i64,
    pub merged_count: usize,
    pub deleted_count: usize,
    pub tags_added: usize,
    pub rating_updated: bool,
    pub favorite_updated: bool,
    pub description_updated: bool,
}

#[tauri::command]
pub async fn merge_duplicate_photos(
    keeper_id: i64,
    dupe_ids: Vec<i64>,
    delete_file: bool,
    state: tauri::State<'_, AppState>,
) -> Result<MergeResult, String> {
    // Filter keeper out of dupe list defensively — caller mistake should not
    // cause us to delete our own keeper.
    let dupe_ids: Vec<i64> = dupe_ids.into_iter().filter(|id| *id != keeper_id).collect();
    if dupe_ids.is_empty() {
        return Err("No duplicate ids to merge".into());
    }

    // Phase 1: read + consolidate under a single DB lock.
    let mut tags_added = 0usize;
    let mut rating_updated = false;
    let mut favorite_updated = false;
    let mut description_updated = false;

    {
        let conn = state.db.lock().map_err(|_| "db lock")?;

        // Collect keeper's existing tags (case-insensitive dedup via lowercase key).
        let keeper_tags: std::collections::HashSet<String> = {
            let mut stmt = conn
                .prepare("SELECT tag FROM tags WHERE photo_id = ?1")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(rusqlite::params![keeper_id], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            rows.filter_map(|r| r.ok())
                .map(|t| t.to_lowercase())
                .collect()
        };

        // Collect all dupe tags (with source + confidence) to merge in.
        let mut dupe_rows: Vec<(String, Option<f64>, Option<String>)> = Vec::new();
        for &did in &dupe_ids {
            let mut stmt = conn
                .prepare("SELECT tag, confidence, source FROM tags WHERE photo_id = ?1")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(rusqlite::params![did], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<f64>>(1).unwrap_or(None),
                        r.get::<_, Option<String>>(2).unwrap_or(None),
                    ))
                })
                .map_err(|e| e.to_string())?;
            for row in rows.flatten() {
                dupe_rows.push(row);
            }
        }

        // Insert new tags on keeper (OR IGNORE handles the UNIQUE(photo_id, tag)
        // collision for the case where the keeper already has this tag from a
        // different pipeline; the first one wins).
        for (tag, confidence, source) in &dupe_rows {
            if keeper_tags.contains(&tag.to_lowercase()) {
                continue;
            }
            match conn.execute(
                "INSERT OR IGNORE INTO tags (photo_id, tag, confidence, source) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![keeper_id, tag, confidence, source],
            ) {
                Ok(n) if n > 0 => tags_added += 1,
                _ => {}
            }
        }

        // Rating: take max across keeper + dupes.
        let keeper_rating: i32 = conn
            .query_row(
                "SELECT COALESCE(rating, 0) FROM photos WHERE id = ?1",
                rusqlite::params![keeper_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let mut best_rating = keeper_rating;
        for &did in &dupe_ids {
            let r: i32 = conn
                .query_row(
                    "SELECT COALESCE(rating, 0) FROM photos WHERE id = ?1",
                    rusqlite::params![did],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if r > best_rating {
                best_rating = r;
            }
        }
        if best_rating != keeper_rating {
            db::set_rating(&conn, keeper_id, best_rating).map_err(|e| e.to_string())?;
            rating_updated = true;
        }

        // Favorite: true if any copy was favorited.
        let keeper_fav: bool = conn
            .query_row(
                "SELECT COALESCE(favorite, 0) FROM photos WHERE id = ?1",
                rusqlite::params![keeper_id],
                |r| r.get::<_, i64>(0).map(|v| v != 0),
            )
            .unwrap_or(false);
        let mut any_fav = keeper_fav;
        if !any_fav {
            for &did in &dupe_ids {
                let f: bool = conn
                    .query_row(
                        "SELECT COALESCE(favorite, 0) FROM photos WHERE id = ?1",
                        rusqlite::params![did],
                        |r| r.get::<_, i64>(0).map(|v| v != 0),
                    )
                    .unwrap_or(false);
                if f {
                    any_fav = true;
                    break;
                }
            }
        }
        if any_fav && !keeper_fav {
            db::set_favorite(&conn, keeper_id, true).map_err(|e| e.to_string())?;
            favorite_updated = true;
        }

        // Description: if keeper is empty, fill from first non-empty dupe.
        let keeper_desc: Option<String> = conn
            .query_row(
                "SELECT description FROM photos WHERE id = ?1",
                rusqlite::params![keeper_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap_or(None);
        let keeper_empty = keeper_desc.as_ref().map(|s| s.trim().is_empty()).unwrap_or(true);
        if keeper_empty {
            for &did in &dupe_ids {
                let d: Option<String> = conn
                    .query_row(
                        "SELECT description FROM photos WHERE id = ?1",
                        rusqlite::params![did],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .unwrap_or(None);
                if let Some(s) = d {
                    if !s.trim().is_empty() {
                        db::update_photo_description(&conn, keeper_id, &s)
                            .map_err(|e| e.to_string())?;
                        description_updated = true;
                        break;
                    }
                }
            }
        }
    } // DB lock released before delegating to delete_photos (which re-locks internally).

    // Phase 2: delete the dupes via the existing path (recycle-bin aware,
    // thumbnail cleanup, DB cascade).
    let deleted_count = delete_photos(dupe_ids.clone(), delete_file, state).await?;

    Ok(MergeResult {
        keeper_id,
        merged_count: dupe_ids.len(),
        deleted_count,
        tags_added,
        rating_updated,
        favorite_updated,
        description_updated,
    })
}

// ── Folder Auto-Organize ───────────────────────────────────────────────────
// One-click: for every distinct `folder` the library has indexed, create a
// "manual" collection named after the last 1-2 path segments and populate it
// with every photo from that folder. Parallels the Smart Places "save all
// clusters as collections" flow (Round 11). Existing collections with the
// same name are skipped — idempotent on repeat runs.

#[derive(serde::Serialize, Clone, Debug)]
pub struct FolderOrganizeResult {
    pub created: usize,
    pub skipped: usize,
    pub total_photos_assigned: usize,
}

/// Turn a folder path like `C:/Users/foo/Pictures/Trip 2023/DCIM` into a
/// short, recognizable collection name. Uses the last two path segments when
/// available ("Trip 2023 / DCIM") and falls back to the single leaf. Handles
/// both `/` and `\` separators. Never returns an empty string — if the path
/// has no separators it returns the path itself.
fn _folder_collection_name(folder: &str) -> String {
    let trimmed = folder.trim_end_matches(|c: char| c == '/' || c == '\\');
    if trimmed.is_empty() {
        return folder.to_string();
    }
    let parts: Vec<&str> = trimmed
        .split(|c: char| c == '/' || c == '\\')
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return folder.to_string();
    }
    if parts.len() == 1 {
        return parts[0].to_string();
    }
    let n = parts.len();
    format!("{} / {}", parts[n - 2], parts[n - 1])
}

#[tauri::command]
pub async fn save_all_folders_as_collections(
    state: tauri::State<'_, AppState>,
) -> Result<FolderOrganizeResult, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let folders = db::get_folders(&conn).map_err(|e| e.to_string())?;
    if folders.is_empty() {
        return Ok(FolderOrganizeResult {
            created: 0,
            skipped: 0,
            total_photos_assigned: 0,
        });
    }

    // Pre-fetch existing collection names so we can skip duplicates with one
    // lookup instead of a SELECT per folder.
    let existing_names: std::collections::HashSet<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM collections")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let mut created = 0usize;
    let mut skipped = 0usize;
    let mut total_assigned = 0usize;

    for (folder, _count) in folders {
        if folder.trim().is_empty() {
            skipped += 1;
            continue;
        }
        let base_name = _folder_collection_name(&folder);
        // If a collection with this short name already exists, disambiguate
        // with the last 3 segments; if that collides too, skip.
        let name = if existing_names.contains(&base_name) {
            // Try last three segments as a tiebreak.
            let parts: Vec<&str> = folder
                .split(|c: char| c == '/' || c == '\\')
                .filter(|s| !s.is_empty())
                .collect();
            if parts.len() >= 3 {
                let n = parts.len();
                let longer = format!("{} / {} / {}", parts[n - 3], parts[n - 2], parts[n - 1]);
                if existing_names.contains(&longer) {
                    skipped += 1;
                    continue;
                }
                longer
            } else {
                skipped += 1;
                continue;
            }
        } else {
            base_name
        };

        let coll_id = match db::create_collection(&conn, &name, "manual", None) {
            Ok(id) => id,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        // Fetch photo ids for this folder and add to the new collection.
        let photo_ids: Vec<i64> = {
            let mut stmt = match conn
                .prepare("SELECT id FROM photos WHERE folder = ?1 ORDER BY COALESCE(date_taken, mtime) DESC")
            {
                Ok(s) => s,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            stmt.query_map(rusqlite::params![&folder], |r| r.get::<_, i64>(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        };

        for pid in &photo_ids {
            let _ = db::add_photo_to_collection(&conn, coll_id, *pid);
        }
        total_assigned += photo_ids.len();
        created += 1;
    }

    Ok(FolderOrganizeResult {
        created,
        skipped,
        total_photos_assigned: total_assigned,
    })
}

// ── Trending Tags ──────────────────────────────────────────────────────────
// "What tags have been most added recently?" — joins tags to photos via
// photos.tagged_at so we only count tags attached during the window. Useful
// as a sidebar-discovery widget: click a trending tag to filter.
//
// Returns Vec<(tag, count)> tuples rather than a named struct so we sidestep
// any serde field-name edge cases on the JS side; the frontend reads the
// two fixed positions and that's it.
//
// Fallback behaviour: if no photos were tagged in the last `days` days
// (e.g. fresh install, or the user hasn't tagged anything recently), we
// widen the window to "all-time most-used tags" so the widget has content
// to show instead of an empty panel.

#[tauri::command]
pub async fn get_trending_tags(
    days: Option<i64>,
    limit: Option<i64>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<(String, i64)>, String> {
    let days = days.unwrap_or(7).clamp(1, 365);
    let limit = limit.unwrap_or(15).clamp(1, 100);
    let conn = state.db.lock().map_err(|_| "db lock")?;

    // `days` and `limit` are integer-clamped above, safe to inline.
    let windowed_sql = format!(
        "SELECT t.tag, COUNT(*) AS n
         FROM tags t
         JOIN photos p ON p.id = t.photo_id
         WHERE p.tagged_at IS NOT NULL
           AND p.tagged_at >= datetime('now', '-{} days')
           AND t.tag IS NOT NULL
           AND TRIM(t.tag) <> ''
         GROUP BY t.tag
         ORDER BY n DESC, t.tag ASC
         LIMIT {}",
        days, limit
    );

    let mut out: Vec<(String, i64)> = {
        let mut stmt = conn.prepare(&windowed_sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| e.to_string())?;
        rows.filter_map(|r| r.ok())
            .filter(|(t, _)| !t.trim().is_empty())
            .collect()
    };

    // Fallback: nothing tagged in the last `days` days → show the library's
    // most-used tags all-time. Better than an empty widget on a library
    // whose tagging pass ran weeks ago.
    if out.is_empty() {
        let fallback_sql = format!(
            "SELECT tag, COUNT(*) AS n
             FROM tags
             WHERE tag IS NOT NULL AND TRIM(tag) <> ''
             GROUP BY tag
             ORDER BY n DESC, tag ASC
             LIMIT {}",
            limit
        );
        let mut stmt = conn.prepare(&fallback_sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| e.to_string())?;
        out = rows
            .filter_map(|r| r.ok())
            .filter(|(t, _)| !t.trim().is_empty())
            .collect();
    }

    Ok(out)
}

// ─── Person filter (v1.5.107) ────────────────────────────────────────────
//
// Sidebar person clicks used to set `curSearch = person_name.toLowerCase()`
// and route through search_photos free-text. That tokenised the name into
// individual words (`"ali can bombadil"` → `["ali", "can", "bombadil"]`)
// and AND-intersected them across all photo tags, so "Ali Can Bombadil"
// was matching every photo that happened to contain the English word "can"
// in any tag or description (Coca-Cola cans, Jack Daniel's cans, "can do"
// in AI captions). One-word names didn't have this problem.
//
// This command takes the person name as a single LIKE pattern against the
// `persons.name` column joined through `face_regions` — exactly what we
// want: photos with a face assigned to that person, nothing else.

#[tauri::command]
pub async fn find_photos_by_person(
    name: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<PhotoSummary>, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|_| "db lock".to_string())?;
        db::search_photos_by_person(&conn, &name).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ─── XMP backfill (v1.5.104) ────────────────────────────────────────────
//
// Backfill tags/description/rating/favorite from existing `.xmp`
// sidecars for every photo already in the library. Use case: the user
// has tagged photos on the Mac side over a shared volume; their
// Windows library has the photo rows but no tags because pre-v1.5.104
// scans never read sidecars. This command sweeps the whole library
// once.
//
// Emits `xmp-import-progress` events with `{ total, done, imported,
// updated_tags }` so the UI can show a progress bar. Skipped photos
// (no sidecar / no new info) don't count toward `imported`.

#[derive(serde::Serialize, Clone)]
pub struct XmpImportProgress {
    pub total: usize,
    pub done: usize,
    pub imported: usize,
    pub updated_tags: usize,
    pub finished: bool,
}

#[derive(serde::Serialize)]
pub struct XmpImportResult {
    pub scanned: usize,
    pub with_sidecar: usize,
    pub new_tags: usize,
    pub new_descriptions: usize,
    pub new_ratings: usize,
    pub new_favorites: usize,
}

#[tauri::command]
pub async fn import_xmp_sidecars(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<XmpImportResult, String> {
    use tauri::Emitter;
    let db = state.db.clone();
    let ah = app_handle.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // Pull all photo paths up front so we can release the lock between
        // sidecar reads (they're disk I/O, no DB needed).
        let photos: Vec<(i64, String)> = {
            let conn = db.lock().map_err(|_| "db lock".to_string())?;
            let mut s = conn
                .prepare("SELECT id, path FROM photos")
                .map_err(|e| e.to_string())?;
            let rows = s
                .query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| e.to_string())?;
            rows.filter_map(|r| r.ok()).collect()
        };

        let total = photos.len();
        let mut result = XmpImportResult {
            scanned: 0,
            with_sidecar: 0,
            new_tags: 0,
            new_descriptions: 0,
            new_ratings: 0,
            new_favorites: 0,
        };

        // Batch DB writes — open a fresh transaction every 500 rows so a
        // long sweep doesn't hold the lock continuously.
        const BATCH: usize = 500;
        let mut pending: Vec<(i64, crate::xmp::XmpRead)> = Vec::with_capacity(BATCH);

        let flush = |db: &Arc<Mutex<rusqlite::Connection>>,
                     pending: &mut Vec<(i64, crate::xmp::XmpRead)>,
                     result: &mut XmpImportResult|
         -> Result<(), String> {
            if pending.is_empty() {
                return Ok(());
            }
            let conn = db.lock().map_err(|_| "db lock".to_string())?;
            let txn = conn.unchecked_transaction().map_err(|e| e.to_string())?;
            // v1.5.106 — direct INSERT (db::insert_tags opens a nested
            // tx which SQLite silently fails inside an outer txn).
            // v1.5.111 — case-insensitive dup guard so "Lara"/"lara"
            // don't both land on the same photo.
            let mut tag_stmt = txn
                .prepare_cached(
                    "INSERT INTO tags (photo_id, tag, confidence, source)
                     SELECT ?1, ?2, ?3, ?4
                     WHERE NOT EXISTS (
                         SELECT 1 FROM tags
                         WHERE photo_id = ?1 AND tag = ?2 COLLATE NOCASE
                     )",
                )
                .map_err(|e| e.to_string())?;
            // v1.5.110 — flip pending→tagged so the sidebar "Tagged"
            // count includes Mac-only photos.
            let mut mark_tagged_stmt = txn
                .prepare_cached(
                    "UPDATE photos SET status='tagged', tagged_at=?2 WHERE id=?1 AND status='pending'",
                )
                .map_err(|e| e.to_string())?;
            let now = chrono::Utc::now().to_rfc3339();
            for (id, xmp) in pending.drain(..) {
                if !xmp.keywords.is_empty() {
                    for tag in &xmp.keywords {
                        if tag_stmt
                            .execute(rusqlite::params![id, tag, 1.0_f64, "xmp_sidecar"])
                            .map(|n| n > 0)
                            .unwrap_or(false)
                        {
                            result.new_tags += 1;
                        }
                    }
                    let _ = mark_tagged_stmt.execute(rusqlite::params![id, &now]);
                }
                if let Some(desc) = xmp.description.as_deref() {
                    if !desc.trim().is_empty() {
                        if db::update_photo_description(&txn, id, desc).is_ok() {
                            result.new_descriptions += 1;
                        }
                    }
                }
                if let Some(r) = xmp.rating {
                    if (-1..=5).contains(&r) {
                        if db::set_rating(&txn, id, r).is_ok() {
                            result.new_ratings += 1;
                        }
                    }
                }
                if xmp.label.as_deref() == Some("Red") {
                    if db::set_favorite(&txn, id, true).is_ok() {
                        result.new_favorites += 1;
                    }
                }
            }
            drop(tag_stmt);
            drop(mark_tagged_stmt);
            txn.commit().map_err(|e| e.to_string())?;
            Ok(())
        };

        for (id, path) in photos.iter() {
            result.scanned += 1;
            if let Ok(Some(xmp)) = crate::xmp::read_xmp_sidecar(path) {
                if !xmp.keywords.is_empty()
                    || xmp.description.is_some()
                    || xmp.rating.is_some()
                    || xmp.label.is_some()
                {
                    result.with_sidecar += 1;
                    pending.push((*id, xmp));
                }
            }
            if pending.len() >= BATCH {
                flush(&db, &mut pending, &mut result)?;
            }
            if result.scanned % 200 == 0 {
                ah.emit(
                    "xmp-import-progress",
                    XmpImportProgress {
                        total,
                        done: result.scanned,
                        imported: result.with_sidecar,
                        updated_tags: result.new_tags,
                        finished: false,
                    },
                )
                .ok();
            }
        }
        flush(&db, &mut pending, &mut result)?;

        ah.emit(
            "xmp-import-progress",
            XmpImportProgress {
                total,
                done: result.scanned,
                imported: result.with_sidecar,
                updated_tags: result.new_tags,
                finished: true,
            },
        )
        .ok();

        Ok::<_, String>(result)
    })
    .await
    .map_err(|e| e.to_string())?
}
