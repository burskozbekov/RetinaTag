//! v1.5.266 — LAN HTTP server impl (axum). Parity with Mac v1.5.242.
//! v1.5.269 — adds /api/upload (multipart file ingest).
//!
//! Exposes the surface the shared iOS app needs to (1) discover the
//! desktop via Bonjour, (2) pair with a 6-digit code, and (3) upload
//! photos directly:
//!
//!   GET  /api/ping             — version / platform / hostname
//!   POST /api/pair/complete    — { code, device_name } → { token }
//!   POST /api/upload           — multipart, requires Authorization:
//!                                  Bearer <token>. File is bucketed
//!                                  into <inbox>/<YYYY>/<YYYY_MM>/
//!                                  using scanner::extract_date_taken.

#![allow(dead_code)]

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Port we bind to. Mac uses the same. Bonjour TXT will surface this
/// so the iOS client doesn't have to hard-code it.
pub const PORT: u16 = 9876;

/// 2 GB max body. Matches Mac. Plenty for a single ProRAW frame or
/// short 4K clip; an iOS app uploading a longer movie should chunk.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Shared state handed to every handler.
#[derive(Clone)]
pub struct ServerState {
    pub db: Arc<Mutex<Connection>>,
    /// v1.5.319 — used by /api/pair/request to emit a Tauri event so
    /// PC's FE can pop the code modal when a peer initiates pairing.
    pub app_handle: Option<tauri::AppHandle>,
}

/// Spin the server on 0.0.0.0:PORT. Returns the JoinHandle the caller
/// can use to shut down. Failures bind/listen are returned as Err so
/// setup() can surface them.
pub async fn run_server(
    db: Arc<Mutex<Connection>>,
    app_handle: tauri::AppHandle,
) -> Result<JoinHandle<()>, String> {
    let state = ServerState { db, app_handle: Some(app_handle) };
    let app: Router = Router::new()
        .route("/api/ping", get(ping))
        .route("/api/pair/request", post(pair_request))
        .route("/api/pair/complete", post(pair_complete))
        .route("/api/upload", post(upload))
        // v1.5.320 — vault flow endpoints (Mac-spec parity).
        .route("/api/vault/status", get(vault_status))
        .route("/api/vault/unlock", post(vault_unlock))
        .route("/api/vault/lock", post(vault_lock))
        // v1.5.321 — photo listing for paired peers.
        .route("/api/photos", get(list_photos))
        // v1.5.322 — thumb + full-photo byte streaming for paired peers.
        .route("/api/thumb/:id", get(thumb_for_id))
        .route("/api/photo/:id", get(photo_for_id))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state);
    let bind = format!("0.0.0.0:{}", PORT);
    let listener = TcpListener::bind(&bind)
        .await
        .map_err(|e| format!("bind {bind}: {e}"))?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(handle)
}

/// v1.5.319 — peer-initiated pairing. Mac (or another PC) posts here
/// when it wants to pair WITH us; we mint a code and surface it via
/// a Tauri event so PC's FE can pop a modal showing it.  The remote
/// will then ask its user to type the code, and POST it back to our
/// /api/pair/complete in the usual way.
#[derive(Deserialize)]
struct PairRequestBody {
    device_name: String,
}

#[derive(Serialize, Clone)]
struct IncomingPairEvent {
    code:        String,
    device_name: String,
    /// RFC3339 timestamp the code expires (lan_pairing::mint_code
    /// gives a 5-minute TTL).
    expires_at:  String,
}

async fn pair_request(
    State(state): State<ServerState>,
    Json(req): Json<PairRequestBody>,
) -> impl IntoResponse {
    use tauri::Emitter;
    let code = crate::lan_pairing::mint_code();
    let expires_at = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
    if let Some(ref handle) = state.app_handle {
        let _ = handle.emit(
            "lan-incoming-pair-request",
            IncomingPairEvent {
                code: code.clone(),
                device_name: req.device_name.clone(),
                expires_at: expires_at.clone(),
            },
        );
    }
    // 202 Accepted: we've started the pairing flow, peer should now
    // wait for the user to type the code back via /api/pair/complete.
    StatusCode::ACCEPTED.into_response()
}

