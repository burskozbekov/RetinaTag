use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tauri::Emitter;

use crate::scanner;

/// Monitors for new removable drives (USB sticks, SD cards, some cameras) and
/// *asks the user* where to import from DCIM/. Unlike the old behaviour, this
/// version never copies files automatically — it only emits a `device-detected`
/// event and lets the frontend decide.
///
/// Import itself is done through the `import_from_device` Tauri command, which
/// lets the user pick a destination folder and organises copies by EXIF date
/// (YYYY / YYYY-MM - MonthName).
///
/// iPhone note: on Windows, iPhones connect as MTP portable devices and do
/// **not** show up as drive letters, so `GetLogicalDrives` won't see them.
/// This monitor covers USB sticks, SD cards, and cameras that mount as mass
/// storage. MTP support would need a separate code path using
/// IPortableDeviceManager.
pub struct DeviceMonitor {
    stop_flag: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DeviceMonitor {
    pub fn start(app_handle: tauri::AppHandle) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();

        let handle = std::thread::spawn(move || {
            // NB: `known_drives` starts EMPTY, not as a snapshot of what is
            // currently plugged in. That way the first pass also emits
            // `device-detected` for devices that were already connected when
            // the app launched — otherwise a USB stick / SD card plugged in
            // before RetinaTag started would silently never trigger the
            // import modal. Give the frontend ~500ms to wire up its listener
            // before the first pass so the initial events aren't dropped.
            let mut known_drives: HashSet<String> = HashSet::new();
            // Same pattern for MTP: start empty so a phone already plugged
            // in when the app boots still triggers the modal. Keys are
            // opaque WPD device IDs which are stable per physical device.
            let mut known_mtp: HashSet<String> = HashSet::new();
            std::thread::sleep(Duration::from_millis(500));
            eprintln!("DeviceMonitor started.");

            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }

                // Pass 1: removable drives (USB sticks, SD cards, cameras).
                let current = get_removable_drives();
                let new_drives: Vec<String> = current.difference(&known_drives).cloned().collect();
                for drive in &new_drives {
                    eprintln!("Removable drive detected: {}", drive);
                    emit_device_detected(&app_handle, drive);
                }
                known_drives = current;

                // Pass 2: MTP devices (iPhone / Android). Windows-only because
                // the `mtp` module wraps IPortableDeviceManager which doesn't
                // exist elsewhere. Emits `mtp-device-connected` with the full
                // MtpDevice payload — the frontend decides whether to auto-
                // open the MTP import modal.
                #[cfg(target_os = "windows")]
                {
                    let current_mtp = get_mtp_devices();
                    let current_ids: HashSet<String> =
                        current_mtp.iter().map(|d| d.id.clone()).collect();
                    for dev in &current_mtp {
                        if !known_mtp.contains(&dev.id) {
                            eprintln!(
                                "MTP device detected: {} ({})",
                                dev.friendly_name, dev.manufacturer
                            );
                            app_handle
                                .emit(
                                    "mtp-device-connected",
                                    serde_json::json!({
                                        "id": dev.id,
                                        "friendly_name": dev.friendly_name,
                                        "manufacturer": dev.manufacturer,
                                        "description": dev.description,
                                    }),
                                )
                                .ok();
                        }
                    }
                    known_mtp = current_ids;
                }
                // Keep the variable reachable on non-Windows targets so the
                // compiler doesn't emit an unused-variable warning. `known_mtp`
                // is populated on Windows; elsewhere it stays empty.
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = &known_mtp;
                }

                // Poll every 3s but wake up faster if asked to stop.
                for _ in 0..30 {
                    if stop_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }

            eprintln!("DeviceMonitor stopped.");
        });

        DeviceMonitor {
            stop_flag,
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
    }
}

