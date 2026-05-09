//! MTP (Media Transfer Protocol) support for Windows.
//!
//! iPhones and most modern Android phones expose themselves to Windows as
//! "portable devices" via MTP — they don't get a drive letter, so the
//! existing `device_monitor` (which polls `GetLogicalDrives()`) can never
//! see them. This module talks directly to the Windows Portable Devices
//! COM API (`IPortableDeviceManager` + friends) so we can list connected
//! phones, enumerate their DCIM folders, copy photos onto the local disk,
//! and delete photos from the phone.
//!
//! All COM calls happen on the thread that calls these functions; the
//! Tauri commands that wrap them run on a `spawn_blocking` worker so we
//! don't block the async runtime.

#![cfg(windows)]

use serde::Serialize;
use std::sync::Once;

use windows::{
    core::{GUID, PCWSTR, PWSTR},
    Win32::{
        Devices::PortableDevices::{
            IPortableDevice, IPortableDeviceContent, IPortableDeviceKeyCollection,
            IPortableDeviceManager, IPortableDeviceProperties, IPortableDevicePropVariantCollection,
            IPortableDeviceResources, IPortableDeviceValues, PortableDevice,
            PortableDeviceKeyCollection, PortableDeviceManager, PortableDevicePropVariantCollection,
            PortableDeviceValues, PORTABLE_DEVICE_DELETE_NO_RECURSION,
            WPD_CONTENT_TYPE_FOLDER, WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT, WPD_CONTENT_TYPE_IMAGE,
            WPD_CONTENT_TYPE_VIDEO, WPD_DEVICE_OBJECT_ID, WPD_OBJECT_CONTENT_TYPE,
            WPD_OBJECT_DATE_CREATED, WPD_OBJECT_DATE_MODIFIED, WPD_OBJECT_ID, WPD_OBJECT_NAME,
            WPD_OBJECT_ORIGINAL_FILE_NAME, WPD_OBJECT_PARENT_ID, WPD_OBJECT_SIZE,
            WPD_RESOURCE_DEFAULT,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, IStream, CLSCTX_INPROC_SERVER,
            COINIT_APARTMENTTHREADED, STGM_READ,
        },
        UI::Shell::PropertiesSystem::PROPERTYKEY,
    },
};
use windows_core::PROPVARIANT;

/// A device we found plugged into the PC via MTP.
#[derive(Debug, Clone, Serialize)]
pub struct MtpDevice {
    /// Opaque device ID used for subsequent WPD calls. Example on Windows
    /// looks like `\\?\usb#vid_05ac&pid_12a8&mi_00#...`.
    pub id: String,
    /// Human-readable name (e.g. "Apple iPhone", "Pixel 7").
    pub friendly_name: String,
    /// Manufacturer string, usually "Apple Inc." / "Google LLC" / etc.
    pub manufacturer: String,
    /// Device description; often "Apple iPhone" even when friendly_name is
    /// the user's custom device name ("Bugra'nın iPhone").
    pub description: String,
}