#[derive(Serialize)]
struct PingResponse {
    ok:       bool,
    version:  String,
    platform: String,
    hostname: String,
}

async fn ping() -> impl IntoResponse {
    let version = env!("CARGO_PKG_VERSION").to_string();
    let platform = "windows".to_string();
    let hostname = hostname_safe();
    Json(PingResponse { ok: true, version, platform, hostname })
}

#[derive(Deserialize)]
struct PairCompleteRequest {
    code:        String,
    device_name: String,
}

#[derive(Serialize)]
struct PairCompleteResponse {
    device_id: i64,
    token:     String,
}

async fn pair_complete(
    State(state): State<ServerState>,
    Json(req): Json<PairCompleteRequest>,
) -> impl IntoResponse {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db lock poisoned").into_response(),
    };
    match crate::lan_pairing::complete_pairing(&conn, &req.code, &req.device_name) {
        Ok(p) => Json(PairCompleteResponse {
            device_id: p.device_id,
            token:     p.bearer_token,
        })
        .into_response(),
        Err(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
    }
}

// ── v1.5.320 — /api/vault/{status,unlock,lock} ─────────────────────────
// These endpoints let a paired peer (Mac, other PC) drive THIS
// machine's vault.  Auth gated by the same Bearer token middleware
// the upload path uses.  The vault KEK lives in AppState.vault_kek
// (Mutex<Option<[u8; 32]>>); we mutate it via the AppHandle.

#[derive(Serialize)]
struct VaultStatusResponse {
    has_pin:  bool,
    unlocked: bool,
}

async fn vault_status(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db lock poisoned").into_response(),
        };
        if auth_device_id(&headers, &conn).is_none() {
            return (StatusCode::UNAUTHORIZED, "Invalid or missing bearer token").into_response();
        }
    }
    let has_pin = match state.db.lock() {
        Ok(c) => crate::db::vault_has_pin(&c),
        Err(_) => false,
    };
    let mut unlocked = false;
    if let Some(ref handle) = state.app_handle {
        use tauri::Manager;
        let app_state = handle.state::<crate::AppState>();
        unlocked = app_state.vault_is_unlocked();
    }
    Json(VaultStatusResponse { has_pin, unlocked }).into_response()
}

#[derive(Deserialize)]
struct VaultUnlockBody {
    pin: String,
}

async fn vault_unlock(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(req): Json<VaultUnlockBody>,
) -> impl IntoResponse {
    // Auth.
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db lock poisoned").into_response(),
        };
        if auth_device_id(&headers, &conn).is_none() {
            return (StatusCode::UNAUTHORIZED, "Invalid or missing bearer token").into_response();
        }
    }
    // Derive KEK on the blocking pool (Argon2id can hit 1 s on a
    // slow CPU; running it on the runtime thread would stall axum).
    let db_arc = state.db.clone();
    let pin_owned = req.pin.clone();
    let kek_result: Result<Option<([u8; 32], Option<String>)>, String> =
        tokio::task::spawn_blocking(move || -> Result<Option<([u8; 32], Option<String>)>, String> {
            let conn = db_arc.lock().map_err(|_| "db lock".to_string())?;
            crate::db::vault_unlock_kek(&conn, &pin_owned).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|inner| inner);
    let kek_opt = match kek_result {
        Ok(opt) => opt,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("unlock: {e}")).into_response();
        }
    };
    let Some((kek, _phrase)) = kek_opt else {
        // Wrong PIN.  Mac's spec uses 401 for this case (vs 429 for
        // lockout, which we don't implement on PC yet — out of scope
        // for this release).
        return (StatusCode::UNAUTHORIZED, "wrong PIN").into_response();
    };
    // Stash the KEK in AppState so the future remote-photo endpoints
    // (and any local FE code) see the vault as unlocked.
    if let Some(ref handle) = state.app_handle {
        use tauri::Manager;
        let app_state = handle.state::<crate::AppState>();
        app_state.vault_set_kek(Some(kek));
    }
    StatusCode::OK.into_response()
}

