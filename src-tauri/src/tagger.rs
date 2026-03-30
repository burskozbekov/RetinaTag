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
    providers,
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
        let r = router.lock().unwrap();
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
        let conn = db_conn.lock().unwrap();
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

    // Concurrency: scale with number of providers (each has its own rate limits)
    let concurrency = (provider_count * 3).max(2).min(16);
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

                // Resize image for API
                let image_b64 = match tokio::task::spawn_blocking({
                    let p = photo_path.clone();
                    move || thumbnail::prepare_for_api(&p)
                })
                .await
                {
                    Ok(Ok(b)) => b,
                    _ => {
                        fail.fetch_add(1, Ordering::Relaxed);
                        let conn = db.lock().unwrap();
                        db::update_photo_status(&conn, photo_id, "error").ok();
                        return;
                    }
                };

                // Get route from smart router
                let route = {
                    let mut r = router.lock().unwrap();
                    r.next_route()
                };

                let route = match route {
                    Some(r) => r,
                    None => {
                        fail.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                };

                let mut current_provider = route.provider;
                let mut current_key = route.api_key;
                let mut current_model = route.model;
                let mut attempt = 0;
                let max_attempts = 3;

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
                        Ok(tags) if !tags.is_empty() => {
                            let cost = current_provider.cost_per_image();
                            let provider_name = current_provider.key_name().to_string();

                            let tag_tuples: Vec<(String, f64, String)> = tags
                                .into_iter()
                                .map(|t| (t, 1.0, provider_name.clone()))
                                .collect();

                            {
                                let conn = db.lock().unwrap();
                                db::insert_tags(&conn, photo_id, &tag_tuples).ok();
                                db::update_photo_status(&conn, photo_id, "tagged").ok();
                                db::update_photo_provider(&conn, photo_id, &provider_name)
                                    .ok();
                                db::record_usage(&conn, &provider_name, photo_id, true, cost)
                                    .ok();
                            }

                            {
                                let mut r = router.lock().unwrap();
                                r.report_success(current_provider);
                            }

                            {
                                let mut bd = bd.lock().unwrap();
                                let entry = bd.entry(provider_name).or_insert((0, 0.0));
                                entry.0 += 1;
                                entry.1 += cost;
                            }

                            done.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                        Ok(_) | Err(_) => {
                            // Record failure
                            {
                                let conn = db.lock().unwrap();
                                db::record_usage(
                                    &conn,
                                    current_provider.key_name(),
                                    photo_id,
                                    false,
                                    0.0,
                                )
                                .ok();
                            }

                            if attempt >= max_attempts {
                                let conn = db.lock().unwrap();
                                db::update_photo_status(&conn, photo_id, "error").ok();
                                fail.fetch_add(1, Ordering::Relaxed);
                                break;
                            }

                            // Try fallback provider
                            let fallback = {
                                let mut r = router.lock().unwrap();
                                r.fallback_route(current_provider)
                            };

                            match fallback {
                                Some(fb) => {
                                    current_provider = fb.provider;
                                    current_key = fb.api_key;
                                    current_model = fb.model;
                                }
                                None => {
                                    let conn = db.lock().unwrap();
                                    db::update_photo_status(&conn, photo_id, "error").ok();
                                    fail.fetch_add(1, Ordering::Relaxed);
                                    break;
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
    let bd = breakdown.lock().unwrap();
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
