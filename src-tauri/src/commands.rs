use std::sync::atomic::Ordering;

use base64::Engine as _;
use tauri::{Emitter, Manager};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

use crate::{db, export, exif_reader, models::*, providers::{self, DEFAULT_OLLAMA_URL}, router::SmartRouter, thumbnail, xmp, AppState};

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
pub async fn stop_scan(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.scan_stop.store(true, Ordering::SeqCst);
    Ok(())
}

// ── Photo queries ─────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_folders(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<(String, i64)>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_folders(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_photos(
    offset: i64,
    limit: i64,
    folder: Option<String>,
    tag_filter: Option<String>,
    status_filter: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<PhotosResponse, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
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
}

#[tauri::command]
pub async fn get_photo_detail(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<Photo, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_photo_detail(&conn, photo_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_stats(state: tauri::State<'_, AppState>) -> Result<AppStats, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_stats(&conn).map_err(|e| e.to_string())
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

    // Check if the query contains non-ASCII (likely non-English)
    let is_non_english = trimmed.chars().any(|c| !c.is_ascii());

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

        // Search with translated terms + original
        let mut all_terms = english_terms;
        all_terms.push(trimmed);
        all_terms.sort();
        all_terms.dedup();

        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::search_photos_multi(&conn, &all_terms).map_err(|e| e.to_string())
    } else {
        // English search — expand with synonyms then FTS
        let expanded = expand_synonyms(&trimmed);
        let conn = state.db.lock().map_err(|_| "db lock")?;

        if expanded.len() > 1 {
            // Search with all synonym terms
            db::search_photos_multi(&conn, &expanded).map_err(|e| e.to_string())
        } else {
            let fts_query = if trimmed.contains(' ') {
                trimmed.clone()
            } else {
                format!("{}*", trimmed)
            };
            db::search_photos_fts(&conn, &fts_query).map_err(|e| e.to_string())
        }
    }
}

/// Expand search term with common synonyms for better recall
fn expand_synonyms(term: &str) -> Vec<String> {
    let lower = term.to_lowercase();
    let groups: &[&[&str]] = &[
        &["woman", "women", "female", "lady", "girl"],
        &["man", "men", "male", "boy", "guy"],
        &["child", "kid", "children", "baby", "toddler", "infant"],
        &["food", "meal", "dish", "cuisine", "dinner", "lunch", "breakfast"],
        &["car", "vehicle", "automobile", "auto"],
        &["dog", "puppy", "canine"],
        &["cat", "kitten", "feline"],
        &["house", "home", "building", "residence"],
        &["happy", "smiling", "joyful", "cheerful", "laughing"],
        &["sad", "crying", "unhappy", "melancholy"],
        &["beautiful", "pretty", "gorgeous", "stunning"],
        &["old", "elderly", "aged", "senior", "ancient"],
        &["young", "youthful", "teen", "teenager"],
        &["big", "large", "huge", "giant", "massive"],
        &["small", "little", "tiny", "miniature"],
        &["street", "road", "avenue", "path", "alley"],
        &["ocean", "sea", "water", "beach", "shore", "coast"],
        &["mountain", "hill", "peak", "summit"],
        &["tree", "forest", "woods", "jungle"],
        &["flower", "blossom", "bloom", "floral"],
        &["night", "dark", "evening", "nighttime"],
        &["couple", "romantic", "love", "together", "pair"],
    ];
    for group in groups {
        if group.contains(&lower.as_str()) {
            return group.iter().map(|s| s.to_string()).collect();
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
        let b64 = thumbnail::get_or_create_thumbnail(&path, &hash, &thumbs_dir, 200)
            .map_err(|e| e.to_string())?;

        // Persist thumbnail path
        let cache_name = format!("{}.jpg", &hash[..hash.len().min(24)]);
        let thumb_path = thumbs_dir.join(&cache_name);
        if let Ok(conn) = db_arc.lock() {
            db::update_thumbnail_path(&conn, photo_id, &thumb_path.to_string_lossy()).ok();
        }

        Ok(b64)
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

        app_handle.emit("tag-complete", result).ok();
    });

    Ok(())
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

// ══════════════════════════════════════════════════════════════════════════════
// NEW FEATURES
// ══════════════════════════════════════════════════════════════════════════════

// ── 1. XMP Sidecar Writing ──────────────────────────────────────────────────

#[tauri::command]
pub async fn write_xmp_for_photo(
    photo_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let (path, tags) = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let photo = db::get_photo_detail(&conn, photo_id).map_err(|e| e.to_string())?;
        let tags: Vec<String> = photo.tags.iter().map(|t| t.tag.clone()).collect();
        (photo.path, tags)
    };
    tokio::task::spawn_blocking(move || {
        xmp::write_xmp_sidecar(&path, &tags).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn write_xmp_all(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<usize, String> {
    let photos_tags: Vec<(String, Vec<String>)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let mut stmt = conn.prepare(
            "SELECT p.id, p.path FROM photos p WHERE p.status = 'tagged'"
        ).map_err(|e| e.to_string())?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

        let mut result = Vec::new();
        for (id, path) in rows {
            let mut tag_stmt = conn.prepare_cached(
                "SELECT tag FROM tags WHERE photo_id = ?1"
            ).map_err(|e| e.to_string())?;
            let tags: Vec<String> = tag_stmt
                .query_map(rusqlite::params![id], |r| r.get(0))
                .map_err(|e| e.to_string())?
                .filter_map(|r| r.ok())
                .collect();
            result.push((path, tags));
        }
        result
    };

    let total = photos_tags.len();
    let mut success = 0usize;
    for (i, (path, tags)) in photos_tags.iter().enumerate() {
        if xmp::write_xmp_sidecar(path, tags).is_ok() {
            success += 1;
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
    state: tauri::State<'_, AppState>,
) -> Result<ExportResult, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let count = match format.as_str() {
        "csv" => export::export_csv(&conn, &output_path).map_err(|e| e.to_string())?,
        "json" => export::export_json(&conn, &output_path).map_err(|e| e.to_string())?,
        _ => return Err("Unknown format. Use 'csv' or 'json'".into()),
    };
    Ok(ExportResult { path: output_path, count })
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
pub async fn start_watching(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    let folders: Vec<String> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_watch_folders(&conn)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|w| w.enabled)
            .map(|w| w.path)
            .collect()
    };

    if folders.is_empty() {
        return Err("No watch folders configured".into());
    }

    let db_arc = state.db.clone();
    let thumbs_dir = state.thumbnails_dir.clone();

    let watcher = crate::watcher::FolderWatcher::new(
        folders, db_arc, thumbs_dir, app_handle,
    ).map_err(|e| e.to_string())?;

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
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_collections(&conn).map_err(|e| e.to_string())
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
    let path = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        let (p, _) = db::get_photo_path_and_hash(&conn, photo_id).map_err(|e| e.to_string())?;
        p
    };
    tokio::task::spawn_blocking(move || {
        exif_reader::read_exif(&path).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_gps_photos(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<GpsPhoto>, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    db::get_photos_with_gps(&conn).map_err(|e| e.to_string())
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

// ── 10. Duplicate Detection ─────────────────────────────────────────────────

#[tauri::command]
pub async fn compute_phashes(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<usize, String> {
    let photos: Vec<(i64, String)> = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_photos_without_phash(&conn, 10000).map_err(|e| e.to_string())?
    };

    let total = photos.len();
    let mut done = 0usize;

    for (id, path) in &photos {
        if let Ok(hash) = exif_reader::compute_phash(path) {
            let conn = state.db.lock().map_err(|_| "db lock")?;
            db::update_photo_phash(&conn, *id, &hash).ok();
        }
        done += 1;
        if done % 50 == 0 {
            app_handle.emit("phash-progress", serde_json::json!({"done": done, "total": total})).ok();
        }
    }
    Ok(done)
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
pub async fn translate_for_clip(
    query: String,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let trimmed = query.trim().to_string();
    let is_non_english = trimmed.chars().any(|c| !c.is_ascii());
    if !is_non_english {
        return Ok(trimmed);
    }

    // Check cache first
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
        "Translate this single word or phrase to English. \
         Return ONLY the exact English translation, one word or short phrase. \
         Do NOT add synonyms or related words. \
         Examples: kadın→woman, kedi→cat, araba→car, güneş batımı→sunset \
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
            // For cloud providers, use existing translate_query
            providers::translate_query(&prompt, provider, &api_key)
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
    let current = "1.0.0".to_string();

    // Try to check GitHub releases (if configured)
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    match client
        .get("https://api.github.com/repos/retinatag/retinatag/releases/latest")
        .header("User-Agent", "RetinaTag/1.0")
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
}

#[tauri::command]
pub async fn get_library_stats(state: tauri::State<'_, AppState>) -> Result<LibraryStats, String> {
    let conn = state.db.lock().map_err(|_| "db lock")?;
    let (total, pending, tagged, error) = db::get_status_counts(&conn).map_err(|e| e.to_string())?;
    Ok(LibraryStats { total, pending, tagged, error })
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

    tokio::task::spawn_blocking(move || -> Result<Vec<FaceRegion>, String> {
        let face_models = crate::face::load_models(&models_dir)
            .map_err(|e| e.to_string())?;

        let img = image::open(&photo_path)
            .map_err(|e| format!("Failed to open photo: {}", e))?;

        // Clear previous detections for this photo
        {
            let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
            db::delete_faces_for_photo(&conn, photo_id).ok();
        }

        let detected = crate::face::detect_faces(&face_models, &img)
            .map_err(|e| format!("Face detection error: {}", e))?;

        let faces_dir = thumbs_dir.join("faces");
        std::fs::create_dir_all(&faces_dir).ok();

        let mut result = Vec::new();

        for face in &detected {
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

/// Get unassigned faces (unique per cluster) with thumbnails for "Who is this?" popup
#[tauri::command]
pub async fn get_unknown_faces(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<FaceRegion>, String> {
    let conn = state.db.lock().unwrap_or_else(|e| e.into_inner());
    let thumbs_dir = state.thumbnails_dir.clone();
    let faces_dir = thumbs_dir.join("faces");

    // Get all unassigned faces
    let rows: Vec<(i64, i64, Vec<u8>)> = db::get_unassigned_faces_with_embeddings(&conn)
        .map_err(|e| e.to_string())?;

    if rows.is_empty() {
        return Ok(vec![]);
    }

    // Cluster them to avoid showing duplicates — show one face per cluster
    let face_data: Vec<(i64, Vec<f32>)> = rows.iter()
        .filter_map(|(fid, _pid, emb)| {
            let embedding = crate::face::bytes_to_embedding(emb);
            if embedding.len() == 512 { Some((*fid, embedding)) } else { None }
        })
        .collect();

    let clusters = crate::face::cluster_embeddings(&face_data);

    // Return the representative face from each cluster, preferring faces with thumbnails
    let mut result = Vec::new();
    for cluster in clusters.iter().take(20) { // max 20 unknown faces at a time
        // Try cluster representative first, fall back to any face in cluster with a thumbnail
        let mut chosen_id = cluster.representative;
        if face_thumb_b64(&faces_dir, chosen_id).is_none() {
            // Representative has no thumbnail — find another face in this cluster that does
            for &fid in &cluster.face_ids {
                if face_thumb_b64(&faces_dir, fid).is_some() {
                    chosen_id = fid;
                    break;
                }
            }
        }
        // Get face region info
        if let Ok(Some(face)) = db::get_face_region(&conn, chosen_id) {
            result.push(FaceRegion {
                id: face.id,
                photo_id: face.photo_id,
                x1: face.x1, y1: face.y1, x2: face.x2, y2: face.y2,
                score: face.score,
                person_id: None,
                person_name: None,
                thumbnail_b64: face_thumb_b64(&faces_dir, face.id),
            });
        }
    }
    Ok(result)
}

/// Name a face and auto-propagate to all matching faces across all photos
#[tauri::command]
pub async fn name_face_and_propagate(
    face_id: i64,
    name: String,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Name cannot be empty".into());
    }

    let db = state.db.clone();

    tokio::task::spawn_blocking(move || {
        let conn = db.lock().unwrap_or_else(|e| e.into_inner());

        // Create person if doesn't exist, or get existing
        let person_id = match db::find_person_by_name(&conn, &name) {
            Ok(Some(pid)) => pid,
            _ => db::create_person(&conn, &name).map_err(|e| e.to_string())?,
        };

        // Assign this face to the person
        db::assign_face_to_person(&conn, face_id, Some(person_id)).map_err(|e| e.to_string())?;

        // Add person name as tag to the photo
        if let Ok(Some(face)) = db::get_face_region(&conn, face_id) {
            db::insert_tags(&conn, face.photo_id, &[(name.clone(), 1.0, "face".to_string())]).ok();
        }

        // Now recognize all other matching faces
        let known = db::get_known_face_embeddings(&conn).map_err(|e| e.to_string())?;
        if known.is_empty() { return Ok(1); }

        // Build per-person average embeddings
        let mut person_embs: std::collections::HashMap<i64, (String, Vec<f32>, usize)> = std::collections::HashMap::new();
        for (pid, pname, emb_bytes) in &known {
            let emb = crate::face::bytes_to_embedding(emb_bytes);
            if emb.len() != 512 { continue; }
            let entry = person_embs.entry(*pid).or_insert_with(|| (pname.clone(), vec![0.0; 512], 0));
            for (i, v) in emb.iter().enumerate() { entry.1[i] += v; }
            entry.2 += 1;
        }
        let person_avgs: Vec<(i64, String, Vec<f32>)> = person_embs.into_iter().map(|(pid, (n, mut emb, count))| {
            let c = count as f32;
            let norm: f32 = emb.iter().map(|v| (v/c)*(v/c)).sum::<f32>().sqrt();
            if norm > 0.0 { for v in emb.iter_mut() { *v = *v / c / norm; } }
            (pid, n, emb)
        }).collect();

        // Match unassigned faces
        let unassigned = db::get_unassigned_faces_with_embeddings(&conn).map_err(|e| e.to_string())?;
        let mut matched = 0usize;
        for (fid, photo_id, emb_bytes) in &unassigned {
            let emb = crate::face::bytes_to_embedding(emb_bytes);
            if emb.len() != 512 { continue; }
            let mut best_sim = 0.0f32;
            let mut best_pid = 0i64;
            let mut best_name = String::new();
            for (pid, n, avg) in &person_avgs {
                let sim: f32 = emb.iter().zip(avg.iter()).map(|(a,b)| a*b).sum();
                if sim > best_sim { best_sim = sim; best_pid = *pid; best_name = n.clone(); }
            }
            if best_sim >= crate::face::RECOGNITION_THRESH {
                db::assign_face_to_person(&conn, *fid, Some(best_pid)).ok();
                db::insert_tags(&conn, *photo_id, &[(best_name, 1.0, "face".to_string())]).ok();
                matched += 1;
            }
        }
        Ok(matched + 1)
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

/// Auto-recognize all unassigned faces by comparing to known person embeddings.
/// Returns number of faces that were matched.
#[tauri::command]
pub async fn recognize_all_faces(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<usize, String> {
    let known = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_known_face_embeddings(&conn).map_err(|e| e.to_string())?
    };

    if known.is_empty() {
        return Err("Please assign some faces to people first, then recognition can be performed.".to_string());
    }

    // Build per-person averaged (mean) embedding
    let mut person_map: std::collections::HashMap<i64, (String, Vec<f64>, usize)> =
        std::collections::HashMap::new();
    for (pid, pname, bytes) in &known {
        let emb = crate::face::bytes_to_embedding(bytes);
        if emb.is_empty() {
            continue;
        }
        let entry = person_map
            .entry(*pid)
            .or_insert_with(|| (pname.clone(), vec![0.0f64; emb.len()], 0));
        for (acc, v) in entry.1.iter_mut().zip(&emb) {
            *acc += *v as f64;
        }
        entry.2 += 1;
    }

    // Normalise to get mean embeddings (as f32)
    let persons: Vec<(i64, String, Vec<f32>)> = person_map
        .into_iter()
        .filter_map(|(pid, (name, sum, cnt))| {
            if cnt == 0 {
                return None;
            }
            let mean: Vec<f32> = sum.iter().map(|v| (*v / cnt as f64) as f32).collect();
            // Re-normalise mean embedding
            let norm: f32 = mean.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm < 1e-8 {
                return None;
            }
            let normed: Vec<f32> = mean.iter().map(|v| v / norm).collect();
            Some((pid, name, normed))
        })
        .collect();

    let unassigned = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_unassigned_faces_with_embeddings(&conn).map_err(|e| e.to_string())?
    };

    let mut matched = 0usize;

    for (face_id, photo_id, emb_bytes) in &unassigned {
        let emb = crate::face::bytes_to_embedding(emb_bytes);
        if emb.is_empty() {
            continue;
        }

        // Find best matching person
        let best = persons
            .iter()
            .map(|(pid, name, mean_emb)| {
                let sim = crate::face::cosine_similarity(&emb, mean_emb);
                (*pid, name.as_str(), sim)
            })
            .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));

        if let Some((pid, person_name, sim)) = best {
            if sim >= crate::face::RECOGNITION_THRESH {
                let conn = state.db.lock().map_err(|_| "db lock")?;
                db::assign_face_to_person(&conn, *face_id, Some(pid)).ok();
                // Add person name as a tag on the photo
                let tag = person_name.to_lowercase();
                db::insert_tags(
                    &conn,
                    *photo_id,
                    &[(tag, sim as f64, "face".to_string())],
                )
                .ok();
                matched += 1;
                app_handle
                    .emit(
                        "face-recognized",
                        serde_json::json!({
                            "face_id": face_id,
                            "photo_id": photo_id,
                            "person": person_name,
                            "similarity": sim
                        }),
                    )
                    .ok();
            }
        }
    }

    Ok(matched)
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
#[tauri::command]
pub async fn check_face_models(app: tauri::AppHandle) -> Result<bool, String> {
    let models_dir = models_dir_for(&app);
    Ok(models_dir.join("det_500m.onnx").exists()
        && models_dir.join("w600k_mbf.onnx").exists())
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
                let like_pat = format!("{}%", &folder);
                let mut stmt = conn.prepare(
                    "SELECT id, path FROM photos WHERE folder = ?1 OR path LIKE ?2"
                ).map_err(|e| e.to_string())?;
                let result: Vec<(i64, String)> = stmt.query_map(rusqlite::params![&folder, &like_pat], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
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

    // Run detection + embedding in blocking thread
    let cluster_results = tokio::task::spawn_blocking(move || -> Result<Vec<FaceClusterResult>, String> {
        let face_models = crate::face::load_models(&models_dir).map_err(|e| e.to_string())?;
        let faces_dir = thumbs_dir.join("faces");
        std::fs::create_dir_all(&faces_dir).ok();

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

            // Detect new faces
            let img = match image::open(path) {
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

/// Silently detect faces in all photos that don't already have face data.
/// Called automatically after tagging completes (if face models are present).
/// Returns the number of NEW faces detected.
#[tauri::command]
pub async fn detect_faces_background(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<usize, String> {
    let models_dir = models_dir_for(&app);

    // Silently skip if face models are missing — not an error
    if !models_dir.join("det_500m.onnx").exists() || !models_dir.join("w600k_mbf.onnx").exists() {
        return Ok(0);
    }

    let thumbs_dir = state.thumbnails_dir.clone();
    let db_arc = state.db.clone();

    // Collect all photos that don't have face detections yet
    let photos: Vec<(i64, String)> = {
        let conn = db_arc.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn
            .prepare(
                "SELECT id, path FROM photos WHERE id NOT IN (SELECT DISTINCT photo_id FROM face_regions)"
            )
            .map_err(|e| e.to_string())?;
        let rows: Vec<(i64, String)> = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    if photos.is_empty() {
        return Ok(0);
    }

    tokio::task::spawn_blocking(move || -> Result<usize, String> {
        let face_models = crate::face::load_models(&models_dir).map_err(|e| e.to_string())?;
        let faces_dir = thumbs_dir.join("faces");
        std::fs::create_dir_all(&faces_dir).ok();

        let mut total_detected = 0usize;

        for (photo_id, path) in &photos {
            let img = match image::open(path) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let detected = match crate::face::detect_faces(&face_models, &img) {
                Ok(d) => d,
                Err(_) => continue,
            };

            for face in &detected {
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
                crop.save_with_format(
                    faces_dir.join(format!("face_{}.jpg", face_id)),
                    image::ImageFormat::Jpeg,
                ).ok();

                total_detected += 1;
            }
        }

        Ok(total_detected)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r)
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

    // Add person name as a tag to all involved photos
    let tag = if let Some(ref name) = person_name {
        name.trim().to_lowercase()
    } else {
        // Look up name from DB
        conn.query_row(
            "SELECT name FROM persons WHERE id = ?1",
            rusqlite::params![pid],
            |r| r.get::<_, String>(0),
        ).unwrap_or_default().to_lowercase()
    };

    let unique_photo_ids: std::collections::HashSet<i64> = photo_ids.into_iter().collect();
    if !tag.is_empty() {
        for photo_id in unique_photo_ids {
            db::insert_tags(&conn, photo_id, &[(tag.clone(), 1.0, "face".to_string())]).ok();
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

    let results = tokio::task::spawn_blocking(move || -> Result<Vec<PhotoSummary>, String> {
        let mut engine = crate::clip::load_engine(&base, tier).map_err(|e| e.to_string())?;
        let query_emb = crate::clip::encode_text(&mut engine, &query_owned).map_err(|e| e.to_string())?;

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
