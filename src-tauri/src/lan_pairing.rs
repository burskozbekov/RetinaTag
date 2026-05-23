//! v1.5.265 — LAN pairing impl. Parity with Mac v1.5.242.
//!
//! Flow:
//!  1. Desktop UI calls `mint_code()` → 6-digit decimal string. Code
//!     is held in a process-local HashMap with a 5-minute TTL. NOT
//!     persisted — if the desktop restarts, all pending codes die.
//!  2. iOS app POSTs the code to /api/pair/complete with a device
//!     name. Server calls `complete_pairing(...)`. The function
//!     consumes the code (single-use), mints a 32-byte hex bearer
//!     token, hashes it with SHA-256, and inserts (device_name,
//!     hash, created_at) into `paired_devices`. Returns the
//!     plaintext token ONCE — the desktop never stores it.
//!  3. Subsequent iOS requests send the bearer in `Authorization:
//!     Bearer <hex>`. Server calls `verify_token(...)`; we hash and
//!     look up. On hit we bump `last_seen_at` and return device id.
//!  4. `list_paired` / `revoke` power the Settings UI list.
//!
//! Module state (the in-memory pending-code map) is a process-wide
//! `Mutex<HashMap>` behind a `OnceLock`. The DB table is created by
//! `db::init_db` so no migration choreography needed.

use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// In-memory pending-code map. Keys are the 6-digit codes; values
/// are the wall-clock expiry instant. We sweep expired entries on
/// every mint and every complete to bound memory.
fn pending() -> &'static Mutex<HashMap<String, Instant>> {
    static M: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 5-minute TTL for a freshly-minted pairing code.
const CODE_TTL: Duration = Duration::from_secs(5 * 60);

/// 6-digit decimal numeric code, zero-padded. Mac uses the same format.
fn random_code() -> String {
    use rand::Rng;
    let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
    format!("{:06}", n)
}

/// 32 random bytes → 64-char lowercase hex. The plaintext we hand back
/// to the iOS app once at pairing-complete time.
fn random_bearer_hex() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

fn now_iso() -> String {
    chrono::Local::now().to_rfc3339()
}

/// Drop entries whose TTL has expired. Called from mint and complete.
fn sweep_expired(map: &mut HashMap<String, Instant>) {
    let now = Instant::now();
    map.retain(|_, &mut exp| exp > now);
}

/// Mint a fresh 6-digit code. Held for 5 min then discarded. Caller
/// (the Settings → iPhone Companion modal on desktop) displays the
/// code to the user along with a countdown.
pub fn mint_code() -> String {
    let mut map = pending().lock().expect("pending mutex poisoned");
    sweep_expired(&mut map);
    // Reroll on the astronomically-unlikely collision so two
    // simultaneous mints can't return the same code.
    loop {
        let code = random_code();
        if !map.contains_key(&code) {
            map.insert(code.clone(), Instant::now() + CODE_TTL);
            return code;
        }
    }
}

/// Result returned by `complete_pairing`.
#[derive(Debug)]
pub struct PairingComplete {
    pub device_id:    i64,
    /// The plaintext bearer token. The caller MUST hand this back to
    /// the iOS app and forget it — we keep only the SHA-256 hash.
    pub bearer_token: String,
}

/// iOS app submits (code, device_name). On success we issue + persist
/// a token and return the plaintext bearer once.
pub fn complete_pairing(
    conn: &Connection,
    code: &str,
    device_name: &str,
) -> Result<PairingComplete, String> {
    let code = code.trim();
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err("Code must be 6 digits.".into());
    }
    // Consume the pending entry under lock so a replayed code is rejected.
    {
        let mut map = pending().lock().map_err(|_| "pending mutex poisoned")?;
        sweep_expired(&mut map);
        match map.remove(code) {
            Some(_) => {} // valid, consumed
            None    => return Err("Code not found or expired.".into()),
        }
    }
    let device_name = device_name.trim();
    if device_name.is_empty() || device_name.len() > 64 {
        return Err("Device name must be 1–64 chars.".into());
    }
    let token = random_bearer_hex();
    let token_hash = sha256_hex(&token);
    let now = now_iso();
    conn.execute(
        "INSERT INTO paired_devices (device_name, token_hash, created_at, last_seen_at)
         VALUES (?1, ?2, ?3, NULL)",
        params![device_name, &token_hash, &now],
    )
    .map_err(|e| format!("persist pairing: {e}"))?;
    let device_id = conn.last_insert_rowid();
    Ok(PairingComplete { device_id, bearer_token: token })
}

/// Hash the incoming bearer, look up, bump last_seen_at. Returns the
/// row id if valid, or None for any failure (callers should map None
/// → HTTP 401).
pub fn verify_token(conn: &Connection, bearer: &str) -> Option<i64> {
    let bearer = bearer.trim();
    if bearer.len() != 64 || !bearer.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let h = sha256_hex(bearer);
    let id: Option<i64> = conn
        .query_row(
            "SELECT id FROM paired_devices WHERE token_hash = ?1",
            params![&h],
            |r| r.get(0),
        )
        .ok();
    if let Some(rid) = id {
        let _ = conn.execute(
            "UPDATE paired_devices SET last_seen_at = ?1 WHERE id = ?2",
            params![now_iso(), rid],
        );
    }
    id
}

/// Row shape for the Settings UI list.
#[derive(Debug, Serialize)]
pub struct PairedDevice {
    pub id:           i64,
    pub device_name:  String,
    pub created_at:   String,
    pub last_seen_at: Option<String>,
}

pub fn list_paired(conn: &Connection) -> Result<Vec<PairedDevice>, String> {
    let mut stmt = conn
        .prepare("SELECT id, device_name, created_at, last_seen_at FROM paired_devices ORDER BY id DESC")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PairedDevice {
                id:           r.get(0)?,
                device_name:  r.get(1)?,
                created_at:   r.get(2)?,
                last_seen_at: r.get(3).ok(),
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        if let Ok(r) = row { out.push(r); }
    }
    Ok(out)
}

pub fn revoke(conn: &Connection, device_id: i64) -> Result<(), String> {
    conn.execute("DELETE FROM paired_devices WHERE id = ?1", params![device_id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE paired_devices (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                device_name TEXT NOT NULL,
                token_hash TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL,
                last_seen_at TEXT
            );",
        )
        .unwrap();
        c
    }

    #[test]
    fn mint_returns_6_digits() {
        let code = mint_code();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn complete_with_bad_code_rejects() {
        let conn = fresh_conn();
        assert!(complete_pairing(&conn, "999999", "iPhone").is_err());
    }

    #[test]
    fn complete_with_valid_code_issues_token() {
        let conn = fresh_conn();
        let code = mint_code();
        let r = complete_pairing(&conn, &code, "iPhone of Test").unwrap();
        assert_eq!(r.bearer_token.len(), 64);
        // Replay should now fail (single-use).
        assert!(complete_pairing(&conn, &code, "iPhone of Test").is_err());
        // Token verifies.
        assert_eq!(verify_token(&conn, &r.bearer_token), Some(r.device_id));
        // Wrong token fails.
        assert_eq!(verify_token(&conn, &"f".repeat(64)), None);
        // List has one row.
        let rows = list_paired(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device_name, "iPhone of Test");
        // Revoke.
        revoke(&conn, r.device_id).unwrap();
        assert_eq!(list_paired(&conn).unwrap().len(), 0);
    }
}