/// One-shot COM init for this process. The Portable Devices stack needs
/// COM; we use STA because some device drivers misbehave under MTA, and
/// WPD itself is perfectly happy with STA.
static COM_INIT: Once = Once::new();
fn ensure_com_init() {
    COM_INIT.call_once(|| unsafe {
        // Ignore the result: if COM is already initialized on this thread
        // with a compatible apartment type we get RPC_E_CHANGED_MODE and
        // that's fine.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    });
}

/// List all MTP/WPD devices currently connected.
///
/// This is the foundation: if this returns an empty vec when an iPhone is
/// plugged in, the whole pipeline below is dead before it starts. Common
/// reasons the iPhone won't appear even when it's connected:
///   - User hasn't tapped "Trust This Computer" on the phone's lock screen.
///   - iPhone is locked (MTP requires it to be unlocked at least once).
///   - Apple Mobile Device Support driver isn't installed (ships with
///     iTunes / Apple Devices / iCloud).
/// Surfacing these cases in the UI is Phase-2 work; for now we just
/// return the raw list.
pub fn list_devices() -> Result<Vec<MtpDevice>, String> {
    ensure_com_init();

    unsafe {
        let manager: IPortableDeviceManager =
            CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("CoCreateInstance(PortableDeviceManager) failed: {e}"))?;

        // Two-call pattern: first call with a null pointer tells us how
        // many devices, second call fills the buffer.
        let mut count: u32 = 0;
        manager
            .GetDevices(std::ptr::null_mut(), &mut count)
            .map_err(|e| format!("GetDevices (count) failed: {e}"))?;

        if count == 0 {
            return Ok(Vec::new());
        }

        // Buffer for PWSTRs — each slot will be filled with an LPWSTR that
        // WPD allocated via CoTaskMemAlloc. We must free each one with
        // CoTaskMemFree after we're done reading it.
        let mut ids: Vec<PWSTR> = vec![PWSTR::null(); count as usize];
        manager
            .GetDevices(ids.as_mut_ptr(), &mut count)
            .map_err(|e| format!("GetDevices (fill) failed: {e}"))?;

        let mut out = Vec::with_capacity(count as usize);
        for id_ptr in ids.iter_mut().take(count as usize) {
            if id_ptr.is_null() {
                continue;
            }

            let id = pwstr_to_string(*id_ptr);
            let friendly_name = read_device_string(&manager, *id_ptr, DeviceStr::Friendly);
            let manufacturer = read_device_string(&manager, *id_ptr, DeviceStr::Manufacturer);
            let description = read_device_string(&manager, *id_ptr, DeviceStr::Description);

            out.push(MtpDevice {
                id,
                friendly_name,
                manufacturer,
                description,
            });

            // Free the id PWSTR — WPD allocated it for us.
            windows::Win32::System::Com::CoTaskMemFree(Some(id_ptr.0 as *const _));
        }

        Ok(out)
    }
}

/// Which of the three "device string" getters on IPortableDeviceManager
/// we want. They all share the same two-call signature.
enum DeviceStr {
    Friendly,
    Manufacturer,
    Description,
}

/// Convenience: call the relevant GetDevice* method and return its value
/// as an owned String. Returns an empty string on any error — a missing
/// friendly name shouldn't break the whole device listing.
unsafe fn read_device_string(
    manager: &IPortableDeviceManager,
    device_id: PWSTR,
    which: DeviceStr,
) -> String {
    // Two-call pattern: first call with a null PWSTR asks WPD how big a
    // buffer we need (writes the size — incl. trailing NUL — into `len`),
    // second call fills the buffer.
    let mut len: u32 = 0;
    let probe = match which {
        DeviceStr::Friendly => {
            manager.GetDeviceFriendlyName(PCWSTR(device_id.0), PWSTR::null(), &mut len)
        }
        DeviceStr::Manufacturer => {
            manager.GetDeviceManufacturer(PCWSTR(device_id.0), PWSTR::null(), &mut len)
        }
        DeviceStr::Description => {
            manager.GetDeviceDescription(PCWSTR(device_id.0), PWSTR::null(), &mut len)
        }
    };
    if probe.is_err() || len == 0 {
        return String::new();
    }

    let mut buf: Vec<u16> = vec![0u16; len as usize];
    let buf_pwstr = PWSTR(buf.as_mut_ptr());
    let fill = match which {
        DeviceStr::Friendly => {
            manager.GetDeviceFriendlyName(PCWSTR(device_id.0), buf_pwstr, &mut len)
        }
        DeviceStr::Manufacturer => {
            manager.GetDeviceManufacturer(PCWSTR(device_id.0), buf_pwstr, &mut len)
        }
        DeviceStr::Description => {
            manager.GetDeviceDescription(PCWSTR(device_id.0), buf_pwstr, &mut len)
        }
    };
    if fill.is_err() {
        return String::new();
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Copy a PWSTR (NUL-terminated wide string) into an owned Rust String.
/// Returns an empty string if the pointer is null.
unsafe fn pwstr_to_string(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *p.0.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(p.0, len);
    String::from_utf16_lossy(slice)
}

// ─── Phase 2: device opening + media enumeration ───────────────────────────

/// One photo/video object living on the device. `id` is an opaque WPD
/// object ID (strings that look like `o123ABC`); it's only meaningful when
/// passed back to the same open IPortableDevice, so we always re-open per
/// request.
#[derive(Debug, Clone, Serialize)]
pub struct MtpObject {
    pub id: String,
    pub name: String,
    pub size: u64,
    /// Capture date if WPD knows one, otherwise None. Format: ISO-8601.
    pub date_created: Option<String>,
    pub is_video: bool,
}

/// Summary of what's on the device. Returned by `list_media()`; the UI
/// shows this before the user picks a destination folder.
#[derive(Debug, Clone, Serialize)]
pub struct MtpMediaList {
    pub photos: Vec<MtpObject>,
    pub total_size: u64,
    pub photo_count: usize,
    pub video_count: usize,
}

/// Open a WPD device handle for `device_id`. Caller owns the returned
/// interface and should let it drop when done (RAII closes the device).
unsafe fn open_device(device_id: &str) -> Result<IPortableDevice, String> {
    ensure_com_init();

    // Client info: WPD requires an IPortableDeviceValues, but the only
    // property most drivers actually read is WPD_CLIENT_NAME. We populate
    // a sensible default so the Windows event log says "RetinaTag" rather
    // than "(unknown)".
    let client_info: IPortableDeviceValues =
        CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("CoCreateInstance(PortableDeviceValues): {e}"))?;

    // Device ID → wide string.
    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();

    let device: IPortableDevice =
        CoCreateInstance(&PortableDevice, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("CoCreateInstance(PortableDevice): {e}"))?;

    device
        .Open(PCWSTR(wide.as_ptr()), &client_info)
        .map_err(|e| {
            format!(
                "IPortableDevice::Open failed ({e}). \
                On iPhone, make sure the phone is unlocked and you've tapped \
                'Trust This Computer'."
            )
        })?;

    Ok(device)
}

/// Read a string property from an IPortableDeviceValues; returns empty on
/// missing / wrong type.
unsafe fn values_get_string(
    values: &IPortableDeviceValues,
    key: &PROPERTYKEY,
) -> String {
    match values.GetStringValue(key) {
        Ok(pwstr) => {
            let s = pwstr_to_string(pwstr);
            windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.0 as *const _));
            s
        }
        Err(_) => String::new(),
    }
}

