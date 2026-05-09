use futures::future::join_all;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use tauri::Emitter;
use tokio::sync::Semaphore;

use crate::{
    db,
    models::*,
    providers::{self, ApiErrorKind},
    router::SmartRouter,
    thumbnail,
};

pub async fn run_tagging(
    db_conn: Arc<Mutex<rusqlite::Connection>>,
    stop_flag: Arc<AtomicBool>,
    app_handle: tauri::AppHandle,
) -> TagComplete {
    // Build the router from current settings
    let router = Arc::new(Mutex::new(SmartRouter::new(&db_conn)));

    let provider_count = {
        let r = router.lock().unwrap_or_else(|e| e.into_inner());
        r.provider_count()
    };

    if provider_count == 0 {
        app_handle
            .emit(
                "tag-error",
                "No AI providers configured. Add at least one API key in Settings.",
            )
            .ok();
        return TagComplete {
            tagged: 0,
            failed: 0,
            provider_breakdown: vec![],
        };
    }

    // Load all pending photos
    let pending: Vec<(i64, String)> = {
        let conn = db_conn.lock().unwrap_or_else(|e| e.into_inner());
        db::get_pending_photos(&conn).unwrap_or_default()
    };

    let total = pending.len();
    if total == 0 {
        return TagComplete {
            tagged: 0,
            failed: 0,
            provider_breakdown: vec![],
        };
    }

    let completed = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));

    // Concurrency: local Ollama can only handle 1 at a time (serial processing).
    // Cloud APIs can run in parallel. Check if the only provider is Local.
    let is_local_only = {
        let r = router.lock().unwrap_or_else(|e| e.into_inner());
        r.is_local_only()
    };
    let is_gemini_only = {
        let r = router.lock().unwrap_or_else(|e| e.into_inner());
        r.is_gemini_only()
    };
    // Gemini free tier: ~15 RPM → serial (1 at a time) to avoid 429 spam
    let concurrency = if is_local_only || is_gemini_only { 1 } else { (provider_count * 3).max(2).min(16) };
    let sem = Arc::new(Semaphore::new(concurrency));

    // Provider breakdown tracker
    let breakdown: Arc<Mutex<std::collections::HashMap<String, (usize, f64)>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));

    let tasks: Vec<_> = pending
        .into_iter()
        .map(|(photo_id, photo_path)| {
            let sem = sem.clone();
            let db = db_conn.clone();
            let router = router.clone();
            let stop = stop_flag.clone();
            let done = completed.clone();
            let fail = failed.clone();
            let ah = app_handle.clone();
            let bd = breakdown.clone();

            tokio::spawn(async move {
                if stop.load(Ordering::Relaxed) {
                    return;
                }

                let _permit = sem.acquire().await.unwrap();

                if stop.load(Ordering::Relaxed) {
                    return;
                }

                let filename = std::path::Path::new(&photo_path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();

                // Get route from smart router first so we know if it's local
                let route = {
                    let mut r = router.lock().unwrap_or_else(|e| e.into_inner());
                    r.next_route()
                };

                let route = match route {
                    Some(r) => r,
                    None => {
                        fail.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                };

                // Resize image — smaller for local Ollama to save VRAM
                let is_local = route.provider == AiProvider::Local;
                let image_b64 = match tokio::task::spawn_blocking({
                    let p = photo_path.clone();
                    move || {
                        if is_local {
                            thumbnail::prepare_for_api_local(&p)
                        } else {
                            thumbnail::prepare_for_api(&p)
                        }
                    }
                })
                .await
                {
                    Ok(Ok(b)) => b,
                    Ok(Err(e)) => {
                        ah.emit("tag-provider-error", serde_json::json!({
                            "provider": route.provider.name(),
                            "file": &filename,
                            "error": format!("Image could not be opened: {}", e),
                            "kind": "ImagePrep"
                        })).ok();
                        fail.fetch_add(1, Ordering::Relaxed);
                        let conn = db.lock().unwrap_or_else(|e| e.into_inner());
                        db::update_photo_status(&conn, photo_id, "error").ok();
                        return;
                    }
                    Err(e) => {
                        ah.emit("tag-provider-error", serde_json::json!({
                            "provider": route.provider.name(),
                            "file": &filename,
                            "error": format!("Image processing thread error: {}", e),
                            "kind": "ImagePrep"
                        })).ok();
                        fail.fetch_add(1, Ordering::Relaxed);
                        let conn = db.lock().unwrap_or_else(|e| e.into_inner());
                        db::update_photo_status(&conn, photo_id, "error").ok();
                        return;
                    }
                };

                let mut current_provider = route.provider;
                let mut current_key = route.api_key;
                let mut current_model = route.model;
                let mut attempt = 0;
                const MAX_ATTEMPTS: usize = 4;

                loop {
                    attempt += 1;

                    // Emit progress
                    ah.emit(
                        "tag-progress",
                        TagProgress {
                            total,
                            completed: done.load(Ordering::Relaxed),
                            failed: fail.load(Ordering::Relaxed),
                            current_file: filename.clone(),
                            current_provider: current_provider.name().to_string(),
                            is_running: true,
                        },
                    )
                    .ok();

                    let result = providers::call_provider(
                        current_provider,
                        &image_b64,
                        &current_key,
                        &current_model,
                    )
                    .await;

                    match result {
                        Ok((tags, description, location)) if !tags.is_empty() => {
                            let cost = current_provider.cost_per_image();
                            let provider_name = current_provider.key_name().to_string();

                            let tag_tuples: Vec<(String, f64, String)> = tags
                                .into_iter()
                                .map(|t| (t, 1.0, provider_name.clone()))
                                .collect();

                            {
                                let conn = db.lock().unwrap_or_else(|e| e.into_inner());
                                db::insert_tags(&conn, photo_id, &tag_tuples).ok();
                                db::update_photo_status(&conn, photo_id, "tagged").ok();
                                db::update_photo_provider(&conn, photo_id, &provider_name).ok();
                                db::record_usage(&conn, &provider_name, photo_id, true, cost).ok();
                                if let Some(desc) = description {
                                    db::update_photo_description(&conn, photo_id, &desc).ok();
                                }
                                // Save AI-estimated location (only if no real GPS)
                                if let Some((lat, lon, name)) = location {
                                    db::update_photo_estimated_location(&conn, photo_id, lat, lon, &name).ok();
                                }
                            }
                            { router.lock().unwrap_or_else(|e| e.into_inner()).report_success(current_provider); }
                            { let mut b = bd.lock().unwrap_or_else(|e| e.into_inner()); let e = b.entry(provider_name).or_insert((0,0.0)); e.0+=1; e.1+=cost; }
                            done.fetch_add(1, Ordering::Relaxed);
                            // Gemini free tier: 15 RPM → enforce ≥5s between requests
                            if current_provider == AiProvider::Gemini {
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            }
                            break;
                        }

                        Ok(_) /* empty tags */ => {
                            // Empty tag list — retry same provider with brief wait
                            if attempt >= MAX_ATTEMPTS {
                                let conn = db.lock().unwrap_or_else(|e| e.into_inner());
                                db::update_photo_status(&conn, photo_id, "error").ok();
                                fail.fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                            // Short wait then retry (don't switch provider yet)
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }

                        Err(e) => {
                            // Classify the error
                            let kind = if let Some(api_err) = e.downcast_ref::<providers::ApiError>() {
                                api_err.kind.clone()
                            } else {
                                let msg = e.to_string().to_lowercase();
                                if msg.contains("401") || msg.contains("403") || msg.contains("unauthorized") || msg.contains("invalid api key") {
                                    ApiErrorKind::AuthFailed
                                } else if msg.contains("429") || msg.contains("rate limit") || msg.contains("too many") {
                                    ApiErrorKind::RateLimit { retry_after_secs: 60 }
                                } else if msg.contains("500") || msg.contains("502") || msg.contains("503") || msg.contains("timeout") {
                                    ApiErrorKind::Transient
                                } else {
                                    ApiErrorKind::Permanent
                                }
                            };

                            // Emit error details to frontend
                            ah.emit("tag-provider-error", serde_json::json!({
                                "provider": current_provider.name(),
                                "file": &filename,
                                "error": e.to_string(),
                                "kind": format!("{:?}", kind)
                            })).ok();

                            { let conn=db.lock().unwrap_or_else(|e| e.into_inner()); db::record_usage(&conn,current_provider.key_name(),photo_id,false,0.0).ok(); }

                            match kind {
                                ApiErrorKind::AuthFailed => {
                                    // API key invalid — disable this provider for the session
                                    router.lock().unwrap_or_else(|e| e.into_inner()).disable_provider(current_provider);
                                    ah.emit("tag-auth-error", serde_json::json!({
                                        "provider": current_provider.name(),
                                        "message": format!("{} API key is invalid. Please check Settings.", current_provider.name())
                                    })).ok();
                                    // Try another provider?
                                    match router.lock().unwrap_or_else(|e| e.into_inner()).fallback_route(current_provider) {
                                        Some(fb) => { current_provider=fb.provider; current_key=fb.api_key; current_model=fb.model; }
                                        None => { let conn=db.lock().unwrap_or_else(|e| e.into_inner()); db::update_photo_status(&conn,photo_id,"error").ok(); fail.fetch_add(1,Ordering::Relaxed); break; }
                                    }
                                }
                                ApiErrorKind::RateLimit { retry_after_secs } => {
                                    // Rate limit — wait and retry with the same provider
                                    ah.emit("tag-rate-limit", serde_json::json!({
                                        "provider": current_provider.name(),
                                        "wait_secs": retry_after_secs
                                    })).ok();
                                    tokio::time::sleep(std::time::Duration::from_secs(retry_after_secs)).await;
                                    attempt -= 1; // Don't count this attempt
                                }
                                ApiErrorKind::Transient => {
                                    // Transient error — short backoff
                                    if attempt >= MAX_ATTEMPTS {
                                        let conn=db.lock().unwrap_or_else(|e| e.into_inner()); db::update_photo_status(&conn,photo_id,"error").ok(); fail.fetch_add(1,Ordering::Relaxed); break;
                                    }
                                    let wait = std::time::Duration::from_millis(500 * 2u64.pow(attempt as u32 - 1));
                                    tokio::time::sleep(wait).await;
                                }
                                ApiErrorKind::Permanent => {
                                    if attempt >= MAX_ATTEMPTS {
                                        let conn=db.lock().unwrap_or_else(|e| e.into_inner()); db::update_photo_status(&conn,photo_id,"error").ok(); fail.fetch_add(1,Ordering::Relaxed); break;
                                    }
                                    match router.lock().unwrap_or_else(|e| e.into_inner()).fallback_route(current_provider) {
                                        Some(fb) => { current_provider=fb.provider; current_key=fb.api_key; current_model=fb.model; }
                                        None => { let conn=db.lock().unwrap_or_else(|e| e.into_inner()); db::update_photo_status(&conn,photo_id,"error").ok(); fail.fetch_add(1,Ordering::Relaxed); break; }
                                    }
                                }
                            }
                        }
                    }
                }
            })
        })
        .collect();

    join_all(tasks).await;

    // Build breakdown
    let bd = breakdown.lock().unwrap_or_else(|e| e.into_inner());
    let provider_breakdown: Vec<ProviderBreakdown> = bd
        .iter()
        .map(|(name, (count, cost))| ProviderBreakdown {
            provider: name.clone(),
            count: *count,
            cost_usd: *cost,
        })
        .collect();

    TagComplete {
        tagged: completed.load(Ordering::Relaxed),
        failed: failed.load(Ordering::Relaxed),
        provider_breakdown,
    }
}
