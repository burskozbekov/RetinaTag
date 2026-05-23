//! v1.5.264 — LAN pairing skeleton (iPhone Companion, parity with Mac
//! v1.5.242). Real impl lands in a follow-up release; this file exists
//! so lib.rs can `mod lan_pairing;` without a "no such file" error.
//!
//! Mac's reference impl in mac-status.txt:
//!   - 6-digit code generated on demand, 5-min TTL, single-use.
//!   - On complete: issue a 32-byte hex bearer token, SHA-256 of token
//!     persisted in `paired_devices(id, device_name, token_hash,
//!     created_at, last_seen_at)`.
//!   - Verification: hash the bearer received over HTTP, look up,
//!     update last_seen_at.

#![allow(dead_code)]

/// Stub: returns a placeholder until the real impl lands.
pub fn placeholder() -> &'static str {
    "lan_pairing v1.5.264 stub"
}