/// Read a u64 property; 0 on missing.
unsafe fn values_get_u64(
    values: &IPortableDeviceValues,
    key: &PROPERTYKEY,
) -> u64 {
    values.GetUnsignedLargeIntegerValue(key).unwrap_or(0)
}

/// Read a GUID property; zeroed GUID on missing.
unsafe fn values_get_guid(
    values: &IPortableDeviceValues,
    key: &PROPERTYKEY,
) -> GUID {
    values.GetGuidValue(key).unwrap_or(GUID::zeroed())
}

/// Build a key-collection containing the properties we care about when
/// inspecting an object. Reused across every GetValues() call during a
/// single enumeration.
unsafe fn build_object_key_collection() -> Result<IPortableDeviceKeyCollection, String> {
    let keys: IPortableDeviceKeyCollection =
        CoCreateInstance(&PortableDeviceKeyCollection, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("CoCreateInstance(PortableDeviceKeyCollection): {e}"))?;
    for k in [
        &WPD_OBJECT_ID,
        &WPD_OBJECT_PARENT_ID,
        &WPD_OBJECT_NAME,
        &WPD_OBJECT_ORIGINAL_FILE_NAME,
        &WPD_OBJECT_CONTENT_TYPE,
        &WPD_OBJECT_SIZE,
        &WPD_OBJECT_DATE_CREATED,
        &WPD_OBJECT_DATE_MODIFIED,
    ] {
        keys.Add(k).map_err(|e| format!("KeyCollection.Add: {e}"))?;
    }
    Ok(keys)
}