async fn vault_lock(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db lock poisoned").into_response(),
        };
        if auth_device_id(&headers, &conn).is_none() {
            return (StatusCode::UNAUTHORIZED, "Invalid or missing bearer token").into_response();
        }
    }
    if let Some(ref handle) = state.app_handle {
        use tauri::Manager;
        let app_state = handle.state::<crate::AppState>();
        app_state.vault_set_kek(None);
    }
    StatusCode::OK.into_response()
}

// ── v1.5.321 — /api/photos ─────────────────────────────────────────────
// Paginated list of photos for a paired peer to render in its remote-
// library view.  Same shape as Mac's spec:
//   GET /api/photos?vault_only=<bool>&offset=N&limit=M
//   → {photos: [...], total, offset, limit}
// vault_only=true requires the vault to be currently unlocked; 401
// otherwise so the FE can prompt for PIN.

#[derive(Deserialize)]
struct ListPhotosQuery {
    #[serde(default)]
    vault_only: Option<bool>,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Serialize)]
struct ListPhotosResponse {
    photos: Vec<crate::models::PhotoSummary>,
    total:  i64,
    offset: i64,
    limit:  i64,
}

async fn list_photos(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(params): Query<ListPhotosQuery>,
) -> impl IntoResponse {
    // Auth.
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db lock poisoned").into_response(),
        };
        if auth_device_id(&headers, &conn).is_none() {
            return (StatusCode::UNAUTHORIZED, "Invalid or missing bearer token").into_response();
        }
    }
    let vault_only = params.vault_only.unwrap_or(false);
    let offset = params.offset.unwrap_or(0).max(0);
    // Soft cap at 500 per page — keeps a single response under a few
    // hundred kB even when filenames + tag arrays balloon, lets the
    // peer paginate visibly.
    let limit = params.limit.unwrap_or(200).clamp(1, 500);

    // Vault-only gate: if the vault is currently locked we DON'T leak
    // the existence / count of vault photos.  Caller should POST
    // /api/vault/unlock first.
    if vault_only {
        let unlocked = if let Some(ref handle) = state.app_handle {
            use tauri::Manager;
            let app_state = handle.state::<crate::AppState>();
            app_state.vault_is_unlocked()
        } else { false };
        if !unlocked {
            return (StatusCode::UNAUTHORIZED, "Vault locked").into_response();
        }
    }

    // Run the actual query off the axum runtime thread.  db::get_photos
    // on a 66 k library is 50-200 ms cold.
    let db = state.db.clone();
    let result: Result<(Vec<crate::models::PhotoSummary>, i64), String> =
        tokio::task::spawn_blocking(move || -> Result<(Vec<crate::models::PhotoSummary>, i64), String> {
            let conn = db.lock().map_err(|_| "db lock".to_string())?;
            crate::db::get_photos(
                &conn, offset, limit, None, None, None, Some(vault_only),
            ).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);

    match result {
        Ok((photos, total)) => Json(ListPhotosResponse {
            photos, total, offset, limit,
        }).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("list_photos: {e}")).into_response(),
    }
}

// ── v1.5.322 — /api/thumb/:id + /api/photo/:id ─────────────────────────
// Byte-streaming endpoints for paired peers.  Same auth gate as
// /api/photos.  For vault photos (.rtenc on disk) we decrypt in-memory
// before sending — the peer never sees ciphertext nor needs our KEK.

fn mime_for_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png"          => "image/png",
        "gif"          => "image/gif",
        "webp"         => "image/webp",
        "bmp"          => "image/bmp",
        "heic" | "heif" => "image/heic",
        "tiff" | "tif" => "image/tiff",
        "mp4" | "m4v"  => "video/mp4",
        "mov"          => "video/quicktime",
        "webm"         => "video/webm",
        "mkv"          => "video/x-matroska",
        _              => "application/octet-stream",
    }
}

