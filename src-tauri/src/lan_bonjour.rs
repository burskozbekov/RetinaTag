//! v1.5.267 — Bonjour / mDNS service advertise. Parity with Mac v1.5.242.
//!
//! Advertises `_retinatag._tcp.local.` on the LAN so the shared iOS
//! app's Bonjour browser discovers this desktop without the user
//! typing an IP address. TXT records carry version + platform +
//! hostname so the client can pick the right host when multiple
//! Macs + PCs are on the network.
//!
//! Best-effort: if mdns-sd fails to bind, we log and exit the
//! advertise task — the HTTP server keeps running, the iOS app can
//! still connect manually by IP if needed.

#![allow(dead_code)]

use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;
use tokio::task::JoinHandle;

pub const SERVICE_TYPE: &str = "_retinatag._tcp.local.";

/// Best-effort hostname for the advertised instance name. Matches
/// the lan_server hostname for consistency.
fn hostname_safe() -> String {
    if let Ok(s) = std::env::var("COMPUTERNAME") {
        if !s.trim().is_empty() {
            return s;
        }
    }
    "RetinaTag-PC".to_string()
}

/// Best-effort first non-loopback IPv4 of the host. Returns
/// 0.0.0.0 if we can't enumerate; mdns-sd then falls back to
/// listening on all interfaces and using whichever it sees a
/// packet on.
fn first_local_ipv4() -> std::net::Ipv4Addr {
    use std::net::{IpAddr, UdpSocket};
    let s = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return std::net::Ipv4Addr::UNSPECIFIED,
    };
    // Connecting a UDP socket doesn't send any packet; it just
    // tells the OS to pick a route, which gives us the local IP
    // it would use.
    if s.connect("8.8.8.8:53").is_ok() {
        if let Ok(local) = s.local_addr() {
            if let IpAddr::V4(v4) = local.ip() {
                return v4;
            }
        }
    }
    std::net::Ipv4Addr::UNSPECIFIED
}

/// Spin the mDNS advertiser. Holds a `ServiceDaemon` alive inside
/// the spawned task; dropping the JoinHandle cancels the task and
/// releases the daemon (which sends a goodbye packet on unregister).
pub fn start_advertise(port: u16) -> Result<JoinHandle<()>, String> {
    let daemon = ServiceDaemon::new().map_err(|e| format!("mdns ServiceDaemon: {e}"))?;
    let hostname = hostname_safe();
    let mut props: HashMap<String, String> = HashMap::new();
    props.insert("version".into(),  env!("CARGO_PKG_VERSION").into());
    props.insert("platform".into(), "windows".into());
    props.insert("hostname".into(), hostname.clone());

    // Fully qualified hostname expected by mdns-sd: "<host>.local."
    let host_fqdn = format!("{}.local.", hostname.to_lowercase().replace(' ', "-"));
    let ip = first_local_ipv4();

    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &hostname,
        &host_fqdn,
        std::net::IpAddr::V4(ip),
        port,
        props,
    )
    .map_err(|e| format!("mdns ServiceInfo: {e}"))?;

    daemon
        .register(info)
        .map_err(|e| format!("mdns register: {e}"))?;

    // Keep the daemon alive by moving it into the task. If the task
    // is aborted (JoinHandle drop), `daemon` drops too, which calls
    // unregister + sends goodbye.
    let handle = tokio::spawn(async move {
        // Park forever; the daemon does its own background work
        // inside its own threads, we just need to hold the handle.
        let _d = daemon;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
    Ok(handle)
}
