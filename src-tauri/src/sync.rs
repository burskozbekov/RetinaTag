//! LAN sync (Phase-1: discovery + pairing + signed transport)
//!
//! This module is the foundation for cross-device tag sync. It owns:
//!
//!   * An Ed25519 identity per install, generated lazily on first
//!     `enable()`. The secret key never leaves the box; the public key
//!     is announced via mDNS and used by peers to verify our signed
//!     envelopes.
//!
//!   * An mDNS-SD advertisement on `_retinatag._tcp.local` so peers on
//!     the same Wi-Fi find each other without manual IP entry.
//!
//!   * An axum HTTP server on a high random port for the peer-to-peer
//!     verbs. Phase-1 ships `/ping` and `/pair`; the data-sync verbs
//!     land in Phase-2.
//!
//!   * A short-lived pair code (6 digits, OsRng) the user reads from
//!     one device into the other. After both sides have exchanged
//!     pubkeys + agreed on the code, they each store the other in
//!     `sync_peers`.
//!
//! Everything below the user-controlled `enable` flag is OFF until the
//! user opts in. Disabling stops the mDNS broadcast and the HTTP
//! server, and clears the in-memory pair-code so a forgotten
//! "Network Sync" toggle doesn't leak the device on the network.
//!
//! Phase-2 will add: signed envelope verification on inbound requests,
//! data-sync verbs (tags / ratings / favorites / description), a
//! Lamport-ish per-row updated_at cursor, conflict resolution
//! (last-write-wins by timestamp), and a JS-side preview-before-apply
//! diff UI.

use anyhow::{Context, Result};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rand::rngs::OsRng;
use rand::RngCore;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// Bonjour service type. Both ends must agree on this string.
const MDNS_SERVICE: &str = "_retinatag._tcp.local.";

/// How long a freshly minted pair code stays valid before it's
/// rejected. Long enough for the user to read it across the room and
/// type it on the other device; short enough that a shoulder-surfed
/// code expires before it's useful.
const PAIR_CODE_TTL: Duration = Duration::from_secs(5 * 60);

// ── Identity ──────────────────────────────────────────────────────────

/// Public information about this install, exposed via mDNS TXT records
/// and the /ping endpoint. The secret key STAYS local — see SigningKey
/// inside `SyncService`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub device_id: String,
    pub device_name: String,
    /// Base64 (URL-safe, no padding) Ed25519 public key, 32 bytes.
    pub public_key_b64: String,
}

/// Load this install's identity from the DB, creating it on first
/// call. Idempotent — subsequent calls return the stored keypair so
/// our device_id is stable across launches and (deterministically)
/// our pubkey doesn't rotate behind paired peers' backs.
pub fn load_or_create_identity(
    conn: &Connection,
    default_name: &str,
) -> Result<(DeviceIdentity, SigningKey)> {
    let row: Option<(String, String, Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT device_id, device_name, secret_key, public_key
               FROM sync_identity WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                ))
            },
        )
        .ok();
    if let Some((device_id, device_name, sk, pk)) = row {
        let mut sk_arr = [0u8; 32];
        if sk.len() != 32 {
            anyhow::bail!("stored secret_key has wrong length");
        }
        sk_arr.copy_from_slice(&sk);
        let sk = SigningKey::from_bytes(&sk_arr);
        let identity = DeviceIdentity {
            device_id,
            device_name,
            public_key_b64: b64_url(&pk),
        };
        return Ok((identity, sk));
    }
    // First run — mint a fresh keypair.
    let mut csprng = OsRng;
    let signing = SigningKey::generate(&mut csprng);
    let verifying = signing.verifying_key();
    let mut id_bytes = [0u8; 8];
    csprng.fill_bytes(&mut id_bytes);
    let device_id = format!("rt-{}", hex::encode(id_bytes));
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO sync_identity (id, device_id, device_name, secret_key, public_key, enabled, created_at)
         VALUES (1, ?1, ?2, ?3, ?4, 0, ?5)",
        params![
            device_id,
            default_name,
            signing.to_bytes().to_vec(),
            verifying.to_bytes().to_vec(),
            now,
        ],
    )?;
    let identity = DeviceIdentity {
        device_id,
        device_name: default_name.to_string(),
        public_key_b64: b64_url(&verifying.to_bytes()),
    };
    Ok((identity, signing))
}