async fn thumb_for_id(
    State(state): State<ServerState>,
    headers: HeaderMap,
    AxumPath(photo_id): AxumPath<i64>,
) -> impl IntoResponse {
    // Auth.
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db lock poisoned").into_response(),
        };
        if auth_device_id(&headers, &conn).is_none() {
            return (StatusCode::UNAUTHORIZED, "Invalid or missing bearer token").into_response();
        }
    }
    // Resolve photo + hash.
    let (path, hash) = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db lock poisoned").into_response(),
        };
        match crate::db::get_photo_path_and_hash(&conn, photo_id) {
            Ok(v) => v,
            Err(_) => return (StatusCode::NOT_FOUND, format!("photo {photo_id} not found")).into_response(),
        }
    };
    // Vault gate: if the path is a .rtenc, vault must be unlocked.
    let is_vault = crate::vault_files::is_encrypted_path(std::path::Path::new(&path));
    if is_vault {
        let unlocked = if let Some(ref handle) = state.app_handle {
            use tauri::Manager;
            let app_state = handle.state::<crate::AppState>();
            app_state.vault_is_unlocked()
        } else { false };
        if !unlocked {
            return (StatusCode::UNAUTHORIZED, "Vault locked").into_response();
        }
    }
    // Off the runtime: thumbnail creation can read+resize a big image.
    let thumbs_dir = if let Some(ref handle) = state.app_handle {
        use tauri::Manager;
        let app_state = handle.state::<crate::AppState>();
        app_state.thumbnails_dir.clone()
    } else {
        // Fallback (shouldn't happen in normal operation).
        std::env::temp_dir().join("retinatag-thumbs")
    };
    let db = state.db.clone();
    let kek_opt: Option<[u8; 32]> = if is_vault {
        if let Some(ref handle) = state.app_handle {
            use tauri::Manager;
            let app_state = handle.state::<crate::AppState>();
            // Snapshot the KEK into a stack-local to avoid holding the
            // mutex into spawn_blocking (the lock can't cross await).
            let mut snap = None;
            if let Ok(g) = app_state.vault_kek.lock() {
                snap = *g;
            }
            snap
        } else { None }
    } else { None };
    let result: Result<Vec<u8>, String> = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        if is_vault {
            let kek = kek_opt.ok_or_else(|| "no kek".to_string())?;
            // Try the encrypted-thumb DB blob first; fall back to
            // decrypting the .rtenc + downsizing to 256 px.
            let conn = db.lock().map_err(|_| "db lock".to_string())?;
            if let Ok(Some(b)) = crate::db::get_encrypted_thumb(&conn, photo_id) {
                drop(conn);
                let plain = crate::vault_crypto::open(&kek, &b).map_err(|e| e.to_string())?;
                return Ok(plain);
            }
            drop(conn);
            // v1.5.349 — VIDEO vault items: image::load_from_memory
            // can't decode .MOV/.MP4/.MKV bytes and would return a
            // 500 to the peer.  Detect by stripping the .rtenc suffix
            // and checking the inner extension against
            // VIDEO_EXTENSIONS.  When we don't have a cached
            // encrypted thumb (handled above) we return a structured
            // 404 instead — the peer's UI already renders a
            // film-strip placeholder for missing video thumbs (PC
            // shipped that in v1.5.334), so a clean 404 keeps the
            // failure visible without a confusing 500.
            //
            // Note: synthesising a real video frame here would need
            // ffmpeg on PATH (not guaranteed) plus a temp-file
            // decrypt + cleanup; deferring that to a follow-up
            // release.  The encrypted-thumb cache above is the
            // happy-path once a vault video has been viewed locally.
            let enc_path = std::path::PathBuf::from(&path);
            let inner = enc_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.strip_suffix(".rtenc").unwrap_or(n))
                .unwrap_or("");
            let inner_ext = std::path::Path::new(inner)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if crate::scanner::VIDEO_EXTENSIONS.contains(&inner_ext.as_str()) {
                return Err("__thumb_404__:video thumb not cached".to_string());
            }
            let plain = crate::vault_files::decrypt_to_bytes(&enc_path, &kek)?;
            // Resize to a 256-px JPEG so the peer doesn't pull a
            // 12-megapixel HEIC for a thumbnail.
            let img = image::load_from_memory(&plain).map_err(|e| e.to_string())?;
            let img = img.thumbnail(256, 256);
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::Jpeg).map_err(|e| e.to_string())?;
            Ok(buf.into_inner())
        } else {
            // Non-vault: standard cached 256-px JPEG path.
            let _b64 = crate::thumbnail::get_or_create_thumbnail(&path, &hash, &thumbs_dir, 256)
                .map_err(|e| e.to_string())?;
            let cache_name = crate::thumbnail::thumb_cache_name(&hash);
            let thumb_path = thumbs_dir.join(&cache_name);
            std::fs::read(&thumb_path).map_err(|e| format!("read thumb: {e}"))
        }
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match result {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .body(axum::body::Body::from(bytes))
            .unwrap()
            .into_response(),
        // v1.5.349 — sentinel prefix lets the vault-video branch
        // request a clean 404 from inside spawn_blocking without
        // smuggling a StatusCode through the Result.
        Err(e) if e.starts_with("__thumb_404__:") => {
            let body = e.trim_start_matches("__thumb_404__:").to_string();
            (StatusCode::NOT_FOUND, body).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("thumb: {e}")).into_response(),
    }
}