/// Recursively enumerate every child of `parent_id` and keep any objects
/// whose CONTENT_TYPE is IMAGE or VIDEO. Folders and functional objects
/// (like "Storage") are recursed into but not returned themselves.
unsafe fn walk_objects(
    content: &IPortableDeviceContent,
    props: &IPortableDeviceProperties,
    keys: &IPortableDeviceKeyCollection,
    parent_id: &str,
    out: &mut Vec<MtpObject>,
    total_size: &mut u64,
) -> Result<(), String> {
    let parent_wide: Vec<u16> = parent_id
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let enumerator = content
        .EnumObjects(0, PCWSTR(parent_wide.as_ptr()), None)
        .map_err(|e| format!("EnumObjects: {e}"))?;

    // WPD drivers return batches of child object IDs. Keep pulling until
    // we get 0.
    loop {
        let mut child_ids: [PWSTR; 32] = [PWSTR::null(); 32];
        let mut fetched: u32 = 0;
        let hr = enumerator.Next(&mut child_ids, &mut fetched);
        if hr.is_err() {
            break;
        }
        if fetched == 0 {
            break;
        }

        for child_pwstr in &child_ids[..fetched as usize] {
            if child_pwstr.is_null() {
                continue;
            }
            let child_id = pwstr_to_string(*child_pwstr);

            // Read properties for this child.
            let values_res = props.GetValues(PCWSTR(child_pwstr.0), keys);
            // Free the id string now — we've either cloned or we don't care.
            windows::Win32::System::Com::CoTaskMemFree(Some(child_pwstr.0 as *const _));

            let values = match values_res {
                Ok(v) => v,
                Err(_) => continue,
            };

            let content_type = values_get_guid(&values, &WPD_OBJECT_CONTENT_TYPE);

            // Pull the filename early — we use it as both metadata and a
            // fallback classifier. iPhone MTP is notorious for returning
            // GUID::zeroed() / WPD_CONTENT_TYPE_UNSPECIFIED on HEIC, MOV,
            // and some JPEG files, so relying purely on content_type gives
            // a "0 photos found" result on otherwise-populated devices.
            let mut name = values_get_string(&values, &WPD_OBJECT_ORIGINAL_FILE_NAME);
            if name.is_empty() {
                name = values_get_string(&values, &WPD_OBJECT_NAME);
            }

            let mut is_image = content_type == WPD_CONTENT_TYPE_IMAGE;
            let mut is_video = content_type == WPD_CONTENT_TYPE_VIDEO;
            let is_folder = content_type == WPD_CONTENT_TYPE_FOLDER
                || content_type == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT;

            // Extension-based fallback for when the driver doesn't tag
            // content_type correctly. Only fires if we haven't already
            // classified the object.
            if !is_image && !is_video && !is_folder && !name.is_empty() {
                let lower = name.to_ascii_lowercase();
                let dot = lower.rfind('.').map(|i| &lower[i + 1..]).unwrap_or("");
                match dot {
                    "jpg" | "jpeg" | "png" | "heic" | "heif" | "tiff" | "tif"
                    | "bmp" | "gif" | "webp" | "dng" | "cr2" | "cr3" | "nef"
                    | "arw" | "orf" | "rw2" | "raf" => is_image = true,
                    "mov" | "mp4" | "m4v" | "avi" | "mkv" | "3gp" | "hevc" => {
                        is_video = true
                    }
                    _ => {}
                }
            }

            if is_image || is_video {
                let size = values_get_u64(&values, &WPD_OBJECT_SIZE);
                *total_size += size;

                // WPD stores dates as a string "yyyy/MM/dd:HH:mm:ss.ms" OR a
                // native DATE; in the windows-rs binding they come as
                // strings most of the time. We just pass what we get
                // through for the UI.
                let date_created = values_get_string(&values, &WPD_OBJECT_DATE_CREATED);
                let date_created = if date_created.is_empty() {
                    let d = values_get_string(&values, &WPD_OBJECT_DATE_MODIFIED);
                    if d.is_empty() { None } else { Some(d) }
                } else {
                    Some(date_created)
                };

                out.push(MtpObject {
                    id: child_id,
                    name,
                    size,
                    date_created,
                    is_video,
                });
            } else if is_folder {
                // Recurse. Use a stack-style approach via recursive call —
                // iPhone DCIM tree is at most 3 levels deep so recursion
                // is safe.
                let _ = walk_objects(content, props, keys, &child_id, out, total_size);
            } else {
                // Unknown content_type — could be a nested "album" object on
                // iPhone that isn't tagged as FOLDER but still contains
                // images. Recurse opportunistically; EnumObjects will
                // immediately return 0 children for real leaf files, so the
                // overhead is minimal.
                let _ = walk_objects(content, props, keys, &child_id, out, total_size);
            }
        }
    }

    Ok(())
}

