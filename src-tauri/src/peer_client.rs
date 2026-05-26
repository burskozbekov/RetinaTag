//! v1.5.312 — HTTP client for talking to OTHER RetinaTag desktops on
//! the LAN. Symmetric counterpart to `lan_server.rs` (which is the
//! server we run for them). All endpoints + the bearer-token scheme
//! match the contract Mac shipped (see pc-status.txt in the shared
//! coord directory and lan_server.rs::run_server for the matching
//! routes once we mirror them).
//!
//! Foundation release: this file defines the struct + the request /
//! response shapes; no Tauri command calls it yet. v1.5.313 wires the
//! `Pair` button on each discovered peer to `pair_request` /
//! `pair_complete`; v1.5.314 plugs `list_photos` into a Browse modal;
//! v1.5.315 piggybacks the existing lightbox onto `get_thumb` /
//! `get_photo`.
//!
//! Why a separate module instead of stuffing this into commands.rs:
//!   • Keeps the HTTP request / response types co-located with the
//!     server-side shapes (when lan_server grows the matching
//!     endpoints in v1.5.318, both halves of the contract live in
//!     two files that are easy to diff side-by-side).
//!   • Reqwest's `Client` builds a connection pool; sharing one
//!     `Client` across peer requests via `PeerClient` lets every
//!     call to the same Mac reuse the same TCP socket, which
//!     matters for the rapid thumbnail-fetch loop the gallery
//!     view will trigger.
//!   • Tokio's HTTP plumbing wants Send futures; that's awkward to
//!     express on free functions inside the giant commands.rs.

#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Default timeout for non-streaming JSON calls.  Long enough that a
/// busy Mac doing CLIP indexing still gets to respond, short enough
/// that a vanished peer doesn't hang the UI for minutes.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One HTTP client bound to a single peer (one Mac, one other PC).
/// Cheap to clone (reqwest's `Client` is Arc-wrapped internally) so
/// the future `PeerRegistry` can hand out per-peer handles without
/// rebuilding the connection pool.
#[derive(Clone, Debug)]
pub struct PeerClient {
    /// `http://<addr>:<port>` — built once at construction so every
    /// endpoint helper just appends the path.
    base: String,
    /// Bearer token from a completed pair.  None until `pair_complete`
    /// returns; calls that need auth will error with `Unauthorised` if
    /// this is None at use-time.
    token: Option<String>,
    client: reqwest::Client,
}

/// JSON body sent to `POST /api/pair/request`.  Mac shows a 6-digit
/// code modal on its UI when this lands.
#[derive(Debug, Serialize)]
pub struct PairRequest<'a> {
    pub device_name: &'a str,
}

/// JSON body sent to `POST /api/pair/complete`.
#[derive(Debug, Serialize)]
pub struct PairComplete<'a> {
    pub code: &'a str,
    pub device_name: &'a str,
}

/// JSON returned from a successful `POST /api/pair/complete`.
#[derive(Debug, Deserialize)]
pub struct PairCompleteResponse {
    pub token: String,
}

/// `GET /api/vault/status` shape.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct VaultStatus {
    pub has_pin: bool,
    pub unlocked: bool,
}

/// `POST /api/vault/unlock` body.
#[derive(Debug, Serialize)]
pub struct VaultUnlock<'a> {
    pub pin: &'a str,
}

/// One row from `GET /api/photos`.  Field set matches the local
/// `PhotoSummary` so the same gallery rendering code can be reused on
/// the remote-view modal once v1.5.314 wires it up.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PeerPhoto {
    pub id: i64,
    pub filename: String,
    #[serde(default)]
    pub date_taken: Option<String>,
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// Extra fields Mac may or may not include depending on its
    /// version.  Leaving them out keeps the deserializer permissive.
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// `GET /api/photos` envelope.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PhotoListResponse {
    pub photos: Vec<PeerPhoto>,
    pub total: i64,
    #[serde(default)]
    pub offset: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
}

impl PeerClient {
    /// Construct from the addr + port that the Bonjour browser saw.
    /// No network IO happens here.
    pub fn new(addr: &str, port: u16) -> Self {
        let base = format!("http://{addr}:{port}");
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            // Reqwest's builder can fail if the user has a corrupt
            // system trust store; fall back to default() so we at
            // least keep the foundation usable.
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { base, token: None, client }
    }