async fn photo_for_id(
    State(state): State<ServerState>,
    headers: HeaderMap,
    AxumPath(photo_id): AxumPath<i64>,
) -> impl IntoResponse {
    // Auth.
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db lock poisoned").into_response(),
        };
        if auth_device_id(&headers, &conn).is_none() {
            return (StatusCode::UNAUTHORIZED, "Invalid or missing bearer token").into_response();
        }
    }
    // Resolve photo.
    let path = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db lock poisoned").into_response(),
        };
        match crate::db::get_photo_path_and_hash(&conn, photo_id) {
            Ok((p, _)) => p,
            Err(_) => return (StatusCode::NOT_FOUND, format!("photo {photo_id} not found")).into_response(),
        }
    };
    let is_vault = crate::vault_files::is_encrypted_path(std::path::Path::new(&path));
    if is_vault {
        let unlocked = if let Some(ref handle) = state.app_handle {
            use tauri::Manager;
            let app_state = handle.state::<crate::AppState>();
            app_state.vault_is_unlocked()
        } else { false };
        if !unlocked {
            return (StatusCode::UNAUTHORIZED, "Vault locked").into_response();
        }
    }
    // For vault photos, decrypt in spawn_blocking + figure the inner
    // extension for Content-Type.  For non-vault, just stream.
    let kek_opt: Option<[u8; 32]> = if is_vault {
        if let Some(ref handle) = state.app_handle {
            use tauri::Manager;
            let app_state = handle.state::<crate::AppState>();
            let mut snap = None;
            if let Ok(g) = app_state.vault_kek.lock() {
                snap = *g;
            }
            snap
        } else { None }
    } else { None };
    let path_for_task = path.clone();
    let result: Result<(Vec<u8>, String), String> = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), String> {
        let p = std::path::PathBuf::from(&path_for_task);
        if is_vault {
            let kek = kek_opt.ok_or_else(|| "no kek".to_string())?;
            let plain = crate::vault_files::decrypt_to_bytes(&p, &kek)?;
            // Inner extension lives inside the .rtenc filename so the
            // peer can pick the right player / decoder.
            let inner_ext = crate::vault_files::original_path_for(&p)
                .as_ref()
                .and_then(|pp| pp.extension())
                .and_then(|e| e.to_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            Ok((plain, inner_ext))
        } else {
            let bytes = std::fs::read(&p).map_err(|e| format!("read photo: {e}"))?;
            let ext = p.extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            Ok((bytes, ext))
        }
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match result {
        Ok((bytes, ext)) => {
            let mime = mime_for_ext(&ext);
            // v1.5.325 — Range header support.  HTML5 <video> issues
            // `Range: bytes=N-M` requests for seek-bar scrubbing; without
            // 206 Partial Content responses the browser re-fetches the
            // whole file on every seek (and disables the seek bar for
            // streams it can't byte-index).  We have the full body in
            // memory already (vault decrypts in-memory; non-vault is
            // `std::fs::read`), so slicing the Vec<u8> is cheap.
            let total = bytes.len() as u64;
            let range_hdr = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
            if let Some((start, end)) = range_hdr.and_then(|h| parse_range_header(h, total)) {
                let slice = bytes[start as usize ..= end as usize].to_vec();
                let content_range = format!("bytes {start}-{end}/{total}");
                return Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE, mime)
                    .header(header::CONTENT_RANGE, content_range)
                    .header(header::CONTENT_LENGTH, slice.len())
                    .header(header::ACCEPT_RANGES, "bytes")
                    .body(axum::body::Body::from(slice))
                    .unwrap()
                    .into_response();
            }
            // No (or unparseable) Range header — full body.
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CONTENT_LENGTH, total)
                .header(header::ACCEPT_RANGES, "bytes")
                .body(axum::body::Body::from(bytes))
                .unwrap()
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("photo: {e}")).into_response(),
    }
}