pub fn set_identity_name(conn: &Connection, name: &str) -> Result<()> {
    conn.execute(
        "UPDATE sync_identity SET device_name = ?1 WHERE id = 1",
        params![name],
    )?;
    Ok(())
}

pub fn set_identity_enabled(conn: &Connection, on: bool) -> Result<()> {
    conn.execute(
        "UPDATE sync_identity SET enabled = ?1 WHERE id = 1",
        params![if on { 1 } else { 0 }],
    )?;
    Ok(())
}

pub fn identity_enabled(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT enabled FROM sync_identity WHERE id = 1",
        [],
        |r| r.get::<_, i64>(0),
    )
    .ok()
    .map(|v| v == 1)
    .unwrap_or(false)
}

// ── Peer registry ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedPeer {
    pub device_id: String,
    pub device_name: String,
    pub public_key_b64: String,
    pub last_addr: Option<String>,
    pub last_seen: Option<i64>,
    pub paired_at: i64,
}

pub fn list_peers(conn: &Connection) -> Result<Vec<PairedPeer>> {
    let mut stmt = conn.prepare(
        "SELECT device_id, device_name, public_key, last_addr, last_seen, paired_at
           FROM sync_peers ORDER BY device_name COLLATE NOCASE",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let pk: Vec<u8> = r.get(2)?;
            Ok(PairedPeer {
                device_id: r.get(0)?,
                device_name: r.get(1)?,
                public_key_b64: b64_url(&pk),
                last_addr: r.get(3)?,
                last_seen: r.get(4)?,
                paired_at: r.get(5)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn insert_peer(
    conn: &Connection,
    device_id: &str,
    device_name: &str,
    public_key: &[u8],
    last_addr: Option<&str>,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT OR REPLACE INTO sync_peers
            (device_id, device_name, public_key, last_addr, last_seen, paired_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![device_id, device_name, public_key, last_addr, now],
    )?;
    Ok(())
}

pub fn remove_peer(conn: &Connection, device_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM sync_peers WHERE device_id = ?1",
        params![device_id],
    )?;
    Ok(())
}

pub fn touch_peer_last_seen(conn: &Connection, device_id: &str, addr: &str) {
    let now = chrono::Utc::now().timestamp();
    let _ = conn.execute(
        "UPDATE sync_peers SET last_seen = ?1, last_addr = ?2 WHERE device_id = ?3",
        params![now, addr, device_id],
    );
}

// ── Wire types ─────────────────────────────────────────────────────────

/// `GET /ping` response — every peer exposes this, paired or not.
/// Lets the UI discover what kind of device an mDNS hit is before the
/// user commits to pairing with it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingPayload {
    pub device_id: String,
    pub device_name: String,
    pub public_key_b64: String,
    pub protocol_version: u32,
    /// Always "RetinaTag" so a stray service on the same Bonjour
    /// domain can't be mistaken for us.
    pub app: String,
}

/// `POST /pair` request — the initiating side sends its own identity
/// + the 6-digit code it read off the responder's UI. The responder
/// verifies the code matches the one it minted, stores the peer, and
/// returns its own identity so the initiator can store it too.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    pub from: DeviceIdentity,
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairResponse {
    pub identity: DeviceIdentity,
    pub paired_at: i64,
}

// ── Discovery (mDNS) ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearbyPeer {
    pub device_id: String,
    pub device_name: String,
    pub public_key_b64: String,
    pub addr: String,
    pub port: u16,
    pub already_paired: bool,
}

// ── Service orchestrator ───────────────────────────────────────────────

/// Outstanding pair code the user has minted on THIS device. Held in
/// memory only; not persisted. Cleared on disable or successful pair.
struct PendingPairCode {
    code: String,
    minted_at: Instant,
}

pub struct SyncService {
    pub identity: DeviceIdentity,
    /// Local HTTP server port — random high port chosen at startup.
    pub port: u16,
    /// Outstanding pair code on this side (waiting for a peer to send
    /// it back via /pair).
    pending: Arc<Mutex<Option<PendingPairCode>>>,
    /// Browsed-but-not-yet-paired peers seen on the LAN.
    nearby: Arc<Mutex<HashMap<String, NearbyPeer>>>,
    /// Trigger that stops the HTTP server's tokio runtime.
    shutdown: Option<oneshot::Sender<()>>,
    /// mDNS daemon — drop to stop the broadcast.
    mdns: Option<ServiceDaemon>,
    /// mDNS browser thread join handle (currently we just let it run
    /// for the service's lifetime).
    _browser_thread: Option<std::thread::JoinHandle<()>>,
}

impl SyncService {
    /// Spin everything up. Must only be called while the user has the
    /// feature toggled on; the caller (commands::sync_enable) is
    /// responsible for persisting `identity.enabled = 1`.
    pub fn start(
        db: Arc<Mutex<Connection>>,
        identity: DeviceIdentity,
        signing: SigningKey,
    ) -> Result<Self> {
        // 1. Pick a free high port for the HTTP server.
        let listener = std::net::TcpListener::bind("0.0.0.0:0")
            .context("bind sync HTTP port")?;
        let port = listener.local_addr()?.port();
        // Convert std listener to tokio listener inside the async
        // runtime below — we hand the std listener via from_std.
        listener.set_nonblocking(true)?;

        // 2. mDNS daemon + service registration.
        let mdns = ServiceDaemon::new().context("start mDNS daemon")?;
        let host_name = format!("{}.local.", identity.device_id);
        let mut props: HashMap<String, String> = HashMap::new();
        props.insert("device_id".to_string(), identity.device_id.clone());
        props.insert("device_name".to_string(), identity.device_name.clone());
        props.insert("pubkey".to_string(), identity.public_key_b64.clone());
        props.insert("v".to_string(), "1".to_string());
        let local_ips = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|i| if i.is_loopback() { None } else { Some(i.ip()) })
            .collect::<Vec<_>>();
        let info = ServiceInfo::new(
            MDNS_SERVICE,
            &identity.device_id,
            &host_name,
            &local_ips[..],
            port,
            Some(props),
        )
        .context("build ServiceInfo")?;
        mdns.register(info).context("register mDNS service")?;

        // 3. mDNS browse loop — populate `nearby` as we see other
        //    devices.
        let nearby: Arc<Mutex<HashMap<String, NearbyPeer>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let nearby_clone = nearby.clone();
        let our_device_id = identity.device_id.clone();
        let db_for_browser = db.clone();
        let receiver = mdns
            .browse(MDNS_SERVICE)
            .context("start mDNS browse")?;
        let browser_thread = std::thread::spawn(move || {
            for event in receiver.iter() {
                if let ServiceEvent::ServiceResolved(info) = event {
                    let props = info.get_properties();
                    let dev_id = props
                        .get_property_val_str("device_id")
                        .map(str::to_string)
                        .unwrap_or_default();
                    if dev_id.is_empty() || dev_id == our_device_id {
                        continue;
                    }
                    let dev_name = props
                        .get_property_val_str("device_name")
                        .map(str::to_string)
                        .unwrap_or_else(|| dev_id.clone());
                    let pubkey = props
                        .get_property_val_str("pubkey")
                        .map(str::to_string)
                        .unwrap_or_default();
                    let addr = info
                        .get_addresses()
                        .iter()
                        .next()
                        .map(|a| a.to_string())
                        .unwrap_or_default();
                    let port = info.get_port();
                    let already_paired = {
                        if let Ok(conn) = db_for_browser.lock() {
                            conn.query_row(
                                "SELECT 1 FROM sync_peers WHERE device_id = ?1",
                                params![&dev_id],
                                |_| Ok(()),
                            )
                            .is_ok()
                        } else {
                            false
                        }
                    };
                    if let Ok(mut map) = nearby_clone.lock() {
                        map.insert(
                            dev_id.clone(),
                            NearbyPeer {
                                device_id: dev_id,
                                device_name: dev_name,
                                public_key_b64: pubkey,
                                addr,
                                port,
                                already_paired,
                            },
                        );
                    }
                }
            }
        });

        // 4. Spawn the axum HTTP server on the tauri async runtime.
        let pending: Arc<Mutex<Option<PendingPairCode>>> = Arc::new(Mutex::new(None));
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let app_state = Arc::new(SyncHttpState {
            db: db.clone(),
            identity: identity.clone(),
            signing: Arc::new(signing),
            pending: pending.clone(),
        });
        let std_listener = listener;
        tauri::async_runtime::spawn(async move {
            let tokio_listener = match tokio::net::TcpListener::from_std(std_listener) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[sync] tokio listener convert failed: {e}");
                    return;
                }
            };
            let router = build_router(app_state);
            let serve = axum::serve(tokio_listener, router);
            tokio::select! {
                res = serve => {
                    if let Err(e) = res {
                        eprintln!("[sync] http server error: {e}");
                    }
                }
                _ = shutdown_rx => {}
            }
        });

        eprintln!(
            "[sync] enabled — device_id={} port={} addrs={:?}",
            identity.device_id, port, local_ips
        );

        Ok(SyncService {
            identity,
            port,
            pending,
            nearby,
            shutdown: Some(shutdown_tx),
            mdns: Some(mdns),
            _browser_thread: Some(browser_thread),
        })
    }

    /// Mint a fresh 6-digit pair code that the user will read aloud /
    /// type into the OTHER device. Returns the code so the UI can
    /// display it. Replaces any prior outstanding code.
    pub fn mint_pair_code(&self) -> String {
        let mut buf = [0u8; 4];
        OsRng.fill_bytes(&mut buf);
        let n = u32::from_le_bytes(buf) % 1_000_000;
        let code = format!("{:06}", n);
        if let Ok(mut g) = self.pending.lock() {
            *g = Some(PendingPairCode {
                code: code.clone(),
                minted_at: Instant::now(),
            });
        }
        code
    }

    pub fn current_pair_code(&self) -> Option<String> {
        let g = self.pending.lock().ok()?;
        g.as_ref().and_then(|p| {
            if p.minted_at.elapsed() < PAIR_CODE_TTL {
                Some(p.code.clone())
            } else {
                None
            }
        })
    }

    pub fn nearby_peers(&self) -> Vec<NearbyPeer> {
        self.nearby
            .lock()
            .ok()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }
}