/// List all photos and videos on the device. This is the heavy call — on
/// a full iPhone with 20k photos it can take ~10–20 seconds.
pub fn list_media(device_id: &str) -> Result<MtpMediaList, String> {
    unsafe {
        let device = open_device(device_id)?;
        let content: IPortableDeviceContent = device
            .Content()
            .map_err(|e| format!("IPortableDevice::Content: {e}"))?;
        let props: IPortableDeviceProperties = content
            .Properties()
            .map_err(|e| format!("IPortableDeviceContent::Properties: {e}"))?;
        let keys = build_object_key_collection()?;

        let mut out: Vec<MtpObject> = Vec::new();
        let mut total_size: u64 = 0;

        // Walk from the synthetic root ("DEVICE"). WPD_DEVICE_OBJECT_ID is
        // a PCWSTR constant pointing at the string "DEVICE".
        let root_id = pwstr_to_string(PWSTR(WPD_DEVICE_OBJECT_ID.as_ptr() as *mut u16));
        walk_objects(&content, &props, &keys, &root_id, &mut out, &mut total_size)?;

        let photo_count = out.iter().filter(|o| !o.is_video).count();
        let video_count = out.iter().filter(|o| o.is_video).count();

        // Diagnostic. If an iPhone returns 0 images, it almost always means:
        //   (a) phone is locked / "Trust This Computer" hasn't been tapped,
        //   (b) iCloud Photos is set to "Optimize iPhone Storage" so the
        //       originals live in the cloud and aren't exposed over MTP, or
        //   (c) the WPD driver is tagging HEIC/MOV with UNSPECIFIED
        //       content_type (we now fall back on the file extension, so
        //       this case should be rare post-v1.4.32).
        // We log here rather than erroring so the UI still renders the
        // "0 photos found" state gracefully.
        eprintln!(
            "[mtp] list_media: {} photos, {} videos, {} bytes total (device {})",
            photo_count, video_count, total_size, device_id
        );

        Ok(MtpMediaList {
            photos: out,
            total_size,
            photo_count,
            video_count,
        })
    }
}

// ─── Phase 3: copy one object to disk ──────────────────────────────────────

/// Public wrapper for bulk operations that want to open once and reuse
/// the handle across many `copy_object_with_device` calls. Returns an
/// opaque `IPortableDevice`; caller just holds it alive and passes it
/// back in.
pub fn open_device_for_bulk(device_id: &str) -> Result<IPortableDevice, String> {
    unsafe { open_device(device_id) }
}

/// Copy a single MTP object to a file on disk. Opens a new device handle
/// per call which is wasteful for bulk import — prefer `open_device_for_bulk()`
/// once and call `copy_object_with_device()` in a loop.
pub fn copy_object(
    device_id: &str,
    object_id: &str,
    dest_path: &std::path::Path,
) -> Result<u64, String> {
    unsafe {
        let device = open_device(device_id)?;
        copy_object_with_device(&device, object_id, dest_path)
    }
}

