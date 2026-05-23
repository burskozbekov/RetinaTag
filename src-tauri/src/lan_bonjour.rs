//! v1.5.264 — mDNS / Bonjour advertise skeleton (parity with Mac
//! v1.5.242). Future impl will use mdns-sd ServiceDaemon to advertise
//! `_retinatag._tcp` with TXT records (version, platform, hostname)
//! so the shared iOS app's Bonjour browser finds Windows desktops
//! identically to Macs. Real impl in v1.5.267.

#![allow(dead_code)]

/// Stub: service type the future ServiceDaemon will advertise.
pub fn service_type() -> &'static str {
    "_retinatag._tcp.local."
}