impl Drop for SyncService {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(mdns) = self.mdns.take() {
            let _ = mdns.shutdown();
        }
        // The browser thread will exit when its mdns receiver closes.
    }
}

// ── HTTP layer ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct SyncHttpState {
    db: Arc<Mutex<Connection>>,
    identity: DeviceIdentity,
    signing: Arc<SigningKey>,
    pending: Arc<Mutex<Option<PendingPairCode>>>,
}

fn build_router(state: Arc<SyncHttpState>) -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/ping", get(ping_handler))
        .route("/pair", post(pair_handler))
        .with_state(state)
}

async fn ping_handler(
    axum::extract::State(state): axum::extract::State<Arc<SyncHttpState>>,
) -> axum::Json<PingPayload> {
    axum::Json(PingPayload {
        device_id: state.identity.device_id.clone(),
        device_name: state.identity.device_name.clone(),
        public_key_b64: state.identity.public_key_b64.clone(),
        protocol_version: 1,
        app: "RetinaTag".to_string(),
    })
}

async fn pair_handler(
    axum::extract::State(state): axum::extract::State<Arc<SyncHttpState>>,
    axum::Json(req): axum::Json<PairRequest>,
) -> Result<axum::Json<PairResponse>, (axum::http::StatusCode, String)> {
    // Code must match the one we currently have outstanding AND it
    // must still be inside the TTL window.
    let current = {
        let g = state.pending.lock().map_err(|_| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "pair lock".to_string(),
            )
        })?;
        g.as_ref().and_then(|p| {
            if p.minted_at.elapsed() < PAIR_CODE_TTL {
                Some(p.code.clone())
            } else {
                None
            }
        })
    };
    let Some(expected) = current else {
        return Err((
            axum::http::StatusCode::FORBIDDEN,
            "no outstanding pair code on this device".to_string(),
        ));
    };
    if !constant_time_eq(expected.as_bytes(), req.code.as_bytes()) {
        return Err((
            axum::http::StatusCode::UNAUTHORIZED,
            "pair code mismatch".to_string(),
        ));
    }
    // Decode the requester's pubkey.
    let pk = b64_url_decode(&req.from.public_key_b64).map_err(|e| {
        (
            axum::http::StatusCode::BAD_REQUEST,
            format!("bad pubkey: {e}"),
        )
    })?;
    if pk.len() != 32 {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "pubkey must be 32 bytes".to_string(),
        ));
    }
    let now = chrono::Utc::now().timestamp();
    {
        let conn = state.db.lock().map_err(|_| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "db lock".to_string(),
            )
        })?;
        insert_peer(
            &conn,
            &req.from.device_id,
            &req.from.device_name,
            &pk,
            None,
        )
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("insert peer: {e}"),
            )
        })?;
    }
    // Burn the code so it can't be replayed.
    if let Ok(mut g) = state.pending.lock() {
        *g = None;
    }
    Ok(axum::Json(PairResponse {
        identity: state.identity.clone(),
        paired_at: now,
    }))
}

