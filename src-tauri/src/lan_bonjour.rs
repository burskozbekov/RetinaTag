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

// ─────────────────────────────────────────────────────────────────────
// v1.5.310 — Bonjour BROWSER side.  start_advertise (above) tells the
// world we exist; this side discovers the OTHER RetinaTag desktops
// (Macs + PCs) on the same LAN.  The pair is symmetric: both sides
// advertise + browse, so PC sees Mac sees PC.
// ─────────────────────────────────────────────────────────────────────

use std::sync::{Mutex, OnceLock};

/// One peer discovered on the LAN. Cloned into the Tauri event payload.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LanPeer {
    /// Bonjour instance name (e.g. "Bugras-MacBook-Pro").  Stable for
    /// the lifetime of the remote daemon; doubles as the dedup key.
    pub name: String,
    /// First non-loopback IPv4 the peer advertises.  Used to build
    /// `http://addr:port` URLs for the LAN HTTP API.
    pub addr: String,
    pub port: u16,
    pub hostname: String,
    pub platform: String,
    pub version: String,
    /// Unix-second timestamp of the last advertise this side received.
    /// The browser refreshes peers periodically; entries older than
    /// ~3× advertise interval are considered dead.
    pub last_seen: i64,
}

/// Single source of truth for the peer set, owned by the browser task
/// and read back by the `lan_list_peers` Tauri command.  Mutex (not
/// Tokio-async) because every access is a one-line read or write —
/// nothing blocks long enough to need an async lock.  `OnceLock`
/// matches the rest of this codebase (see lan_pairing.rs) — we avoid
/// adding `once_cell` as a dep since std now ships the same pattern.
static PEERS: OnceLock<Mutex<HashMap<String, LanPeer>>> = OnceLock::new();

fn peers_map() -> &'static Mutex<HashMap<String, LanPeer>> {
    PEERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Spawn the Bonjour browser.  Forwards every discovery event into the
