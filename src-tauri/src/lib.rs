// v1.5.75 — Audit cleanup cycle. The P0/P1 work shipped in 1.5.73/1.5.74
// covered the catastrophic stuff; this pass closes the long tail of P2
// gotchas the audit surfaced.
//
// Backend
// • watcher.rs callback: lock with unwrap_or_else(into_inner) instead of
//   .unwrap() so a poisoned mutex from a sibling thread can't kill the
//   watcher callback silently (the file-watching feature used to die
//   for the rest of the session with no log line).
// • auto_hide_nsfw: bounds-check prompt_embs[0] before indexing — a
//   total CLIP encode failure (model missing / GPU OOM) used to panic
//   inside spawn_blocking and surface as a generic IPC error.
// • Delete-to-recycle: PowerShell stderr / spawn failure is now logged
//   to eprintln instead of being swallowed by `let _ = .output()`.
//   The DB row + thumbnail are still cleaned up (the user asked for
//   delete) but support can see why files might still be on disk.
// • fix_health_issues: also wipes the cached thumbnail and any .rtenc
//   blob for each orphan photo it deletes. The thumbnails dir used to
//   grow unboundedly over a "library shrink" cycle.
// • scanner.rs: skip walker entries whose `file_name()` is empty
//   (drive roots like `D:\`). They used to land in the DB with
//   `filename = ""` and silently break XMP write / rename UI.
//
// Frontend
// • _cuShowDeleteModal: keydown listener detached on every close path,
//   not just Escape. Was a slow-burn leak — each Cleanup-Delete cycle
//   added a permanent `document` listener.
// • Face-popup polls: 60-second hard cap on the setInterval that waits
//   for #faceNameInput to unmount. Three call sites used to spin
//   forever if the popup never mounted (showFacePopup throw, re-render
//   race).
// • Gallery shortcuts: input-focus guard. Typing into the description
//   editor / filename rename / search box no longer triggers gallery
//   shortcuts (p/n/g/s/r/f/x/[/]/0-5/etc.). Modifier chords (Ctrl+S,
//   Ctrl+Z, Ctrl+K, Cmd+K) still pass through.
// • lbNav: drops the previous photo's <img src> before loading the
//   next so the WebView doesn't hold base64 data URLs from
//   get_private_photo_data alive across vault-lightbox navigation
//   (was ~5-20 MB per photo, easily 1+ GB after a long browse).
//
// v1.5.74 — P1 bug-fix cycle. Eliminates the freeze + data-integrity
// classes the v1.5.73 audit surfaced.
//
// Backend
// • recognize_all_faces: now wrapped in spawn_blocking. The cosine-sim
//   fan-out plus per-match db.lock() + insert_tags + emit used to run
//   inline on the async-runtime worker thread; on a 50K-photo / 5K-face
//   library this took minutes and froze every other IPC.
// • Watcher.process_new_files: no longer holds the db mutex across slow
//   I/O (image_dimensions, EXIF read, ffprobe, thumbnail generation).
//   Previously each watched-file import wedged every DB command for
//   1–3 s. Now the lock is taken only around the dedup query and the
//   final insert.
// • apply_rename: wrapped in a single transaction and rolls back the
//   on-disk rename if the DB update fails (per-row), and rolls back
//   every applied rename if the transaction commit fails. Eliminates
//   the "photo disappeared from gallery" outcome of a half-applied
//   batch rename.
// • vault_files::decrypt_to_bytes: was silently dropping
//   `read_to_end` errors and surfacing them as "vault corrupt / auth
//   tag mismatch", panicking users into running recovery. Now the
//   disk error is propagated verbatim.
//
// Frontend
// • Search pipeline: monotonic _searchSeq guard inserted after every
//   await. Fast typing in a 60K library used to render results for a
//   query the user had already typed past; now stale resolutions bail
//   out before mutating photos / curSearch.
// • toast / toastAction: escape the message + label before innerHTML
//   interpolation. Backend errors with `<` or `&` in them no longer
//   break the toast render.
//
// v1.5.73 — Critical bug-fix release. Commercial sale hardening.
//
// Backend
// • Single-instance plugin (tauri-plugin-single-instance): second
//   launches now activate the existing window instead of spawning a
//   parallel process that fought over the same SQLite WAL and ONNX
//   sessions.
// • Vault P0 leak fixes — every code path that flipped `private = 1`
//   on a photo now routes through `move_photo_private`, which encrypts
//   the original file + thumbnail and rolls back the on-disk rename if
//   the DB write fails. Patched call sites:
//     - batch_set_private (was a flag-flip only, plaintext stayed
//       readable in Explorer)
//     - toggle_photo_private (used to silently no-op encryption when
//       the vault was locked but still wrote private=1; now hard-fails
//       so the user unlocks first)
//     - auto_hide_nsfw (used to call set_photo_private(true) inline)
//
// Frontend
// • XSS / render-corruption hardening: filenames, tags, descriptions,
//   and meta fields are now passed through _esc before innerHTML
//   interpolation. A file named "cat<3.jpg" or a user-added tag
//   containing "&" no longer breaks the card grid.
// • selectPhoto: guards against await race that overwrote a newer
//   selection's detail panel with the previous click's resolved data.
// • _loadDescriptionFor: auto-saves the previous photo's dirty
//   description instead of silently dropping it when the user clicks
//   another photo mid-edit.
// • created_at null guard in detail panel render — restored backups
//   with missing timestamps no longer crash the whole panel.
//
// v1.5.72 — Startup-freeze fix on large libraries.
// • Frontend init: gallery loads first; sidebar widgets fire sequentially
//   on setTimeout(0) microtasks instead of Promise.all, so UI clicks
//   register immediately on launch.
// • Backend: get_photos / get_photo_detail / get_stats / get_folders /
//   get_folders_with_status / get_collections wrap their work in
//   tauri::async_runtime::spawn_blocking, freeing the async-runtime
//   worker pool for IPC dispatch while the DB query runs on the
//   blocking pool. On a 60K+ photo library the WebView used to appear
//   frozen for 5–10s on launch as parallel state.db.lock() calls
//   saturated the worker pool — this restores responsive launch.
// v1.5.71 — Search quality overhaul + remove Trending Tags:
// • English multi-word queries now use stop-word filtering + per-concept
//   group-AND logic. "couple on the boat" strips "on"/"the", then intersects
//   (couple-synonyms) ∩ (boat-synonyms) instead of ORing 16+ terms.
// • Path/description search no longer passes stop words (fixes "on*" and
//   "the*" prefix matches polluting filename/folder results).
// • Description search uses content words only, not full synonym expansion.
// • Turkish vehicle terms (tekne, gemi, yat, uçak, tren…) added to the
//   contextual dictionary with disambiguation context for re-ranking.
// • Removed Trending Tags panel — user-requested removal.
// v1.5.70 — Lightbox + search readability:
// • Lightbox tools moved to a vertical right-edge toolbar so they no
//   longer float on top of the photo. Image width clamped to
//   calc(95vw - 80px) to leave the toolbar a clear lane.
// • Default cursor (no zoom-out) so the viewport doesn't visually
//   suggest "click anywhere to zoom" while you're just looking.
// • Removed the top-left ▶ slideshow tap target — it confused users
//   who weren't looking for a slideshow. Keyboard S still toggles.
// • Search hint dropped the "x" → "y" arrow when an auto-translation
//   ran. Translation now lives in a hover tooltip; the badge just
//   shows the count, leaving the search bar quiet.
// v1.5.69 — UI: kill stray horizontal scrollbars. .modal-box now has
// overflow-x:hidden so flex rows that almost-fit don't trigger a
// horizontal scroll track at the bottom of the modal. Defensive
// max-width:100% on form controls so a too-long select can't push the
// row past the modal width.
// v1.5.68 — Portable vault (deterministic KEK from BIP39 mnemonic).
// Earlier versions used a per-vault random KEK, which meant a vault
// set up on Windows couldn't be opened on Mac (or after a reinstall)
// even with the same recovery phrase. Now KEK = Argon2id(mnemonic,
// fixed_app_salt) — same 24 words → same KEK on any device. PIN stays
// as the fast local unlock. Adds vault_restore_from_mnemonic command,
// kek_version column for migration detection, and a one-time re-key
// pass that decrypts every .rtenc + thumb blob with the old random
// KEK and re-encrypts under the new deterministic KEK on the next
// unlock of any v1.5.64-67 vault.
// v1.5.67 — Vault hang fix. Several vault commands were sync and ran
// Argon2id (~250 ms-1 s) or WinRT UserConsentVerifier on the IPC
// thread, which froze the WebView ("Not Responding"). Now async +
// spawn_blocking: vault_unlock, vault_set_pin_with_recovery,
// vault_biometric_status, vault_biometric_enroll, vault_biometric_unlock,
// toggle_photo_private, get_private_photo_data.
// v1.5.66 — REAL file-level vault encryption. Up to v1.5.65 the
// "vault" only AES-encrypted thumbnails; the original photo bytes were
// still readable from Windows Explorer. Now flipping a photo into the
// vault encrypts the actual file in place to `<path>.rtenc`. Migration
// for existing vaults is opt-in (consent modal because losing PIN +
// recovery phrase = losing the files). New vault_files.rs module
// handles atomic encrypt/decrypt; new lightbox decrypts on-the-fly to
// a base64 data URL so plaintext bytes never re-touch disk.
// v1.5.65 — Vault tab clobber fix: the inline onclick was being
// overwritten by the view-tab click handler, so clicking Vault hid the
// gallery without opening the modal. Also un-hardcoded the sidebar
// version label that read "v1.3" since the dawn of time.
// v1.5.64 — Vault Faz 2.1 (AES-256 thumbnail encryption + PIN-derived
// KEK + auto-upgrade for legacy vaults) and Faz 2.3 (Windows Hello
// biometric unlock via UserConsentVerifier + DPAPI-wrapped KEK).
// Touching this comment forces `tauri::generate_context!()` to re-embed
// `../dist/index.html`, otherwise proc-macro fingerprint caching skips
// the rebuild and the bundled binary keeps the stale frontend (the
// v1.5.60-62 problem). Build hash bump lives here, not in build.rs,
// because `cargo:rerun-if-changed` doesn't reach inside the proc-macro
// that does the actual bundling.
use std::sync::{
    atomic::AtomicBool,
    Arc, Mutex,
};

