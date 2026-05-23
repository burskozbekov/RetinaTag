//! v1.5.266 — LAN HTTP server impl (axum). Parity with Mac v1.5.242.
//!
//! Exposes a small surface so the shared iOS app can discover the
//! desktop (via Bonjour, see lan_bonjour) and pair with it:
//!
//!   GET  /api/ping             — version / platform / hostname
//!   POST /api/pair/complete    — { code, device_name } → { token }
//!
//! /api/upload (multipart, 2 GB body, stream-to-disk) is the next
//! atom — pulled out to keep this release small. Nothing in this
//! file is wired into the running app yet; the `run_server` entry
//! point will be invoked from setup() in a follow-up release.

#![allow(dead_code)]

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Port we bind to. Mac uses the same. Bonjour TXT will surface this
/// so the iOS client doesn't have to hard-code it.
pub const PORT: u16 = 9876;

/// Shared state handed to every handler. Just the DB for now;
/// future iterations may add the inbox-path, settings, etc.
#[derive(Clone)]
pub struct ServerState {
    pub db: Arc<Mutex<Connection>>,
}

/// Spin the server on 0.0.0.0:PORT. Returns the JoinHandle the caller
/// can use to shut down. Failures bind/listen are returned as Err so
/// setup() can surface them.
pub async fn run_server(db: Arc<Mutex<Connection>>) -> Result<JoinHandle<()>, String> {
    let state = ServerState { db };
    let app: Router = Router::new()
        .route("/api/ping", get(ping))
        .route("/api/pair/complete", post(pair_complete))
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
    /// Plaintext bearer; the desktop never sees it after this point
    /// (we only persisted the SHA-256). The iOS app stores it in
    /// Keychain.
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
