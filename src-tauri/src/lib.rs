use std::sync::{
    atomic::AtomicBool,
    Arc, Mutex,
};

use tauri::Manager;

mod clip;
mod clip_tokenizer;
mod commands;
mod db;
mod export;
mod exif_reader;
mod face;
mod models;
mod providers;
mod router;
mod scanner;
mod tagger;
mod thumbnail;
mod watcher;
mod xmp;

pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub thumbnails_dir: std::path::PathBuf,
    pub scan_running: Arc<AtomicBool>,
    pub scan_stop: Arc<AtomicBool>,
    pub tag_running: Arc<AtomicBool>,
    pub tag_stop: Arc<AtomicBool>,
    pub watcher: Mutex<Option<watcher::FolderWatcher>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            let app_data_dir = app
                .path()
                .app_data_dir()
                .expect("Failed to resolve app data dir");
            std::fs::create_dir_all(&app_data_dir)?;

            let thumbnails_dir = app_data_dir.join("thumbnails");
            std::fs::create_dir_all(&thumbnails_dir)?;

            let db_path = app_data_dir.join("retina.db");
            let conn = db::init_db(db_path.to_str().unwrap())
                .expect("Failed to initialize SQLite database");

            app.manage(AppState {
                db: Arc::new(Mutex::new(conn)),
                thumbnails_dir,
                scan_running: Arc::new(AtomicBool::new(false)),
                scan_stop: Arc::new(AtomicBool::new(false)),
                tag_running: Arc::new(AtomicBool::new(false)),
                tag_stop: Arc::new(AtomicBool::new(false)),
                watcher: Mutex::new(None),
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // Original commands
            commands::open_folder_dialog,
            commands::scan_folder,
            commands::stop_scan,
            commands::get_folders,
            commands::get_photos,
            commands::get_photo_detail,
            commands::search_photos,
            commands::get_stats,
            commands::get_thumbnail,
            commands::start_tagging,
            commands::stop_tagging,
            commands::add_tag,
            commands::remove_tag,
            commands::get_settings,
            commands::save_setting,
            commands::get_provider_statuses,
            commands::get_all_tags,
            commands::check_ollama,
            // 1. XMP sidecar
            commands::write_xmp_for_photo,
            commands::write_xmp_all,
            // 2. Export
            commands::export_data,
            // 3. Drag & drop
            commands::scan_dropped_paths,
            // 4. Watch folders
            commands::add_watch_folder,
            commands::remove_watch_folder,
            commands::get_watch_folders,
            commands::start_watching,
            commands::stop_watching,
            // 5. Tag management
            commands::merge_tags,
            commands::rename_tag_global,
            commands::delete_tag_global,
            commands::get_tag_details,
            // 6. Collections
            commands::create_collection,
            commands::delete_collection,
            commands::get_collections,
            commands::add_to_collection,
            commands::remove_from_collection,
            commands::get_smart_collection_photos,
            // 7. EXIF / GPS
            commands::get_photo_exif,
            commands::get_gps_photos,
            commands::extract_all_gps,
            // 8. Cost dashboard
            commands::get_cost_dashboard,
            // 9. Open in explorer
            commands::open_in_explorer,
            commands::open_file,
            // 10. Duplicates
            commands::compute_phashes,
            commands::get_duplicates,
            // 11. Budget
            commands::get_budget_status,
            // 12. Natural language search
            commands::natural_language_search,
            // 13. Update check
            commands::check_for_updates,
            commands::retry_failed_photos,
            commands::clear_all_tags,
            commands::retag_photo,
            commands::get_library_stats,
            commands::test_ollama_raw,
            commands::get_ollama_status,
            commands::get_local_model_presets,
            commands::start_ollama_service,
            commands::stop_ollama_service,
            commands::pull_ollama_model,
            // 14. Face Recognition
            commands::detect_faces_in_photo,
            commands::get_faces_for_photo,
            commands::create_person,
            commands::get_persons,
            commands::assign_face_to_person,
            commands::delete_person,
            commands::rename_person,
            commands::recognize_all_faces,
            commands::download_face_models,
            commands::check_face_models,
            commands::scan_and_cluster_faces,
            commands::assign_cluster_to_person,
            // 15. CLIP Semantic Search
            commands::get_clip_status,
            commands::download_clip_models,
            commands::index_clip_embeddings,
            commands::semantic_search,
            commands::get_clip_index_count,
            commands::translate_for_clip,
            commands::get_unknown_faces,
            commands::name_face_and_propagate,
            commands::detect_faces_background,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
