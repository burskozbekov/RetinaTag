//! v1.5.264 — LAN HTTP server skeleton (parity with Mac v1.5.242).
//! Future impl will spin axum on 0.0.0.0:9876 with /api/ping,
//! /api/pair/request, /api/pair/complete, /api/upload (multipart,
//! 2 GB body limit, stream-to-disk). Real wiring in v1.5.266+.

#![allow(dead_code)]

/// Stub: returns the planned bind port until the real server lands.
pub fn planned_port() -> u16 {
    9876
}