/// Parse a single-range `Range: bytes=N-M` header against a known
/// content length.  Returns `(start, end_inclusive)` clamped into
/// `[0, total)`.  Returns `None` for multi-range, suffix-range
/// (`bytes=-N` last N bytes — easy to add later if a caller wants
/// it), or malformed inputs.  Following RFC 7233 enough for HTML5
/// `<video>` which only ever asks for `bytes=N-` or `bytes=N-M`.
fn parse_range_header(hdr: &str, total: u64) -> Option<(u64, u64)> {
    let rest = hdr.strip_prefix("bytes=")?;
    if rest.contains(',') { return None; }
    let mut it = rest.split('-');
    let s = it.next()?.trim();
    let e = it.next()?.trim();
    if it.next().is_some() { return None; }
    if s.is_empty() { return None; } // suffix range not supported
    let start: u64 = s.parse().ok()?;
    if start >= total { return None; }
    let end: u64 = if e.is_empty() {
        total.saturating_sub(1)
    } else {
        e.parse().ok()?
    };
    let end = end.min(total.saturating_sub(1));
    if start > end { return None; }
    Some((start, end))
}

// ── /api/upload ─────────────────────────────────────────────────────────
#[derive(Serialize)]
struct UploadResponse {
    ok:           bool,
    saved_path:   String,
    bytes:        usize,
    bucket_year:  i32,
    bucket_month: u32,
}

/// Pulls the Bearer token from the Authorization header, hashes it,
/// looks up via lan_pairing::verify_token. Returns the device id on
/// success, None on any header or token miss.
fn auth_device_id(headers: &HeaderMap, conn: &Connection) -> Option<i64> {
    let raw = headers.get("authorization")?.to_str().ok()?;
    let bearer = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer "))?;
    crate::lan_pairing::verify_token(conn, bearer.trim())
}

/// %USERPROFILE%\Pictures\RetinaTag-iOS-Inbox\
fn default_inbox() -> PathBuf {
    if let Some(p) = dirs::picture_dir() {
        return p.join("RetinaTag-iOS-Inbox");
    }
    // Fallback to CWD if no pictures dir is exposed by the OS.
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("RetinaTag-iOS-Inbox")
}