    /// Attach a previously-stored bearer token to a fresh client.
    /// Used after `pair_complete` returns the token AND on every
    /// subsequent app launch when the token is rehydrated from DB.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Health probe — `GET /api/ping`.  Returns true on 200, false on
    /// timeout / non-200.  Used by v1.5.313's UI to quickly check the
    /// peer is alive before opening the pair dialog.
    pub async fn ping(&self) -> bool {
        match self.client
            .get(format!("{}/api/ping", self.base))
            .send()
            .await
        {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }

    /// `POST /api/pair/request {device_name}` — triggers the 6-digit
    /// code modal on the remote.  Returns Ok(()) on 200/202; non-200
    /// becomes an Err so the UI can surface "Mac is locked / offline".
    pub async fn pair_request(&self, device_name: &str) -> Result<()> {
        let r = self
            .client
            .post(format!("{}/api/pair/request", self.base))
            .json(&PairRequest { device_name })
            .send()
            .await
            .context("pair_request: send")?;
        if !r.status().is_success() {
            return Err(anyhow!("pair_request: HTTP {}", r.status()));
        }
        Ok(())
    }

    /// `POST /api/pair/complete {code, device_name}` — user has read
    /// the code off the remote's screen and typed it here.  Returns
    /// the bearer token on success.
    pub async fn pair_complete(
        &self,
        code: &str,
        device_name: &str,
    ) -> Result<String> {
        let r = self
            .client
            .post(format!("{}/api/pair/complete", self.base))
            .json(&PairComplete { code, device_name })
            .send()
            .await
            .context("pair_complete: send")?;
        let status = r.status();
        if !status.is_success() {
            let body = r.text().await.unwrap_or_default();
            return Err(anyhow!("pair_complete: HTTP {status} — {body}"));
        }
        let parsed: PairCompleteResponse = r
            .json()
            .await
            .context("pair_complete: json")?;
        Ok(parsed.token)
    }

    /// `GET /api/vault/status` (auth required).
    pub async fn vault_status(&self) -> Result<VaultStatus> {
        let token = self.token.as_deref().ok_or_else(|| anyhow!("no token"))?;
        let r = self
            .client
            .get(format!("{}/api/vault/status", self.base))
            .bearer_auth(token)
            .send()
            .await
            .context("vault_status: send")?;
        let status = r.status();
        if !status.is_success() {
            return Err(anyhow!("vault_status: HTTP {status}"));
        }
        let parsed: VaultStatus = r.json().await.context("vault_status: json")?;
        Ok(parsed)
    }

    /// `POST /api/vault/unlock {pin}` (auth required).
    pub async fn vault_unlock(&self, pin: &str) -> Result<()> {
        let token = self.token.as_deref().ok_or_else(|| anyhow!("no token"))?;
        let r = self
            .client
            .post(format!("{}/api/vault/unlock", self.base))
            .bearer_auth(token)
            .json(&VaultUnlock { pin })
            .send()
            .await
            .context("vault_unlock: send")?;
        let status = r.status();
        if status.is_success() {
            return Ok(());
        }
        // Mac's spec: 401 = wrong PIN, 429 = lockout.  Forward the
        // status verbatim so the UI can show the right message.
        if status.as_u16() == 401 {
            return Err(anyhow!("wrong PIN"));
        }
        if status.as_u16() == 429 {
            return Err(anyhow!("locked out — try later"));
        }
        Err(anyhow!("vault_unlock: HTTP {status}"))
    }

    /// `POST /api/vault/lock` (auth required).  No body either way.
    pub async fn vault_lock(&self) -> Result<()> {
        let token = self.token.as_deref().ok_or_else(|| anyhow!("no token"))?;
        let r = self
            .client
            .post(format!("{}/api/vault/lock", self.base))
            .bearer_auth(token)
            .send()
            .await
            .context("vault_lock: send")?;
        if !r.status().is_success() {
            return Err(anyhow!("vault_lock: HTTP {}", r.status()));
        }
        Ok(())
    }

    /// `GET /api/photos?vault_only=<bool>&offset=N&limit=M` (auth).
    /// `vault_only=true` requires the peer's vault to be unlocked or
    /// the call comes back 401.
    pub async fn list_photos(
        &self,
        vault_only: bool,
        offset: i64,
        limit: i64,
    ) -> Result<PhotoListResponse> {
        let token = self.token.as_deref().ok_or_else(|| anyhow!("no token"))?;
        let r = self
            .client
            .get(format!("{}/api/photos", self.base))
            .bearer_auth(token)
            .query(&[
                ("vault_only", vault_only.to_string()),
                ("offset", offset.to_string()),
                ("limit", limit.to_string()),
            ])
            .send()
            .await
            .context("list_photos: send")?;
        let status = r.status();
        if !status.is_success() {
            return Err(anyhow!("list_photos: HTTP {status}"));
        }
        let parsed: PhotoListResponse = r.json().await.context("list_photos: json")?;
        Ok(parsed)
    }

    /// `GET /api/thumb/{id}` (auth).  Returns JPEG bytes.
    pub async fn get_thumb(&self, id: i64) -> Result<Vec<u8>> {
        let token = self.token.as_deref().ok_or_else(|| anyhow!("no token"))?;
        let r = self
            .client
            .get(format!("{}/api/thumb/{id}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .context("get_thumb: send")?;
        let status = r.status();
        if !status.is_success() {
            return Err(anyhow!("get_thumb: HTTP {status}"));
        }
        Ok(r.bytes().await.context("get_thumb: read")?.to_vec())
    }

    /// `GET /api/photo/{id}` (auth).  Returns the raw bytes (JPEG,
    /// HEIC, MOV, …) the peer has on disk.  Range support is wired in
    /// `get_photo_range` for video streaming.
    pub async fn get_photo(&self, id: i64) -> Result<Vec<u8>> {
        let token = self.token.as_deref().ok_or_else(|| anyhow!("no token"))?;
        let r = self
            .client
            .get(format!("{}/api/photo/{id}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .context("get_photo: send")?;
        let status = r.status();
        if !status.is_success() {
            return Err(anyhow!("get_photo: HTTP {status}"));
        }
        Ok(r.bytes().await.context("get_photo: read")?.to_vec())
    }

    /// Range-aware variant of `get_photo` for video streaming.
    /// `range_hdr` should look like "bytes=0-65535".  The peer must
    /// respond 206 Partial Content; 200 means it doesn't honour Range
    /// and we got the full body.  Caller's job to thread the bytes +
    /// the Content-Range header back to the HTML5 <video> element.
    pub async fn get_photo_range(
        &self,
        id: i64,
        range_hdr: &str,
    ) -> Result<(reqwest::StatusCode, Option<String>, Vec<u8>)> {
        let token = self.token.as_deref().ok_or_else(|| anyhow!("no token"))?;
        let r = self
            .client
            .get(format!("{}/api/photo/{id}", self.base))
            .bearer_auth(token)
            .header("Range", range_hdr)
            .send()
            .await
            .context("get_photo_range: send")?;
        let status = r.status();
        let content_range = r
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = r.bytes().await.context("get_photo_range: read")?.to_vec();
        Ok((status, content_range, bytes))
    }
}