use tauri::{Emitter, Manager};

mod clip;
mod clip_tokenizer;
mod commands;
mod db;
mod export;
mod exif_reader;
mod face;
mod models;
mod vault_crypto;
mod vault_biometric;
mod vault_files;
mod providers;
mod quality;
mod router;
mod scanner;
mod tagger;
mod thumbnail;
mod watcher;
mod xmp;
mod device_monitor;
mod tray;
#[cfg(windows)]
mod mtp;

pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub thumbnails_dir: std::path::PathBuf,
    pub scan_running: Arc<AtomicBool>,
    pub scan_stop: Arc<AtomicBool>,
    pub tag_running: Arc<AtomicBool>,
    pub tag_stop: Arc<AtomicBool>,
    /// Set to true when a face scan is running. Used to show status in UI.
    pub face_running: Arc<AtomicBool>,
    /// Set to true to cooperatively stop the current face scan.
    /// Checked inside `detect_faces_background` and `recognize_all_faces`.
    pub face_stop: Arc<AtomicBool>,
    pub watcher: Mutex<Option<watcher::FolderWatcher>>,
    pub device_monitor: Mutex<Option<device_monitor::DeviceMonitor>>,
    /// Face IDs returned by the last get_unknown_faces call.
    /// On the NEXT call, any still-unassigned faces here are auto-skipped.
    pub last_shown_face_ids: Mutex<Vec<i64>>,
    /// v1.5.64 — Faz 2.1: in-memory KEK after `vault_unlock` succeeds.
    /// Populated only while the vault is unlocked; cleared on lock or
    /// when the auto-lock timer fires. Keeping it on the heap inside a
    /// Mutex<Option> means it never lands on disk and is zero-padded on
    /// drop. Thumbnails are only decryptable while this is `Some(_)`.
    pub vault_kek: Mutex<Option<[u8; 32]>>,
    /// v1.5.155 — Plaintext temp files we wrote so the lightbox could play
    /// vault videos via WebView2's blob/file source. WebView2 can't stream
    /// from a `.rtenc` directly, so `vault_decrypt_to_temp` materialises
    /// each requested video at `%LOCALAPPDATA%\com.retinatag.app\vault-temp\
    /// rt_vault_<id>_<rand>.<ext>` and returns the path. We keep the paths
    /// here so `vault_lock` can shred them — otherwise an unlocked-and-then-
    /// locked vault would leave plaintext videos on disk until the next
    /// vacuum run, defeating the whole point of the vault.
    pub vault_temp_files: Arc<Mutex<Vec<std::path::PathBuf>>>,
    /// v1.5.164 — IDs of vault folders currently revealed to Explorer
    /// via `vault_decrypt_folder_to_explorer`. `vault_relock_folder` pops
    /// its id on success; `vault_lock` drains the whole list and walks
    /// each to shred the plaintext mirror BEFORE clearing the KEK, so
    /// "locked" really means locked (no plaintext on disk) regardless
    /// of whether the user clicked 🔒 themselves or the auto-lock timer
    /// fired while they were browsing the reveal in Explorer.
    pub revealed_folders: Arc<Mutex<Vec<i64>>>,
    /// v1.5.173 — Central vault-store directory at
    /// `%LOCALAPPDATA%\com.retinatag.app\vault-store\`. Every new
    /// .rtenc blob produced by vault_add_paths lands here instead of
    /// next to the user's plaintext source. Without this, encrypting
    /// `C:\Users\foo\Desktop\VAULT\` left `VAULT\bugra.jpg.rtenc` and
    /// friends sitting in plain sight on the desktop — the folder
    /// stayed populated (with sealed blobs), v1.5.157's empty-dir
    /// cleanup skipped it, and the user saw their "vault" still
    /// present in Explorer with weird `.rtenc` extensions. Moving the
    /// blobs to this central store leaves the desktop folder empty,
    /// v1.5.157 then removes it, and the vault becomes truly invisible
    /// to Explorer.
    pub vault_store_dir: std::path::PathBuf,
}