// ── Outbound HTTP client (used by sync_complete_pairing) ────────────────

/// Talk to a peer's `/ping` endpoint to learn their identity before
/// we commit to pairing.
pub async fn ping_peer(addr: &str) -> Result<PingPayload> {
    let url = format!("http://{}/ping", addr);
    let resp = reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .context("ping send")?;
    if !resp.status().is_success() {
        anyhow::bail!("ping http {}", resp.status());
    }
    let payload = resp.json::<PingPayload>().await.context("ping decode")?;
    if payload.app != "RetinaTag" {
        anyhow::bail!("not a RetinaTag peer (app={:?})", payload.app);
    }
    Ok(payload)
}

/// Send our identity + the code the user just read to a peer's /pair
/// endpoint. On success the peer returns their identity, which the
/// caller persists.
pub async fn pair_with_peer(
    addr: &str,
    code: &str,
    me: &DeviceIdentity,
) -> Result<PairResponse> {
    let url = format!("http://{}/pair", addr);
    let body = PairRequest {
        from: me.clone(),
        code: code.to_string(),
    };
    let resp = reqwest::Client::new()
        .post(url)
        .timeout(Duration::from_secs(8))
        .json(&body)
        .send()
        .await
        .context("pair send")?;
    let status = resp.status();
    if !status.is_success() {
        let msg = resp.text().await.unwrap_or_default();
        anyhow::bail!("pair http {}: {}", status, msg);
    }
    let payload = resp.json::<PairResponse>().await.context("pair decode")?;
    Ok(payload)
}