/// PEERS map and emits `lan-peer-found` / `lan-peer-lost` so the
/// frontend can update its peer list without polling.
pub fn start_browse(app_handle: tauri::AppHandle) -> Result<JoinHandle<()>, String> {
    use mdns_sd::ServiceEvent;
    use tauri::Emitter;

    let daemon = ServiceDaemon::new()
        .map_err(|e| format!("mdns ServiceDaemon (browse): {e}"))?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| format!("mdns browse: {e}"))?;

    // Skip OUR OWN advertise.  ServiceInfo.fullname is
    // "<hostname>.<service_type>", so an exact match means we
    // bounced our own announce off mdns-sd.
    let self_name = hostname_safe();

    let handle = tokio::spawn(async move {
        // Hold the daemon alive for the lifetime of this task; dropping
        // it stops the browser and releases the socket.
        let _d = daemon;
        loop {
            // recv() blocks the mdns-sd worker thread; we await its
            // crossbeam channel via a small bridge.  The `try_recv`
            // loop drains everything queued, then sleep briefly.
            while let Ok(ev) = receiver.try_recv() {
                match ev {
                    ServiceEvent::ServiceResolved(info) => {
                        let name = info.get_fullname().to_string();
                        // Strip the service-type suffix for display
                        // ("Bugra-MacBook-Pro._retinatag._tcp.local." →
                        //  "Bugra-MacBook-Pro").
                        let display = name
                            .split('.')
                            .next()
                            .unwrap_or(&name)
                            .to_string();
                        if display.eq_ignore_ascii_case(&self_name) {
                            continue;
                        }
                        // Pick the first IPv4 address; v6 is fine too
                        // but the existing HTTP server is bound to
                        // v4 first by `first_local_ipv4` on both sides.
                        let addr = info
                            .get_addresses()
                            .iter()
                            .find_map(|a| match a {
                                std::net::IpAddr::V4(v4) => Some(v4.to_string()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        if addr.is_empty() {
                            continue;
                        }
                        let port = info.get_port();
                        let props = info.get_properties();
                        let mut hostname = String::new();
                        let mut platform = String::new();
                        let mut version = String::new();
                        for prop in props.iter() {
                            match prop.key() {
                                "hostname" => hostname = prop.val_str().to_string(),
                                "platform" => platform = prop.val_str().to_string(),
                                "version"  => version  = prop.val_str().to_string(),
                                _ => {}
                            }
                        }
                        let now = chrono::Utc::now().timestamp();
                        let peer = LanPeer {
                            name: display.clone(),
                            addr,
                            port,
                            hostname,
                            platform,
                            version,
                            last_seen: now,
                        };
                        {
                            let mut map = peers_map().lock().unwrap_or_else(|e| e.into_inner());
                            map.insert(display.clone(), peer.clone());
                        }
                        let _ = app_handle.emit("lan-peer-found", &peer);
                    }
                    ServiceEvent::ServiceRemoved(_ty, fullname) => {
                        let display = fullname
                            .split('.')
                            .next()
                            .unwrap_or(&fullname)
                            .to_string();
                        let removed = {
                            let mut map = peers_map().lock().unwrap_or_else(|e| e.into_inner());
                            map.remove(&display)
                        };
                        if removed.is_some() {
                            let _ = app_handle.emit("lan-peer-lost", &display);
                        }
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });
    Ok(handle)
}

/// Snapshot of currently-known peers.  Tauri commands call into this
/// instead of locking PEERS directly so the lock scope stays inside
/// this module.
pub fn snapshot_peers() -> Vec<LanPeer> {
    let map = peers_map().lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<LanPeer> = map.values().cloned().collect();
    // Stable order so the UI doesn't reshuffle every refresh.
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

// ─────────────────────────────────────────────────────────────────────
// v1.5.317 — `dns-sd.exe` subprocess fallback.
//
// `mdns-sd` (the Rust crate) competes with Apple's `mDNSResponder`
// for UDP/5353 on Windows when Bonjour Service is installed (iTunes,
// Bonjour Print Services, Xcode, etc. all install it).  Apple's
// daemon wins the multicast filter race in practice on the user's
// machine — we'd see our own advertise echo but never the peer's
// announce.  The OS-level CLI `dns-sd.exe` talks DIRECTLY to
// mDNSResponder, so it sees every service Apple Bonjour sees.
//
// This task spawns `dns-sd.exe -B _retinatag._tcp` once, parses the
// "Add/Rmv" lines from its stdout, and for each Add runs a one-shot
// `dns-sd.exe -L <name> _retinatag._tcp local` to resolve host +
// port + TXT records.  Resolved peers go into the same PEERS map
// the mdns-sd browser uses, so the FE gets identical events
// regardless of which backend won.
//
// Best-effort: if dns-sd.exe isn't on PATH (user never installed
// Bonjour Print Services / iTunes) we log and exit; the in-process
// mdns-sd browser is still running and might pick things up.
// ─────────────────────────────────────────────────────────────────────

pub fn start_browse_dnssd(app_handle: tauri::AppHandle) -> Result<tokio::task::JoinHandle<()>, String> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use std::io::{BufRead, BufReader};
    use tauri::Emitter;

    // Quick existence probe — if dns-sd isn't there, bail before we
    // spawn anything.
    let _ = std::process::Command::new("dns-sd.exe")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .status()
        .map_err(|e| format!("dns-sd.exe not found: {e}"))?;

    let self_name = hostname_safe();

    let handle = tokio::task::spawn_blocking(move || {
        let mut child = match std::process::Command::new("dns-sd.exe")
            .args(["-B", SERVICE_TYPE.trim_end_matches('.'), "local"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[lan/dnssd] spawn failed: {e}");
                return;
            }
        };
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => return,
        };
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { continue };
            // dns-sd -B output sample:
            //   Browsing for _retinatag._tcp
            //   Timestamp     A/R Flags if Domain   Service Type     Instance Name
            //    3:41:36.880  Add     2  9 local.   _retinatag._tcp. KOMPEDER
            //
            // We only care about Add / Rmv rows after the header.
            let trimmed = line.trim();
            if !trimmed.contains("Add") && !trimmed.contains("Rmv") {
                continue;
            }
            // Tokenize on whitespace; the last field is the instance
            // name (may contain spaces in pathological cases, but the
            // standard Mac/PC names don't).
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() < 6 {
                continue;
            }
            // Layout: [time, A/R, flags, if, domain, type, name...]
            let action = parts[1];
            let name = parts[parts.len() - 1].to_string();
            // Skip our own advertise.
            if name.eq_ignore_ascii_case(&self_name) {
                continue;
            }
            if action == "Rmv" {
                let removed = {
                    let mut map = peers_map().lock().unwrap_or_else(|e| e.into_inner());
                    map.remove(&name)
                };
                if removed.is_some() {
                    let _ = app_handle.emit("lan-peer-lost", &name);
                }
                continue;
            }
            // action == "Add" — resolve the service to host+port+TXT.
            let resolved = resolve_via_dnssd(&name);
            if let Some(peer) = resolved {
                {
                    let mut map = peers_map().lock().unwrap_or_else(|e| e.into_inner());
                    map.insert(name.clone(), peer.clone());
                }
                let _ = app_handle.emit("lan-peer-found", &peer);
            }
        }
        // dns-sd exited unexpectedly — kill the child if still alive
        // and log; the in-process mdns-sd browser remains as backup.
        let _ = child.kill();
        eprintln!("[lan/dnssd] subprocess exited");
    });
    Ok(handle)
}

/// One-shot `dns-sd -L` call returning the host+port+TXT for a single
/// instance name.  Synchronous; runs inside the spawn_blocking task.
fn resolve_via_dnssd(instance_name: &str) -> Option<LanPeer> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use std::time::Duration;

    // dns-sd -L doesn't exit on its own — it streams updates.  We
    // need to spawn it, read until we see a "can be reached at"
    // line + the TXT block, then kill.
    let mut child = std::process::Command::new("dns-sd.exe")
        .args(["-L", instance_name, SERVICE_TYPE.trim_end_matches('.'), "local"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let reader = std::io::BufReader::new(stdout);
    use std::io::BufRead;

    let mut host = String::new();
    let mut port: u16 = 0;
    let mut hostname = String::new();
    let mut platform = String::new();
    let mut version = String::new();
    let mut got_target = false;

    let deadline = std::time::Instant::now() + Duration::from_secs(3);

    for line in reader.lines() {
        if std::time::Instant::now() > deadline {
            break;
        }
        let Ok(line) = line else { continue };
        let trimmed = line.trim();
        // Sample line:
        //   "Bugras-MBP.local.:9876 can be reached at Bugras-MBP.local.:9876 (interface 9)"
        if trimmed.contains("can be reached at") {
            // Extract "host:port" from the right-hand side of "at ".
            if let Some(at_idx) = trimmed.find("can be reached at ") {
                let rest = &trimmed[at_idx + "can be reached at ".len()..];
                let endpoint = rest.split_whitespace().next().unwrap_or("");
                if let Some(colon) = endpoint.rfind(':') {
                    host = endpoint[..colon].trim_end_matches('.').to_string();
                    port = endpoint[colon + 1..].parse().unwrap_or(0);
                    got_target = true;
                }
            }
            continue;
        }
        // TXT lines look like:  hostname=KOMPEDER
        //                       platform=windows
        //                       version=1.5.316
        if let Some(eq) = trimmed.find('=') {
            let key = trimmed[..eq].trim();
            let val_raw = trimmed[eq + 1..].trim();
            // dns-sd wraps TXT values in quotes sometimes; strip.
            let val = val_raw.trim_matches('"').to_string();
            match key {
                "hostname" => hostname = val,
                "platform" => platform = val,
                "version"  => version  = val,
                _ => {}
            }
            // dns-sd typically dumps all TXT lines together with the
            // target.  Once we have target + at least one TXT key,
            // we can break early — saves a few hundred ms per peer.
            if got_target && !hostname.is_empty() {
                break;
            }
        }
    }
    let _ = child.kill();

    if !got_target || host.is_empty() || port == 0 {
        return None;
    }
    // Resolve host to an IPv4.  `host` is typically "<name>.local.";
    // we ask std::net to do the lookup, falling through to the bare
    // host string if it can't (PC's hosts file or mDNS resolver
    // handles .local. on most setups).
    let addr = (host.as_str(), port).to_socket_addrs()
        .ok()
        .and_then(|mut iter| iter.find_map(|a| match a {
            std::net::SocketAddr::V4(v4) => Some(v4.ip().to_string()),
            _ => None,
        }))
        .unwrap_or_else(|| host.clone());

    Some(LanPeer {
        name: instance_name.to_string(),
        addr,
        port,
        hostname,
        platform,
        version,
        last_seen: chrono::Utc::now().timestamp(),
    })
}

use std::net::ToSocketAddrs;