/// Suppress Windows "The application was unable to start correctly (0xc0000142)"
/// and similar modal error dialogs from child processes.
///
/// We shell out to `ffmpeg.exe` / `ffprobe.exe` for video thumbnails and
/// duration. On machines with a broken install (missing VC++ runtime, or
/// another binary named `ffmpeg.exe` on PATH — e.g. a stale .NET wrapper)
/// Windows shows a kernel-level modal dialog the moment the child image
/// fails to load. Those dialogs freeze the whole UI and the user has to
/// click OK for every single video.
///
/// `SetErrorMode` on our process suppresses those dialogs; since Windows
/// 7 SP1 the error mode is inherited by child processes, so ffmpeg/ffprobe
/// launched from us will also stay silent and just return a non-zero exit
/// code that our code already handles.
#[cfg(target_os = "windows")]
fn suppress_windows_error_dialogs() {
    const SEM_FAILCRITICALERRORS: u32 = 0x0001;
    const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
    const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;
    extern "system" {
        fn SetErrorMode(u_mode: u32) -> u32;
    }
    unsafe {
        SetErrorMode(
            SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn suppress_windows_error_dialogs() {}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    suppress_windows_error_dialogs();
    tauri::Builder::default()
        // v1.5.73 — single-instance guard. Second launches (Start menu,
        // tray double-click, file-association open) hand their argv off to
        // this primary instance instead of spawning a parallel process,
        // which used to result in two RetinaTag windows fighting over the
        // same SQLite DB (.wal race) and two CLIP/Ollama subprocess pools.
        // The closure runs on the primary; we just unminimise + focus.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            use tauri::Manager;
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_drag::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            // Prefer the Roaming AppData path (app_config_dir on Windows ==
            // %APPDATA%\<identifier>) — that's where pre-1.3 builds wrote the
            // library, so an existing 200 MB DB is sitting there untouched.
            // Fall back to the new Local path only if nothing is in Roaming
            // (i.e. a clean first install on a brand new machine).
            //
            // WHY THIS MATTERS: Tauri 2's `app_data_dir()` resolves to
            // %LOCALAPPDATA% on Windows, but earlier versions of this app
            // ended up writing to %APPDATA%\Roaming. Blindly calling
            // `app_data_dir()` on an upgraded install silently points at an
            // empty new DB while the user's real library (49 k photos,
            // persons, tags, faces) still lives in Roaming — the app looks
            // like it lost everything. Checking Roaming first fixes that.
            let roaming_dir = app.path().app_config_dir().ok();
            let local_dir = app.path().app_data_dir().ok();

            let app_data_dir = {
                let roaming_has_db = roaming_dir.as_ref().is_some_and(|d| {
                    let db = d.join("retina.db");
                    std::fs::metadata(&db).map(|m| m.len() > 1024).unwrap_or(false)
                });
                if roaming_has_db {
                    roaming_dir.clone().unwrap()
                } else {
                    // No existing Roaming DB — use whichever path is available.
                    // Prefer Roaming for new installs too, to stay consistent
                    // with earlier versions and make future upgrades simple.
                    roaming_dir
                        .or(local_dir)
                        .expect("Failed to resolve any app data dir")
                }
            };
            eprintln!("[init] app_data_dir = {}", app_data_dir.display());
            std::fs::create_dir_all(&app_data_dir)?;

            let thumbnails_dir = app_data_dir.join("thumbnails");
            std::fs::create_dir_all(&thumbnails_dir)?;

            let db_path = app_data_dir.join("retina.db");
            let conn = db::init_db(db_path.to_str().unwrap())
                .expect("Failed to initialize SQLite database");

            // Restore persisted tag-language setting (survives across restarts).
            if let Ok(Some(lang)) = db::get_setting(&conn, "tag_language") {
                let code = if lang == "tr" { 1u8 } else { 0u8 };
                providers::TAG_LANG.store(code, std::sync::atomic::Ordering::Relaxed);
            }

            // Read "start minimized to tray" preference before we install the
            // tray icon so we can hide the window after it's shown. We stash
            // the result in a local so the setup closure can use it below.
            let start_minimized = db::get_setting(&conn, "start_minimized")
                .ok()
                .flatten()
                .map(|s| s == "1")
                .unwrap_or(false);
            let close_to_tray = db::get_setting(&conn, "close_to_tray")
                .ok()
                .flatten()
                .map(|s| s == "1")
                .unwrap_or(false);

            // v1.5.156 — Wipe leftover plaintext temp files from a previous
            // session before any window can fire vault_unlock. `vault_decrypt_to_temp`
            // materialises vault videos under `%LOCALAPPDATA%\com.retinatag.app\
            // vault-temp\` so WebView2 can play them; `vault_lock` shreds those
            // on manual lock and on the auto-lock timer. But a crash, a force-quit,
            // a Task-Manager kill, or even closing the main window while the vault
            // was unlocked all bypass `vault_lock` — meaning yesterday's plaintext
            // videos would still be on disk on next launch and Explorer would
            // happily open them without ever touching the PIN. We do the wipe
            // here (right after we know we have a local data dir but before we
            // expose any IPC) so there's no window where the new session's temp
            // files mix with stale ones.
            //
            // Path mirrors `vault_decrypt_to_temp`: app_local_data_dir(), NOT
            // the roaming `app_data_dir` we use for the DB. Vault videos must
            // never end up in roaming because Enterprise / Folder Redirection
            // setups sync %APPDATA% to the network, which would leak plaintext
            // to a fileserver the user never opted into.
            if let Ok(local_root) = app.path().app_local_data_dir() {
                let temp_root = local_root.join("vault-temp");
                if temp_root.exists() {
                    match std::fs::read_dir(&temp_root) {
                        Ok(rd) => {
                            let mut wiped = 0usize;
                            for entry in rd.flatten() {
                                let p = entry.path();
                                if p.is_file() {
                                    if let Err(e) = std::fs::remove_file(&p) {
                                        eprintln!("[vault-temp] remove {} failed: {}", p.display(), e);
                                    } else {
                                        wiped += 1;
                                    }
                                }
                            }
                            if wiped > 0 {
                                eprintln!("[vault-temp] wiped {} stale plaintext file(s) from previous session", wiped);
                            }
                        }
                        Err(e) => eprintln!("[vault-temp] read_dir {} failed: {}", temp_root.display(), e),
                    }
                }
            }

            // v1.5.173 — Central vault-store directory. Created up-front so
            // vault_add_paths can move freshly-sealed .rtenc blobs in here
            // without racing the directory's first-time creation. Lives under
            // LOCALAPPDATA (NOT roaming) so blobs never sync to Folder
            // Redirection / OneDrive — the user's vault has to stay local.
            let vault_store_dir = app
                .path()
                .app_local_data_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join("vault-store");
            if let Err(e) = std::fs::create_dir_all(&vault_store_dir) {
                eprintln!("[vault-store] create_dir_all {} failed: {}", vault_store_dir.display(), e);
            }

            app.manage(AppState {
                db: Arc::new(Mutex::new(conn)),
                thumbnails_dir,
                scan_running: Arc::new(AtomicBool::new(false)),
                scan_stop: Arc::new(AtomicBool::new(false)),
                tag_running: Arc::new(AtomicBool::new(false)),
                tag_stop: Arc::new(AtomicBool::new(false)),
                face_running: Arc::new(AtomicBool::new(false)),
                face_stop: Arc::new(AtomicBool::new(false)),
                watcher: Mutex::new(None),
                device_monitor: Mutex::new(None),
                last_shown_face_ids: Mutex::new(Vec::new()),
                vault_kek: Mutex::new(None),
                vault_temp_files: Arc::new(Mutex::new(Vec::new())),
                revealed_folders: Arc::new(Mutex::new(Vec::new())),
                vault_store_dir,
            });

            // Install the system tray icon + menu. Non-fatal on failure.
            let handle = app.handle().clone();
            if let Err(e) = tray::install(&handle) {
                eprintln!("[tray] install failed: {}", e);
            }

            // If the user opted into "start minimized to tray" AND close-to-tray
            // is enabled (otherwise there's no tray icon to restore from), hide
            // the main window immediately. The tray menu's "Show RetinaTag"
            // restores it. We do this only when both prefs are on so we don't
            // accidentally leave a user with no visible window and no tray.
            if start_minimized && close_to_tray {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.hide();
                }
            }

            // Parse --scan-path=<dir> CLI arg and forward it to the frontend
            // once the window is ready. Explorer context-menu entries pass the
            // clicked path this way so "Tag with RetinaTag" on a folder
            // immediately kicks off a scan on that folder.
            let args: Vec<String> = std::env::args().collect();
            let mut scan_path: Option<String> = None;
            let mut i = 0;
            while i < args.len() {
                let a = &args[i];
                if a == "--scan-path" && i + 1 < args.len() {
                    scan_path = Some(args[i + 1].clone());
                    break;
                } else if let Some(v) = a.strip_prefix("--scan-path=") {
                    scan_path = Some(v.to_string());
                    break;
                }
                i += 1;
            }
            if let Some(p) = scan_path {
                let handle2 = app.handle().clone();
                // Delay emit slightly so the frontend listener is registered.
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    let _ = handle2.emit("cli-scan-path", p);
                });
            }

            // v1.5.106 — startup XMP sidecar backfill. Triggered from
            // Rust (not JS) so it doesn't depend on a WebView ready
            // state, an unmounted listener, or a syntax issue further
            // down dist/index.html eating the setTimeout. Sleeps 8s so
            // the initial UI render + DB warm-up isn't competing for
            // the lock, then sweeps the library once. Writes a
            // single-line log to xmp_import.log next to retina.db so
            // we can verify it ran from outside the WebView.
            let db_for_xmp = app.state::<AppState>().db.clone();
            let log_dir = app_data_dir.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(8));
                let started = chrono::Utc::now();
                let log_path = log_dir.join("xmp_import.log");
                let mut log = String::new();
                use std::fmt::Write as _;
                let _ = writeln!(log, "[{}] xmp backfill: starting", started.to_rfc3339());

                // v1.5.111 — first, normalise the existing tag set.
                // Mac and Windows have historically each tagged in
                // their preferred case (face-naming caps the person
                // name; AI taggers lowercase nouns), so the library
                // ends up with `Lara` / `lara` / `Bugra` / `bugra`
                // pairs scattered across photos. Tag Manager and
                // any free-text search treat these as separate
                // entries. Collapse to one canonical spelling per
                // tag before the XMP backfill runs (so new imports
                // can also case-match against the merged form).
                if let Ok(conn) = db_for_xmp.lock() {
                    match db::normalize_tag_case(&conn) {
                        Ok((groups, renamed, deleted)) => {
                            let _ = writeln!(
                                log,
                                "  normalize_tag_case: merged {} groups (renamed {} rows, deleted {} conflicts)",
                                groups, renamed, deleted
                            );
                        }
                        Err(e) => {
                            let _ = writeln!(log, "  normalize_tag_case error: {}", e);
                        }
                    }
                    // v1.5.152 — One-shot fix for WPD's slash-separated
                    // date_taken values that pre-1.5.152 MTP imports
                    // wrote raw into the DB. Broke the timeline year
                    // dial. Idempotent: rows already in ISO are skipped
                    // by the WHERE clauses.
                    match db::normalize_date_taken_format(&conn) {
                        Ok(n) => {
                            if n > 0 {
                                let _ = writeln!(
                                    log,
                                    "  normalize_date_taken_format: rewrote {} rows from slash/colon form to ISO",
                                    n
                                );
                            }
                        }
                        Err(e) => {
                            let _ = writeln!(log, "  normalize_date_taken_format error: {}", e);
                        }
                    }
                }

                // Pull (id, path) for every photo.
                let photos: Vec<(i64, String)> = match db_for_xmp.lock() {
                    Ok(conn) => match conn.prepare("SELECT id, path FROM photos") {
                        Ok(mut s) => s.query_map([], |r| {
                            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                        })
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                        .unwrap_or_default(),
                        Err(_) => Vec::new(),
                    },
                    Err(_) => Vec::new(),
                };
                let _ = writeln!(log, "  loaded {} photo rows", photos.len());

                let mut scanned = 0usize;
                let mut with_sidecar = 0usize;
                let mut new_tags = 0usize;
                let mut new_descriptions = 0usize;
                let mut new_ratings = 0usize;
                let mut new_favorites = 0usize;
                const BATCH: usize = 500;
                let mut pending: Vec<(i64, crate::xmp::XmpRead)> = Vec::with_capacity(BATCH);
                let mut new_faces = 0usize;
                let mut new_persons = 0usize;
                let mut flush = |pending: &mut Vec<(i64, crate::xmp::XmpRead)>,
                                 stats: (&mut usize, &mut usize, &mut usize, &mut usize, &mut usize, &mut usize)|
                 -> Result<(), String> {
                    if pending.is_empty() { return Ok(()); }
                    let conn = db_for_xmp.lock().map_err(|_| "lock".to_string())?;
                    let txn = conn.unchecked_transaction().map_err(|e| e.to_string())?;
                    // v1.5.111 — case-insensitive de-dup: refuse the
                    // INSERT if a case-variant of this tag already
                    // exists on the photo (e.g. photo already has
                    // "Lara" from face-naming; Mac sidecar tries to
                    // insert "lara" — we skip). UNIQUE(photo_id, tag)
                    // alone only catches byte-exact dupes.
                    let mut insert_tag = txn
                        .prepare_cached(
                            "INSERT INTO tags (photo_id, tag, confidence, source)
                             SELECT ?1, ?2, ?3, ?4
                             WHERE NOT EXISTS (
                                 SELECT 1 FROM tags
                                 WHERE photo_id = ?1 AND tag = ?2 COLLATE NOCASE
                             )"
                        )
                        .map_err(|e| format!("prepare insert: {}", e))?;
                    // v1.5.110 — flip status pending -> tagged when we
                    // import keywords from a sidecar. Without this the
                    // "Tagged" stat in the sidebar undercounts Mac-only
                    // photos: tags exist in the tags table but
                    // photos.status stays 'pending' so the count is
                    // wrong. Skip 'error' rows so we don't paper over
                    // real ingestion failures.
                    let mut mark_tagged = txn
                        .prepare_cached(
                            "UPDATE photos SET status='tagged', tagged_at=?2 WHERE id=?1 AND status='pending'"
                        )
                        .map_err(|e| format!("prepare mark_tagged: {}", e))?;
                    // v1.5.108 — MWG face region import. Two extra prepared
                    // statements: one to look up "does this photo+person+
                    // imported-face already exist" (idempotency on re-launch),
                    // one to insert the new face_region with the
                    // denormalised pixel box.
                    let mut find_face = txn
                        .prepare_cached(
                            "SELECT 1 FROM face_regions WHERE photo_id = ?1 AND person_id = ?2 AND embedding IS NULL LIMIT 1"
                        )
                        .map_err(|e| format!("prepare find_face: {}", e))?;
                    let mut insert_face = txn
                        .prepare_cached(
                            "INSERT INTO face_regions (photo_id, x1, y1, x2, y2, score, embedding, person_id, created_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL, ?6, ?7)"
                        )
                        .map_err(|e| format!("prepare insert_face: {}", e))?;
                    let now = chrono::Utc::now().to_rfc3339();
                    for (id, xmp) in pending.drain(..) {
                        if !xmp.keywords.is_empty() {
                            for tag in &xmp.keywords {
                                if insert_tag
                                    .execute(rusqlite::params![id, tag, 1.0_f64, "xmp_sidecar"])
                                    .map(|n| n > 0)
                                    .unwrap_or(false)
                                {
                                    *stats.0 += 1;
                                }
                            }
                            // v1.5.110 — bump pending->tagged so the
                            // "Tagged" sidebar counter reflects Mac
                            // photos. UPDATE is conditional on
                            // status='pending' so we don't disturb
                            // 'tagged'/'error' rows.
                            let _ = mark_tagged.execute(rusqlite::params![id, &now]);
                        }
                        if let Some(d) = xmp.description.as_deref() {
                            if !d.trim().is_empty() {
                                if db::update_photo_description(&txn, id, d).is_ok() {
                                    *stats.1 += 1;
                                }
                            }
                        }
                        if let Some(r) = xmp.rating {
                            if (-1..=5).contains(&r) {
                                if db::set_rating(&txn, id, r).is_ok() {
                                    *stats.2 += 1;
                                }
                            }
                        }
                        if xmp.label.as_deref() == Some("Red") {
                            if db::set_favorite(&txn, id, true).is_ok() {
                                *stats.3 += 1;
                            }
                        }
                        // v1.5.108 — import MWG regions as face_regions
                        // rows. Stored with embedding=NULL + score=0 so
                        // we can tell "imported from XMP" apart from
                        // "AI-detected" later (e.g. for cluster fill
                        // or rescan triggers).
                        for face in &xmp.faces {
                            let trimmed = face.name.trim();
                            if trimmed.is_empty() { continue; }
                            // find or create the person
                            let person_id: i64 = match db::find_person_by_name(&txn, trimmed) {
                                Ok(Some(pid)) => pid,
                                _ => match db::create_person(&txn, trimmed) {
                                    Ok(pid) => { *stats.5 += 1; pid }
                                    Err(_) => continue,
                                },
                            };
                            // already imported? skip
                            let exists: Option<i32> = find_face
                                .query_row(rusqlite::params![id, person_id], |r| r.get(0))
                                .ok();
                            if exists.is_some() { continue; }
                            // Pick the source dimensions. Prefer the
                            // AppliedToDimensions stored in the XMP; if
                            // missing fall back to the photos.width/
                            // photos.height we already have in the DB.
                            let (img_w, img_h) = if face.applied_w > 0 && face.applied_h > 0 {
                                (face.applied_w as f32, face.applied_h as f32)
                            } else {
                                // Look up live dimensions from photos row.
                                let dims: Option<(i32, i32)> = txn
                                    .query_row(
                                        "SELECT width, height FROM photos WHERE id = ?1",
                                        rusqlite::params![id],
                                        |r| Ok((r.get::<_, i32>(0)?, r.get::<_, i32>(1)?)),
                                    )
                                    .ok();
                                match dims {
                                    Some((w, h)) if w > 0 && h > 0 => (w as f32, h as f32),
                                    _ => continue, // no way to denormalise
                                }
                            };
                            let x1 = ((face.cx - face.w / 2.0) * img_w).max(0.0) as i32;
                            let y1 = ((face.cy - face.h / 2.0) * img_h).max(0.0) as i32;
                            let x2 = ((face.cx + face.w / 2.0) * img_w).min(img_w) as i32;
                            let y2 = ((face.cy + face.h / 2.0) * img_h).min(img_h) as i32;
                            let now = chrono::Utc::now().to_rfc3339();
                            if insert_face
                                .execute(rusqlite::params![id, x1, y1, x2, y2, person_id, now])
                                .is_ok()
                            {
                                *stats.4 += 1;
                            }
                        }
                    }
                    drop(insert_tag);
                    drop(mark_tagged);
                    drop(find_face);
                    drop(insert_face);
                    txn.commit().map_err(|e| e.to_string())?;
                    Ok(())
                };

                for (id, path) in &photos {
                    scanned += 1;
                    if let Ok(Some(xmp)) = crate::xmp::read_xmp_sidecar(path) {
                        if !xmp.keywords.is_empty() || xmp.description.is_some()
                            || xmp.rating.is_some() || xmp.label.is_some() {
                            with_sidecar += 1;
                            pending.push((*id, xmp));
                        }
                    }
                    if pending.len() >= BATCH {
                        if let Err(e) = flush(&mut pending, (&mut new_tags, &mut new_descriptions, &mut new_ratings, &mut new_favorites, &mut new_faces, &mut new_persons)) {
                            let _ = writeln!(log, "  flush err at scanned={}: {}", scanned, e);
                        }
                    }
                }
                if let Err(e) = flush(&mut pending, (&mut new_tags, &mut new_descriptions, &mut new_ratings, &mut new_favorites, &mut new_faces, &mut new_persons)) {
                    let _ = writeln!(log, "  final flush err: {}", e);
                }
                let _ = writeln!(log,
                    "  result: scanned={} with_sidecar={} new_tags={} new_desc={} new_rating={} new_fav={} new_faces={} new_persons={}",
                    scanned, with_sidecar, new_tags, new_descriptions, new_ratings, new_favorites, new_faces, new_persons);
                let _ = writeln!(log, "  duration: {:?}", chrono::Utc::now() - started);
                let _ = std::fs::write(&log_path, log);
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // Original commands
            commands::open_folder_dialog,
            commands::open_file_dialog,
            commands::scan_folder,
            commands::stop_scan,
            commands::get_folders,
            commands::get_folders_with_status,
            commands::get_photos,
            commands::get_photo_detail,
            commands::search_photos,
            commands::get_stats,
            commands::get_thumbnail,
            commands::get_thumbnail_path,
            commands::regenerate_thumbnails,
            commands::fix_sideways_thumbnails,
            commands::start_tagging,
            commands::stop_tagging,
            commands::add_tag,
            commands::remove_tag,
            commands::get_photos_timeline,
            commands::get_timeline_buckets,
            commands::backfill_dates,
            commands::set_photo_date_taken,
            commands::check_ffmpeg,
            commands::get_settings,
            commands::save_setting,
            commands::get_provider_statuses,
            commands::set_estimated_location,
            commands::get_all_tags,
            commands::check_ollama,
            commands::get_related_tags,
            // 1. XMP sidecar
            // v1.5.61 — touch to bust generate_context!() proc-macro cache so
            // recent dist/index.html edits actually re-embed into the binary.
            commands::write_xmp_for_photo,
            commands::write_xmp_all,
            commands::delete_all_xmp_sidecars,
            // v1.5.104 — read .xmp sidecars (Mac side writes, Windows reads)
            commands::import_xmp_sidecars,
            // v1.5.107 — dedicated person filter (no free-text tokenisation)
            commands::find_photos_by_person,
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
            commands::reveal_app_data_dir,
            // 10. Duplicates
            commands::compute_phashes,
            commands::get_duplicates,
            // 10b. Cleanup (duplicates + blurry)
            commands::compute_blur_scores,
            commands::get_cleanup_summary,
            commands::get_cleanup_duplicates,
            commands::get_cleanup_blurry,
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
            commands::merge_persons,
            commands::get_person_timeline,
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
            commands::ai_search_expand,
            commands::get_unknown_faces,
            commands::name_face_and_propagate,
            commands::suggest_face_matches,
            commands::detect_faces_background,
            commands::count_unscanned_faces,
            commands::redetect_unnamed_faces_in_folder,
            commands::stop_face_scan,
            // 16. GPU status & person tag sync
            commands::get_gpu_status,
            commands::set_tray_progress,
            commands::hide_to_tray,
            commands::sync_person_tags,
            // 17. Full-res lightbox
            commands::get_photo_full,
            // 18. File date, HEIC codec, Device auto-import
            commands::get_file_date,
            commands::check_heic_support,
            commands::install_heic_codec,
            commands::start_device_monitor,
            commands::stop_device_monitor,
            commands::rescan_devices,
            commands::mtp_list_devices,
            commands::mtp_list_media,
            commands::mtp_import,
            commands::mtp_delete,
            commands::mtp_delete_non_favorites,
            commands::import_from_device,
            // 19. Rating & Favorites
            commands::set_rating,
            commands::set_favorite,
            commands::batch_set_rating,
            commands::batch_set_favorite,
            commands::batch_add_tags,
            commands::batch_remove_tags,
            commands::batch_add_tags_with_xmp,
            // 20. Find Similar (CLIP)
            commands::find_similar,
            commands::find_similar_by_image_path,
            commands::find_similar_by_image_bytes,
            commands::save_cropped_image,
            // 21. Color Extraction & Search
            commands::extract_colors_batch,
            commands::search_by_color,
            // 22. Library Analytics
            commands::get_library_analytics,
            // 23. Calendar View
            commands::get_photos_calendar,
            commands::get_year_month_counts,
            // 24. Health Check
            commands::run_health_check,
            commands::fix_health_issues,
            // 25. Skip face + Delete photos
            commands::skip_face,
            commands::skip_face_cluster,
            commands::filter_still_unknown,
            commands::skip_faces_batch,
            commands::skip_all_unknown_faces,
            commands::undo_face_skip,
            commands::reset_all_skipped_faces,
            commands::count_skipped_faces,
            commands::undo_face_name,
            commands::count_unknown_faces,
            commands::delete_photos,
            // 26. Smart Rename
            commands::generate_smart_names,
            commands::apply_rename,
            // 27. Clear face data
            commands::clear_all_faces,
            // 28. Memories, maintenance, relink
            commands::get_on_this_day,
            commands::gc_thumbnails,
            commands::find_missing_files,
            commands::relink_photo,
            // 29. Phase 10 — Private vault / NSFW / GPS clusters
            commands::toggle_photo_private,
            commands::list_private_photos,
            commands::vault_has_pin,
            commands::vault_set_pin,
            commands::vault_unlock,
            commands::vault_clear_pin,
            commands::vault_add_paths,
            commands::vault_reset_full,
            commands::vault_set_pin_with_recovery,
            commands::vault_verify_mnemonic,
            commands::vault_restore_from_mnemonic,
            commands::vault_lock,
            commands::vault_kek_loaded,
            commands::vault_decrypt_to_temp,
            commands::list_vault_folders,
            commands::vault_decrypt_folder_to_explorer,
            commands::vault_relock_folder,
            commands::vault_migrate_to_store,
            commands::get_private_thumbnail,
            commands::get_private_photo_data,
            commands::vault_pending_file_migration_count,
            commands::vault_run_file_migration,
            commands::vault_biometric_status,
            commands::vault_biometric_enroll,
            commands::vault_biometric_disable,
            commands::vault_biometric_unlock,
            commands::auto_hide_nsfw,
            commands::compute_gps_clusters,
            commands::get_gps_clusters,
            commands::set_tag_language,
            commands::get_tag_language,
            commands::get_tray_prefs,
            commands::set_tray_prefs,
            commands::set_watch_folder_enabled,
            commands::set_watch_folder_auto_tag,
            commands::batch_set_private,
            commands::save_gps_cluster_as_collection,
            commands::get_scan_history,
            commands::export_collection_as_folder,
            commands::batch_assign_person,
            commands::set_photo_description,
            commands::export_metadata_snapshot,
            commands::import_metadata_snapshot,
            commands::merge_duplicate_photos,
            commands::save_all_folders_as_collections,
            commands::get_trending_tags,
        ])
        // Intercept window close on the main window. If the `close_to_tray`
        // preference is enabled we hide the window instead of exiting, so the
        // app keeps running in the system tray (watch-folder scans + background
        // notifications stay alive).
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let app = window.app_handle();
                    let close_to_tray = app.try_state::<AppState>()
                        .and_then(|state| {
                            let conn = state.db.lock().ok()?;
                            db::get_setting(&conn, "close_to_tray").ok().flatten()
                        })
                        .map(|v| v == "1")
                        .unwrap_or(false);
                    if close_to_tray {
                        let _ = window.hide();
                        api.prevent_close();
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