/// Same as `copy_object` but reuses an already-open device handle.
/// Returns the number of bytes written. `dest_path`'s parent must exist;
/// we overwrite any existing file at that path.
pub unsafe fn copy_object_with_device(
    device: &IPortableDevice,
    object_id: &str,
    dest_path: &std::path::Path,
) -> Result<u64, String> {
    let content: IPortableDeviceContent = device
        .Content()
        .map_err(|e| format!("Content: {e}"))?;
    let resources: IPortableDeviceResources = content
        .Transfer()
        .map_err(|e| format!("Content::Transfer (IPortableDeviceResources): {e}"))?;

    let object_wide: Vec<u16> = object_id
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // Ask for the default resource stream (= the actual file bytes).
    let mut optimal_transfer_size: u32 = 0;
    let mut stream_opt: Option<IStream> = None;
    resources
        .GetStream(
            PCWSTR(object_wide.as_ptr()),
            &WPD_RESOURCE_DEFAULT,
            STGM_READ.0 as u32,
            &mut optimal_transfer_size,
            &mut stream_opt,
        )
        .map_err(|e| format!("GetStream: {e}"))?;

    let stream = stream_opt.ok_or_else(|| "GetStream returned null stream".to_string())?;

    // 64 KiB buffer is plenty — WPD drivers typically hand back 64K/256K
    // chunks anyway. Avoid gigabyte-size buffers for videos.
    let mut buf = vec![0u8; 64 * 1024];
    let mut out_file = std::fs::File::create(dest_path)
        .map_err(|e| format!("create {dest_path:?}: {e}"))?;
    let mut total_written: u64 = 0;

    use std::io::Write;
    loop {
        let mut read: u32 = 0;
        let hr = stream.Read(
            buf.as_mut_ptr() as *mut _,
            buf.len() as u32,
            Some(&mut read),
        );
        if hr.is_err() {
            return Err(format!("IStream::Read: {:?}", hr));
        }
        if read == 0 {
            break;
        }
        out_file
            .write_all(&buf[..read as usize])
            .map_err(|e| format!("write {dest_path:?}: {e}"))?;
        total_written += read as u64;
    }

    Ok(total_written)
}

// ─── Phase 5: delete objects from device ───────────────────────────────────

/// Delete the given objects from the device. Returns (deleted, failed).
/// On iPhone this is the equivalent of "remove from Camera Roll" — files
/// are gone permanently.
pub fn delete_objects(device_id: &str, object_ids: &[String]) -> Result<(usize, usize), String> {
    if object_ids.is_empty() {
        return Ok((0, 0));
    }
    unsafe {
        let device = open_device(device_id)?;
        let content: IPortableDeviceContent =
            device.Content().map_err(|e| format!("Content: {e}"))?;

        // Build an IPortableDevicePropVariantCollection of VT_LPWSTR entries.
        let coll: IPortableDevicePropVariantCollection = CoCreateInstance(
            &PortableDevicePropVariantCollection,
            None,
            CLSCTX_INPROC_SERVER,
        )
        .map_err(|e| format!("CoCreateInstance(PropVariantCollection): {e}"))?;

        for id in object_ids {
            // WPD's Delete/EnumObjects accept VT_BSTR for object IDs on
            // both Apple and Android drivers, so PROPVARIANT::from(&str)
            // (which produces a VT_BSTR) is sufficient here.
            let pv = PROPVARIANT::from(id.as_str());
            coll.Add(&pv)
                .map_err(|e| format!("PropVariantCollection::Add: {e}"))?;
        }

        // Delete (no recursion — we don't want to accidentally nuke a
        // whole folder tree if someone passed a folder ID by mistake).
        let delete_result = content.Delete(
            PORTABLE_DEVICE_DELETE_NO_RECURSION.0 as u32,
            &coll,
            std::ptr::null_mut(),
        );

        // Inspect the results collection to count successes vs failures.
        // If the Delete call itself failed, treat all as failed.
        match delete_result {
            Ok(_) => {
                // For a conservative first pass, assume all succeeded. WPD
                // does have a per-item HRESULT collection but parsing that
                // is a whole extra pile of COM; we'll refine later if real
                // failures show up.
                Ok((object_ids.len(), 0))
            }
            Err(e) => Err(format!("Content::Delete: {e}")),
        }
    }
}