/// Sanitize a filename to ascii-safe + drop path components. iOS sends
/// "IMG_1234.HEIC"-style names; we still defend against ../etc/passwd
/// just in case.
fn safe_filename(raw: &str) -> String {
    let stem = Path::new(raw).file_name().and_then(|s| s.to_str()).unwrap_or("upload");
    let cleaned: String = stem
        .chars()
        .filter(|c| !matches!(*c, '\\' | '/' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    if cleaned.trim().is_empty() {
        "upload.bin".to_string()
    } else {
        cleaned
    }
}

/// Add the inbox dir as a watch folder if it isn't one already. Best-
/// effort: errors are logged to stderr, not propagated to the client.
fn ensure_inbox_is_watch_folder(conn: &Connection, inbox: &Path) {
    let path_str = inbox.to_string_lossy().to_string();
    let _ = conn.execute(
        "INSERT OR IGNORE INTO watch_folders (path, auto_tag, enabled, created_at) \
         VALUES (?1, 0, 1, ?2)",
        rusqlite::params![&path_str, chrono::Local::now().to_rfc3339()],
    );
}

async fn upload(
    State(state): State<ServerState>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> impl IntoResponse {
    // 1. Authenticate.
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db lock poisoned").into_response(),
        };
        if auth_device_id(&headers, &conn).is_none() {
            return (StatusCode::UNAUTHORIZED, "Invalid or missing bearer token").into_response();
        }
    }

    // 2. Pull the first file part. iOS app sends one upload per
    //    request; this loop is just a defensive walker.
    let mut filename: Option<String> = None;
    let mut bytes:    Option<Bytes>  = None;
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None)    => break,
            Err(e)      => return (StatusCode::BAD_REQUEST, format!("multipart: {e}")).into_response(),
        };
        let name = field.name().map(|s| s.to_string()).unwrap_or_default();
        if name == "file" || filename.is_none() {
            if let Some(fname) = field.file_name() {
                filename = Some(safe_filename(fname));
            }
            match field.bytes().await {
                Ok(b)  => { bytes = Some(b); break; }
                Err(e) => return (StatusCode::BAD_REQUEST, format!("read body: {e}")).into_response(),
            }
        }
    }
    let body  = match bytes    { Some(b) => b, None => return (StatusCode::BAD_REQUEST, "no file field").into_response() };
    let fname = filename.unwrap_or_else(|| "upload.bin".to_string());

    // 3. Write the file to a temp path inside the inbox first; we'll
    //    move it into its date bucket once we've extracted the date.
    let inbox = default_inbox();
    if let Err(e) = std::fs::create_dir_all(&inbox) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("mkdir inbox: {e}")).into_response();
    }
    let tmp_path = inbox.join(format!(".upload-{}.tmp", std::process::id()));
    if let Err(e) = tokio::fs::write(&tmp_path, &body).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("write tmp: {e}")).into_response();
    }

    // 4. Decide the date bucket. extract_date_taken reads EXIF + XMP +
    //    path patterns (and v1.5.266+ skips mtime), so we get an
    //    authoritative date from the file's own metadata or the
    //    iOS-side filename. If neither yields anything we fall
    //    back to the current local year/month.
    use chrono::Datelike;
    let tmp_str = tmp_path.to_string_lossy().to_string();
    let (year, month) = {
        let parsed = crate::scanner::extract_date_taken(&tmp_str);
        if let Some(dt) = parsed
            .as_deref()
            .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok())
        {
            (dt.year(), dt.month())
        } else {
            let now = chrono::Local::now();
            (now.year(), now.month())
        }
    };

    let bucket = inbox.join(format!("{:04}", year)).join(format!("{:04}_{:02}", year, month));
    if let Err(e) = std::fs::create_dir_all(&bucket) {
        let _ = std::fs::remove_file(&tmp_path);
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("mkdir bucket: {e}")).into_response();
    }

    // 5. Pick a non-clashing destination filename. If "IMG_1234.HEIC"
    //    already exists, suffix with -1, -2, ...
    let final_path = pick_unique_path(&bucket, &fname);
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        // Cross-volume rename can fail with EXDEV; fall back to copy+remove.
        if let Err(e2) = std::fs::copy(&tmp_path, &final_path) {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("move file: {} / {}", e, e2)).into_response();
        }
        let _ = std::fs::remove_file(&tmp_path);
    }

    // 6. Auto-add the inbox as a watch folder so the scanner picks
    //    these up without the user touching Settings.
    {
        let conn = state.db.lock().ok();
        if let Some(conn) = conn {
            ensure_inbox_is_watch_folder(&conn, &inbox);
        }
    }

    Json(UploadResponse {
        ok:           true,
        saved_path:   final_path.to_string_lossy().to_string(),
        bytes:        body.len(),
        bucket_year:  year,
        bucket_month: month,
    })
    .into_response()
}

/// Find a path inside `dir` that doesn't already exist, using `base`
/// as the start. "IMG.HEIC", "IMG-1.HEIC", "IMG-2.HEIC", …
fn pick_unique_path(dir: &Path, base: &str) -> PathBuf {
    let first = dir.join(base);
    if !first.exists() { return first; }
    let (stem, ext) = match base.rfind('.') {
        Some(i) => (&base[..i], &base[i..]),
        None    => (base, ""),
    };
    for n in 1..10_000 {
        let candidate = dir.join(format!("{}-{}{}", stem, n, ext));
        if !candidate.exists() { return candidate; }
    }
    // Astronomically unlikely; just stomp on the original.
    first
}

/// Best-effort hostname lookup. Returns "unknown" on any failure so
/// the response shape stays stable for the iOS client.
fn hostname_safe() -> String {
    if let Ok(s) = std::env::var("COMPUTERNAME") {
        if !s.trim().is_empty() {
            return s;
        }
    }
    "unknown".to_string()
}
