use std::sync::atomic::Ordering;

use tauri::Emitter;

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
        all_terms.dedup();

        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::search_photos_multi(&conn, &all_terms).map_err(|e| e.to_string())
    } else {
        // English search — use FTS directly
        let conn = state.db.lock().map_err(|_| "db lock")?;

        // Try prefix match for partial words
        let fts_query = if trimmed.contains(' ') {
            trimmed.clone()
        } else {
            format!("{}*", trimmed)
        };

        db::search_photos_fts(&conn, &fts_query).map_err(|e| e.to_string())
    }
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
        let result =
            crate::tagger::run_tagging(db_arc, stop_flag, app_handle.clone()).await;

        tag_running.store(false, Ordering::SeqCst);
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
    pub model_available: bool,
    pub available_models: Vec<String>,
    pub endpoint: String,
}

#[tauri::command]
pub async fn check_ollama(state: tauri::State<'_, AppState>) -> Result<OllamaStatus, String> {
    let endpoint = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_setting(&conn, "ollama_endpoint")
            .ok()
            .flatten()
            .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string())
    };

    let model = {
        let conn = state.db.lock().map_err(|_| "db lock")?;
        db::get_setting(&conn, "model_local")
            .ok()
            .flatten()
            .unwrap_or_else(|| AiProvider::Local.default_model().to_string())
    };

    let (running, model_available, available_models) =
        providers::check_ollama_status(&model, &endpoint).await;

    Ok(OllamaStatus {
        running,
        model_available,
        available_models,
        endpoint,
    })
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