impl Drop for DeviceMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Emit a `device-detected` event for `drive`, populated with media_root /
/// has_media / file_count. Used both by the background poller and by the
/// manual `rescan_devices` command.
fn emit_device_detected(app_handle: &tauri::AppHandle, drive: &str) {
    let media_root = find_media_root(drive);
    let (has_media, file_count) = if let Some(ref root) = media_root {
        let count = count_media_files(root);
        (count > 0, count)
    } else {
        (false, 0)
    };
    app_handle
        .emit(
            "device-detected",
            serde_json::json!({
                "drive": drive,
                "media_root": media_root.as_ref().map(|p| p.to_string_lossy().to_string()),
                "has_media": has_media,
                "file_count": file_count,
            }),
        )
        .ok();
}

/// Scan all currently-connected removable drives and emit `device-detected`
/// for each one. Returns the number of drives found so the caller can decide
/// whether to show a "no devices" message (which is where we explain the
/// Windows/MTP limitation — iPhones don't show up as drive letters, so even
/// a full rescan can't see them and the user has to import manually).
pub fn rescan_now(app_handle: &tauri::AppHandle) -> usize {
    let drives = get_removable_drives();
    for drive in &drives {
        emit_device_detected(app_handle, drive);
    }
    drives.len()
}

/// Common media folder names cameras and phones use.
const MEDIA_ROOTS: &[&str] = &["DCIM", "Pictures", "Photos", "PRIVATE/AVCHD"];

/// Find the first media-like folder on the drive. Falls back to None if
/// nothing obvious is found (user can still pick the drive root manually
/// via the UI if they want).
fn find_media_root(drive: &str) -> Option<PathBuf> {
    for candidate in MEDIA_ROOTS {
        let p = Path::new(drive).join(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Count image/video files under a root (recursive, capped).
fn count_media_files(root: &Path) -> usize {
    walkdir::WalkDir::new(root)
        .max_depth(8)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && scanner::is_image_file(e.path()))
        .take(100_000)
        .count()
}

/// Get the set of removable drive root paths (e.g. "E:\", "F:\")
#[cfg(target_os = "windows")]
fn get_removable_drives() -> HashSet<String> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetDriveTypeW(root: *const u16) -> u32;
    }

    const DRIVE_REMOVABLE: u32 = 2;
    let mut drives = HashSet::new();

    unsafe {
        let bitmask = GetLogicalDrives();
        for i in 0..26u32 {
            if bitmask & (1 << i) != 0 {
                let letter = (b'A' + i as u8) as char;
                let root_str = format!("{}:\\", letter);
                let root: Vec<u16> = OsStr::new(&root_str)
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect();
                let drive_type = GetDriveTypeW(root.as_ptr());
                if drive_type == DRIVE_REMOVABLE {
                    drives.insert(root_str);
                }
            }
        }
    }

    drives
}

#[cfg(not(target_os = "windows"))]
fn get_removable_drives() -> HashSet<String> {
    HashSet::new()
}

/// Return the currently-connected MTP devices that look like phones or
/// cameras — matches the same filter the import UI uses so we don't spam the
/// user with "Android gamepad connected" modals. Errors are logged but
/// swallowed so a transient COM failure doesn't kill the poll loop.
#[cfg(target_os = "windows")]
fn get_mtp_devices() -> Vec<crate::mtp::MtpDevice> {
    match crate::mtp::list_devices() {
        Ok(devs) => devs
            .into_iter()
            .filter(|d| {
                let hay = format!(
                    "{} {} {}",
                    d.friendly_name.to_lowercase(),
                    d.manufacturer.to_lowercase(),
                    d.description.to_lowercase()
                );
                // Same regex the UI uses in `dist/index.html`.
                hay.contains("iphone")
                    || hay.contains("ipad")
                    || hay.contains("ipod")
                    || hay.contains("apple")
                    || hay.contains("android")
                    || hay.contains("galaxy")
                    || hay.contains("samsung")
                    || hay.contains("pixel")
                    || hay.contains("xiaomi")
                    || hay.contains("huawei")
                    || hay.contains("oneplus")
                    || hay.contains("phone")
                    || hay.contains("camera")
            })
            .collect(),
        Err(e) => {
            eprintln!("mtp::list_devices error: {}", e);
            Vec::new()
        }
    }
}