// ── Crypto helpers ─────────────────────────────────────────────────────

/// Sign `payload` with our identity key. Phase-2 sync verbs will use
/// this for every outbound delta so the peer can verify provenance.
pub fn sign_envelope(signing: &SigningKey, payload: &[u8]) -> String {
    let sig = signing.sign(payload);
    b64_url(&sig.to_bytes())
}

/// Verify a signature produced by `sign_envelope`. Returns Ok(()) on
/// match, Err otherwise. Used Phase-2-side.
#[allow(dead_code)]
pub fn verify_envelope(
    pubkey: &[u8],
    payload: &[u8],
    signature_b64: &str,
) -> Result<()> {
    let pk_arr: [u8; 32] = pubkey
        .try_into()
        .map_err(|_| anyhow::anyhow!("pubkey must be 32 bytes"))?;
    let vk = VerifyingKey::from_bytes(&pk_arr)?;
    let sig_bytes = b64_url_decode(signature_b64)?;
    if sig_bytes.len() != 64 {
        anyhow::bail!("signature must be 64 bytes");
    }
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sig_bytes);
    let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
    vk.verify(payload, &sig)?;
    Ok(())
}

// ── Encoding helpers ───────────────────────────────────────────────────

fn b64_url(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(bytes)
}

fn b64_url_decode(s: &str) -> Result<Vec<u8>> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    Ok(URL_SAFE_NO_PAD.decode(s)?)
}

/// Length-and-content constant-time compare. Standard `==` on `&[u8]`
/// short-circuits on the first byte diff which is a side-channel
/// timing leak. Pair codes are 6 ASCII chars so the leak is small —
/// using constant_time_eq anyway because it's free.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
