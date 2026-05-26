// v1.5.283 — libmpv embed for accurate HEVC HDR video playback.
//
// WebView2's HEVC pipeline (Media Foundation decoder + Windows compositor
// tone-mapping) produces noticeably different colours from VLC / mpv /
// Photos.app on Mac.  The user records on iPhone (HEVC + Dolby Vision)
// and expects the colours they see on macOS — which uses AVFoundation —
// to look the same on Windows.  Chromium's video path can't get there.
//
// Solution: embed libmpv, which is the same decoder + tone-mapper VLC
// uses underneath (libavcodec + libplacebo).  We render into a native
// child HWND parented to the Tauri main window, positioned over a
// transparent placeholder <div> in the lightbox.  WebView2 sits on
// top, but the placeholder div is transparent, so visually the user
// sees the mpv-rendered video instead of WebView2's interpretation.
//
// libmpv-2.dll is shipped under bin/libmpv-2.dll alongside the app
// executable (via Tauri's `resources` config).  We load it at runtime
// via `libloading` — no import library hassle, and the app still
// starts if the DLL is missing or fails (we'd fall back to the
// existing <video> path).

// ── Tauri commands ──────────────────────────────────────────────────────────
//
// The commands themselves live in this top-level module so the
// `#[tauri::command]` proc-macro's companion symbols (`__cmd__mpv_probe`
// etc.) resolve where `tauri::generate_handler!` expects them.  Bodies
// forward to the Windows impl or return an error on other platforms.

#[tauri::command]
pub fn mpv_probe() -> Result<String, String> {
    #[cfg(target_os = "windows")] { imp::mpv_probe() }
    #[cfg(not(target_os = "windows"))] { Err("libmpv is Windows-only in this build".into()) }
}

#[tauri::command]
pub fn mpv_test_open(path: String) -> Result<String, String> {
    #[cfg(target_os = "windows")] { imp::mpv_test_open(path) }
    #[cfg(not(target_os = "windows"))] {
        let _ = path;
        Err("libmpv is Windows-only in this build".into())
    }
}

#[tauri::command]
pub fn mpv_close() -> Result<(), String> {
    #[cfg(target_os = "windows")] { imp::mpv_close() }
    #[cfg(not(target_os = "windows"))] { Ok(()) }
}

/// v1.5.285 — Open `path` inside an mpv-rendered child window parented
/// to the Tauri main window, at the given rect (CSS pixels, top-left
/// origin within the main window's client area).
#[tauri::command]
pub fn mpv_show_in_window(
    app: tauri::AppHandle,
    path: String,
    x: i32, y: i32, w: i32, h: i32,
    muted: Option<bool>,
) -> Result<(), String> {
    #[cfg(target_os = "windows")] { imp::mpv_show_in_window(app, path, x, y, w, h, muted.unwrap_or(false)) }
    #[cfg(not(target_os = "windows"))] {
        let _ = (app, path, x, y, w, h, muted);
        Err("libmpv is Windows-only in this build".into())
    }
}

/// v1.5.285 — Reposition the active overlay window (called from JS on
/// window resize/move so the video follows the placeholder div).
#[tauri::command]
pub fn mpv_set_rect(
    app: tauri::AppHandle,
    x: i32, y: i32, w: i32, h: i32,
) -> Result<(), String> {
    #[cfg(target_os = "windows")] { imp::mpv_set_rect(app, x, y, w, h) }
    #[cfg(not(target_os = "windows"))] {
        let _ = (app, x, y, w, h);
        Ok(())
    }
}

/// v1.5.285 — Hide the overlay (lightbox closed).  Keeps the player
/// alive so the next open is fast; pair with `mpv_close` to fully
/// tear it down.
#[tauri::command]
pub fn mpv_hide_overlay() -> Result<(), String> {
    #[cfg(target_os = "windows")] { imp::mpv_hide_overlay() }
    #[cfg(not(target_os = "windows"))] { Ok(()) }
}

/// v1.5.285 — Toggle pause on the active overlay player.
#[tauri::command]
pub fn mpv_set_paused(paused: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")] { imp::mpv_set_paused(paused) }
    #[cfg(not(target_os = "windows"))] {
        let _ = paused;
        Ok(())
    }
}

/// Non-command helper: lib.rs's window-event handler calls this when
/// the main window moves or resizes so the mpv overlay stays glued to
/// the placeholder div.  No-op on non-Windows.
pub fn on_main_window_geometry_changed(app: &tauri::AppHandle) {
    #[cfg(target_os = "windows")] { imp::reposition_after_main_window_event(app); }
    #[cfg(not(target_os = "windows"))] {
        let _ = app;
    }
}

/// Non-command helper: hide the overlay when the main window is
/// minimised / loses focus, show it again when it returns.  Without
/// this the popup would float over other apps when the user Alt+Tabs.
pub fn set_overlay_visible(visible: bool) {
    #[cfg(target_os = "windows")] { imp::set_overlay_visible(visible); }
    #[cfg(not(target_os = "windows"))] {
        let _ = visible;
    }
}

#[cfg(target_os = "windows")]
mod imp {

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use libloading::{Library, Symbol};

// ── libmpv C API subset we actually use ────────────────────────────────────
//
// Signatures lifted from libmpv-2.dll's `client.h` (zhongfly's build).  We
// only declare the dozen calls the player needs.
//
// The crate-internal `MpvHandle` type-erases the opaque `mpv_handle *` so
// nothing outside this module touches the raw pointer.

type MpvHandlePtr = *mut c_void;

// Format codes from libmpv/client.h
const MPV_FORMAT_NONE:   c_int = 0;
const MPV_FORMAT_STRING: c_int = 1;
const MPV_FORMAT_FLAG:   c_int = 3;
const MPV_FORMAT_INT64:  c_int = 4;
const MPV_FORMAT_DOUBLE: c_int = 5;

// Function pointer signatures (cdecl on Windows for libmpv)
type FnCreate         = unsafe extern "C" fn() -> MpvHandlePtr;
type FnInitialize     = unsafe extern "C" fn(MpvHandlePtr) -> c_int;
type FnTerminate      = unsafe extern "C" fn(MpvHandlePtr);
type FnSetOption      = unsafe extern "C" fn(MpvHandlePtr, *const c_char, c_int, *mut c_void) -> c_int;
type FnSetOptionStr   = unsafe extern "C" fn(MpvHandlePtr, *const c_char, *const c_char) -> c_int;
type FnSetProperty    = unsafe extern "C" fn(MpvHandlePtr, *const c_char, c_int, *mut c_void) -> c_int;
type FnSetPropertyStr = unsafe extern "C" fn(MpvHandlePtr, *const c_char, *const c_char) -> c_int;
type FnGetPropertyStr = unsafe extern "C" fn(MpvHandlePtr, *const c_char) -> *mut c_char;
type FnFree           = unsafe extern "C" fn(*mut c_void);
type FnCommand        = unsafe extern "C" fn(MpvHandlePtr, *const *const c_char) -> c_int;
type FnErrorString    = unsafe extern "C" fn(c_int) -> *const c_char;

/// All the symbols we resolve once when the DLL is first loaded.
struct MpvApi {
    _lib: Library, // keep DLL alive
    create:           unsafe extern "C" fn() -> MpvHandlePtr,
    initialize:       unsafe extern "C" fn(MpvHandlePtr) -> c_int,
    terminate:        unsafe extern "C" fn(MpvHandlePtr),
    set_option:       unsafe extern "C" fn(MpvHandlePtr, *const c_char, c_int, *mut c_void) -> c_int,
    set_option_str:   unsafe extern "C" fn(MpvHandlePtr, *const c_char, *const c_char) -> c_int,
    set_property:     unsafe extern "C" fn(MpvHandlePtr, *const c_char, c_int, *mut c_void) -> c_int,
    set_property_str: unsafe extern "C" fn(MpvHandlePtr, *const c_char, *const c_char) -> c_int,
    get_property_str: unsafe extern "C" fn(MpvHandlePtr, *const c_char) -> *mut c_char,
    free:             unsafe extern "C" fn(*mut c_void),
    command:          unsafe extern "C" fn(MpvHandlePtr, *const *const c_char) -> c_int,
    error_string:     unsafe extern "C" fn(c_int) -> *const c_char,
}

impl MpvApi {
    /// Resolve libmpv-2.dll either next to the .exe (production install)
    /// or under third-party/mpv/ (cargo run from source).
    fn load() -> Result<Self, String> {
        let candidates = candidate_paths();
        let mut last_err = String::new();
        for path in &candidates {
            match unsafe { Library::new(path) } {
                Ok(lib) => {
                    eprintln!("[mpv] loaded {}", path.display());
                    return resolve_symbols(lib);
                }
                Err(e) => {
                    last_err = format!("{}: {}", path.display(), e);
                }
            }
        }
        Err(format!(
            "libmpv-2.dll not found.  Tried:\n  {}\nLast error: {}",
            candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n  "),
            last_err
        ))
    }
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    // 1. Tauri ships the DLL via `resources` in tauri.conf.json, which
    //    preserves the relative path.  After install the DLL lives at
    //    <install-dir>\third-party\mpv\libmpv-2.dll.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.join("third-party").join("mpv").join("libmpv-2.dll"));
            v.push(dir.join("libmpv-2.dll"));
            v.push(dir.join("mpv-2.dll"));
        }
    }
    // 2. Source tree (cargo run --release from src-tauri)
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    v.push(manifest.join("third-party").join("mpv").join("libmpv-2.dll"));
    // 3. Workspace target/release/ alongside retina-tag.exe in dev
    v.push(manifest.join("target").join("release").join("libmpv-2.dll"));
    v.push(manifest.join("target").join("release").join("third-party").join("mpv").join("libmpv-2.dll"));
    v
}

fn resolve_symbols(lib: Library) -> Result<MpvApi, String> {
    unsafe {
        macro_rules! sym {
            ($name:literal) => {{
                let s: Symbol<*const c_void> = lib
                    .get($name)
                    .map_err(|e| format!("symbol {} not found: {}", stringify!($name), e))?;
                *(&*s as *const _ as *const _)
            }};
        }
        Ok(MpvApi {
            create:           sym!(b"mpv_create\0"),
            initialize:       sym!(b"mpv_initialize\0"),
            terminate:        sym!(b"mpv_terminate_destroy\0"),
            set_option:       sym!(b"mpv_set_option\0"),
            set_option_str:   sym!(b"mpv_set_option_string\0"),
            set_property:     sym!(b"mpv_set_property\0"),
            set_property_str: sym!(b"mpv_set_property_string\0"),
            get_property_str: sym!(b"mpv_get_property_string\0"),
            free:             sym!(b"mpv_free\0"),
            command:          sym!(b"mpv_command\0"),
            error_string:     sym!(b"mpv_error_string\0"),
            _lib:             lib,
        })
    }
}

static MPV_API: OnceLock<Result<MpvApi, String>> = OnceLock::new();

fn api() -> Result<&'static MpvApi, String> {
    MPV_API.get_or_init(MpvApi::load).as_ref().map_err(|e| e.clone())
}

// ── High-level player wrapper ───────────────────────────────────────────────

pub struct MpvPlayer {
    handle: MpvHandlePtr,
}

unsafe impl Send for MpvPlayer {}
unsafe impl Sync for MpvPlayer {}

impl MpvPlayer {
    /// Create + initialise an mpv context.  Sets the colour-accurate
    /// config (gpu-next, libplacebo tone-map, etc.).
    ///
    /// If `parent_hwnd` is `Some`, mpv is told to render into that
    /// window via the `wid` option.  This option MUST be applied
    /// **before** `mpv_initialize`, otherwise mpv creates its own
    /// top-level video output window and the embed silently fails
    /// (audio still plays — the user-visible "no video" symptom).
    /// `None` is for the standalone test where mpv may create its own
    /// window via `force-window=yes`.
    pub fn new(parent_hwnd: Option<isize>) -> Result<Self, String> {
        let a = api()?;
        let handle = unsafe { (a.create)() };
        if handle.is_null() {
            return Err("mpv_create returned NULL".into());
        }

        let me = MpvPlayer { handle };

        // Quiet on stderr unless something blows up.
        me.set_option_str("msg-level", "all=warn")?;
        me.set_option_str("terminal", "no")?;

        // v1.5.286 — wid MUST be set before mpv_initialize, or mpv
        // creates its own window and renders there instead of the
        // parent.  See mpv issue #10189 for the u32 cast workaround.
        if let Some(hwnd) = parent_hwnd {
            let wid_u32: u32 = hwnd as u32;
            let mut wid_i64: i64 = wid_u32 as i64;
            let name = CString::new("wid").unwrap();
            let r = unsafe {
                (a.set_option)(
                    me.handle,
                    name.as_ptr(),
                    MPV_FORMAT_INT64,
                    &mut wid_i64 as *mut _ as *mut c_void,
                )
            };
            if r < 0 {
                return Err(format!("set_option wid: {}", api_err(a, r)));
            }
        }

        // v1.5.283 — Colour-accurate config tuned for iPhone HEVC + HDR.
        //   • vo=gpu-next     — libplacebo renderer (Vulkan/D3D11)
        //   • hwdec=auto-copy — let the GPU decode but keep frames in
        //     system memory so our colour pipeline runs against them,
        //     not against a vendor-specific D3D11VA surface
        //   • target-colorspace-hint=yes — tells the OS what we're
        //     outputting so HDR passthrough works when the display is
        //     in HDR mode (no downside on SDR)
        //   • tone-mapping=st2094-10 — current best-of-class HDR→SDR
        //     curve; consumes Dolby Vision dynamic metadata when present
        //   • hdr-compute-peak / hdr-peak-percentile — avoid blowing
        //     out highlights on SDR displays
        for (k, v) in [
            ("vo",                       "gpu-next"),
            ("hwdec",                    "auto-copy"),
            ("target-colorspace-hint",   "yes"),
            ("tone-mapping",             "st2094-10"),
            ("hdr-compute-peak",         "yes"),
            ("hdr-peak-percentile",      "99.9"),
            ("gamut-mapping-mode",       "perceptual"),
            ("keep-open",                "always"),  // don't auto-close on EOF
            ("keepaspect",               "yes"),
            ("border",                   "no"),       // no window decorations
            ("input-default-bindings",   "no"),       // we drive playback
            ("input-vo-keyboard",        "no"),
            ("osc",                      "no"),       // we draw our own UI
        ] {
            me.set_option_str(k, v)?;
        }

        let r = unsafe { (a.initialize)(me.handle) };
        if r < 0 {
            return Err(format!("mpv_initialize: {}", api_err(a, r)));
        }
        Ok(me)
    }

    pub fn load_file(&self, path: &Path) -> Result<(), String> {
        let p = path.to_string_lossy().to_string();
        self.command(&["loadfile", &p, "replace"])
    }

    pub fn play(&self) -> Result<(), String>  { self.set_property_str("pause", "no") }
    pub fn pause(&self) -> Result<(), String> { self.set_property_str("pause", "yes") }
    pub fn stop(&self) -> Result<(), String>  { self.command(&["stop"]) }

    pub fn seek(&self, seconds: f64) -> Result<(), String> {
        self.command(&["seek", &seconds.to_string(), "absolute"])
    }

    pub fn set_volume(&self, vol: f64) -> Result<(), String> {
        self.set_property_str("volume", &vol.clamp(0.0, 100.0).to_string())
    }

    pub fn set_muted(&self, muted: bool) -> Result<(), String> {
        self.set_property_str("mute", if muted { "yes" } else { "no" })
    }

    fn command(&self, args: &[&str]) -> Result<(), String> {
        let a = api()?;
        let cstrings: Vec<CString> = args
            .iter()
            .map(|s| CString::new(*s).unwrap_or_else(|_| CString::new("").unwrap()))
            .collect();
        let mut ptrs: Vec<*const c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let r = unsafe { (a.command)(self.handle, ptrs.as_ptr()) };
        if r < 0 {
            return Err(format!("mpv_command {:?}: {}", args, api_err(a, r)));
        }
        Ok(())
    }

    fn set_option_str(&self, name: &str, val: &str) -> Result<(), String> {
        let a = api()?;
        let n = CString::new(name).map_err(|e| e.to_string())?;
        let v = CString::new(val).map_err(|e| e.to_string())?;
        let r = unsafe { (a.set_option_str)(self.handle, n.as_ptr(), v.as_ptr()) };
        if r < 0 {
            return Err(format!("set_option_str {}={}: {}", name, val, api_err(a, r)));
        }
        Ok(())
    }

    fn set_property_str(&self, name: &str, val: &str) -> Result<(), String> {
        let a = api()?;
        let n = CString::new(name).map_err(|e| e.to_string())?;
        let v = CString::new(val).map_err(|e| e.to_string())?;
        let r = unsafe { (a.set_property_str)(self.handle, n.as_ptr(), v.as_ptr()) };
        if r < 0 {
            return Err(format!("set_property_str {}={}: {}", name, val, api_err(a, r)));
        }
        Ok(())
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            if let Ok(a) = api() {
                unsafe { (a.terminate)(self.handle) };
            }
            self.handle = std::ptr::null_mut();
        }
    }
}

fn api_err(a: &MpvApi, code: c_int) -> String {
    unsafe {
        let p = (a.error_string)(code);
        if p.is_null() {
            format!("error {}", code)
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

// ── Global player slot ──────────────────────────────────────────────────────
//
// Only one mpv context lives at a time — the lightbox plays one video at a
// time, and a fresh context per file would waste 50-100 ms of startup.  We
// also stash the placeholder HWND so we can show/hide it without re-creating
// the player.

static GLOBAL_PLAYER: OnceLock<Mutex<Option<MpvPlayer>>> = OnceLock::new();

fn global() -> &'static Mutex<Option<MpvPlayer>> {
    GLOBAL_PLAYER.get_or_init(|| Mutex::new(None))
}

// ── Tauri commands ──────────────────────────────────────────────────────────

/// Probes for libmpv at runtime.  Returns Ok(version_string) on success
/// so the frontend can decide whether to expose the mpv-backed path or
/// fall back to <video>.
pub fn mpv_probe() -> Result<String, String> {
    let a = api()?;
    let handle = unsafe { (a.create)() };
    if handle.is_null() {
        return Err("mpv_create NULL".into());
    }
    let key = CString::new("mpv-version").unwrap();
    let ver_ptr = unsafe { (a.get_property_str)(handle, key.as_ptr()) };
    let ver = if ver_ptr.is_null() {
        "unknown".to_string()
    } else {
        let s = unsafe { CStr::from_ptr(ver_ptr) }.to_string_lossy().into_owned();
        unsafe { (a.free)(ver_ptr as *mut c_void) };
        s
    };
    unsafe { (a.terminate)(handle) };
    Ok(ver)
}

/// v1.5.284 — Standalone test: open the given video in a fresh mpv
/// window (no parent), with colour-accurate config.  Used to verify
/// the mpv pipeline matches VLC before we wire it into the lightbox.
pub fn mpv_test_open(path: String) -> Result<String, String> {
    let p = std::path::PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("file not found: {}", path));
    }

    // Tear down any previous test player so the user can click multiple
    // files in a row without leaks.
    {
        let mut slot = global().lock().map_err(|_| "global lock")?;
        *slot = None;
    }

    // Standalone window — no parent HWND.  mpv creates its own
    // top-level window with the colour-accurate renderer.
    let player = MpvPlayer::new(None)?;
    player.set_option_str("force-window", "yes")?;
    player.set_option_str("title", &format!("RetinaTag (mpv test) — {}", p.file_name().unwrap_or_default().to_string_lossy()))?;
    player.load_file(&p)?;

    let mut slot = global().lock().map_err(|_| "global lock")?;
    *slot = Some(player);

    Ok(format!("mpv test window opened for {}", p.display()))
}

/// Close any active mpv player.  Frontend calls this when the lightbox
/// closes or when starting a fresh test.
pub fn mpv_close() -> Result<(), String> {
    let mut slot = global().lock().map_err(|_| "global lock")?;
    *slot = None;
    // Also drop the overlay child window so SetWindowPos calls don't
    // operate on a stale HWND.
    let mut o = overlay().lock().map_err(|_| "overlay lock")?;
    if let Some(child) = o.child_hwnd.take() {
        unsafe { destroy_child_window(child); }
    }
    o.player_active = false;
    Ok(())
}

// ── v1.5.285 — In-window overlay ────────────────────────────────────────────
//
// Lifecycle:
//   1. `mpv_show_in_window` creates (or re-uses) a child HWND parented to
//      the Tauri main window, positions it at the given rect, then
//      starts a fresh MpvPlayer with `wid` pointing at that HWND.
//   2. While the lightbox is open the frontend calls `mpv_set_rect`
//      whenever the window resizes so the video follows the layout.
//   3. When the lightbox closes, `mpv_hide_overlay` hides the HWND but
//      keeps the player around so re-open is fast.  Full teardown via
//      `mpv_close` destroys both.
//
// Z-order: WS_CHILD windows are always drawn ABOVE their parent's main
// content, but BELOW any other top-level windows that pop up over the
// main window.  This is fine for the lightbox because the controls
// (close button, ←/→ arrows) sit OUTSIDE the placeholder rect, so the
// mpv overlay doesn't cover them.

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, SetWindowPos,
    ShowWindow, CS_HREDRAW, CS_VREDRAW, HWND_TOP, HWND_TOPMOST, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER, WINDOW_EX_STYLE, WNDCLASSEXW,
    WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_POPUP, WS_VISIBLE,
};
use windows::core::PCWSTR;

// windows-rs 0.58 wants a real `system` fn pointer in `lpfnWndProc`; passing
// `DefWindowProcW` directly trips the Rust-vs-system ABI check, so we route
// every message through this trampoline.
unsafe extern "system" fn overlay_wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

struct Overlay {
    /// The child window mpv is rendering into.  `None` until the first
    /// `mpv_show_in_window` call creates it.
    child_hwnd: Option<HWND>,
    /// Whether an mpv player is currently bound to `child_hwnd`.
    player_active: bool,
    /// Last placeholder rect in *main window client coordinates*.
    /// Cached so `reposition_after_main_window_event` can re-derive
    /// the screen-space rect when the main window moves/resizes
    /// without needing a roundtrip to JS.
    last_client_rect: Option<(i32, i32, i32, i32)>,
}

unsafe impl Send for Overlay {}

static OVERLAY: OnceLock<Mutex<Overlay>> = OnceLock::new();

fn overlay() -> &'static Mutex<Overlay> {
    OVERLAY.get_or_init(|| Mutex::new(Overlay {
        child_hwnd: None,
        player_active: false,
        last_client_rect: None,
    }))
}

/// Register the window class for the mpv overlay.  Kept around even
/// though v1.5.286 ships the standalone-window flow — the embed code
/// path is still wired up via `mpv_show_in_window` for future
/// experiments.
fn ensure_window_class() -> Vec<u16> {
    static CLASS_NAME: OnceLock<Vec<u16>> = OnceLock::new();
    CLASS_NAME
        .get_or_init(|| {
            let name: Vec<u16> = "RetinaTagMpvOverlay\0".encode_utf16().collect();
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(overlay_wndproc),
                lpszClassName: PCWSTR(name.as_ptr()),
                ..Default::default()
            };
            unsafe { RegisterClassExW(&wc) };
            name
        })
        .clone()
}

/// Create a top-level borderless popup window that mpv renders into.
///
/// Why top-level instead of WS_CHILD?  WebView2 uses DirectComposition;
/// it composes its surface OVER any native child HWNDs underneath, so
/// a WS_CHILD window parented inside the Tauri main window stays
/// invisible (audio plays, video doesn't).  A top-level WS_POPUP with
/// WS_EX_TOOLWINDOW + WS_EX_NOACTIVATE sits in its own DComp swap-chain
/// above WebView2 — visible, doesn't steal focus, doesn't show in
/// Alt+Tab.  The caller is responsible for repositioning it whenever
/// the main window moves or resizes.
unsafe fn create_overlay_window(x: i32, y: i32, w: i32, h: i32) -> Result<HWND, String> {
    let class_name = ensure_window_class();
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::UI::WindowsAndMessaging::HMENU;
    let style = WS_POPUP | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN;
    let ex_style = WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
    let hwnd = CreateWindowExW(
        ex_style,
        PCWSTR(class_name.as_ptr()),
        PCWSTR(std::ptr::null()),
        style,
        x, y, w, h,
        HWND::default(),    // no parent — top-level
        HMENU::default(),
        HINSTANCE::default(),
        None,
    )
    .map_err(|e| format!("CreateWindowExW: {}", e))?;
    Ok(hwnd)
}

/// Convert (x, y) from the Tauri main window's client coordinates to
/// screen (desktop) coordinates.  The mpv overlay is a top-level
/// window so it lives in screen-coord space, but the frontend passes
/// the placeholder div's getBoundingClientRect — which is client-area
/// relative.  Doing the conversion in Rust keeps the JS side simple
/// and immune to title-bar / window-frame size differences.
unsafe fn client_to_screen(parent: HWND, x: i32, y: i32) -> (i32, i32) {
    let mut p = POINT { x, y };
    let _ = ClientToScreen(parent, &mut p);
    (p.x, p.y)
}

unsafe fn destroy_child_window(hwnd: HWND) {
    let _ = DestroyWindow(hwnd);
}

fn main_window_hwnd(app: &tauri::AppHandle) -> Result<HWND, String> {
    use tauri::Manager;
    let win = app.get_webview_window("main").ok_or_else(|| "main window not found".to_string())?;
    let raw = win.hwnd().map_err(|e| e.to_string())?;
    Ok(HWND(raw.0 as *mut _))
}

pub fn mpv_show_in_window(
    app: tauri::AppHandle,
    path: String,
    x: i32, y: i32, w: i32, h: i32,
    muted: bool,
) -> Result<(), String> {
    let file = std::path::PathBuf::from(&path);
    if !file.exists() {
        return Err(format!("file not found: {}", path));
    }

    let parent = main_window_hwnd(&app)?;
    let (sx, sy) = unsafe { client_to_screen(parent, x, y) };

    let mut o = overlay().lock().map_err(|_| "overlay lock")?;
    o.last_client_rect = Some((x, y, w, h));
    let child_hwnd = match o.child_hwnd {
        Some(existing) => {
            unsafe {
                SetWindowPos(existing, HWND_TOPMOST, sx, sy, w, h, SWP_NOACTIVATE)
                    .map_err(|e| e.to_string())?;
            }
            existing
        }
        None => {
            let new_hwnd = unsafe { create_overlay_window(sx, sy, w, h)? };
            // Ensure first-time creation also lands at topmost Z — without
            // this the popup sits below WebView2's DComposition surface.
            unsafe {
                let _ = SetWindowPos(new_hwnd, HWND_TOPMOST, sx, sy, w, h, SWP_NOACTIVATE);
            }
            o.child_hwnd = Some(new_hwnd);
            new_hwnd
        }
    };
    unsafe { let _ = ShowWindow(child_hwnd, SW_SHOWNOACTIVATE); }

    {
        let mut slot = global().lock().map_err(|_| "global lock")?;
        *slot = None;
    }

    let player = MpvPlayer::new(Some(child_hwnd.0 as isize))?;
    if muted {
        player.set_muted(true)?;
    }
    player.load_file(&file)?;

    {
        let mut slot = global().lock().map_err(|_| "global lock")?;
        *slot = Some(player);
    }
    o.player_active = true;
    Ok(())
}

pub fn mpv_set_rect(app: tauri::AppHandle, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
    let mut o = overlay().lock().map_err(|_| "overlay lock")?;
    let Some(child) = o.child_hwnd else { return Ok(()) };
    o.last_client_rect = Some((x, y, w, h));
    let parent = main_window_hwnd(&app)?;
    let (sx, sy) = unsafe { client_to_screen(parent, x, y) };
    unsafe {
        SetWindowPos(child, HWND_TOPMOST, sx, sy, w, h, SWP_NOACTIVATE)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Called by lib.rs from `on_window_event` when the main window moves
/// or resizes.  Re-applies the cached client-area rect — the screen
/// coords change implicitly because `ClientToScreen` queries the
/// window's current position.
///
/// No-ops when the overlay isn't active or no rect has been set yet.
pub fn reposition_after_main_window_event(app: &tauri::AppHandle) {
    let o = overlay().lock();
    let Ok(o) = o else { return; };
    let Some(child) = o.child_hwnd else { return; };
    let Some((x, y, w, h)) = o.last_client_rect else { return; };
    let Ok(parent) = main_window_hwnd(app) else { return; };
    let (sx, sy) = unsafe { client_to_screen(parent, x, y) };
    unsafe {
        let _ = SetWindowPos(child, HWND_TOPMOST, sx, sy, w, h, SWP_NOACTIVATE);
    }
}

/// Hide / show the overlay window without destroying it.  Used by the
/// window-event handler when the main window is minimised / loses
/// focus — the popup is top-level so without this it would keep
/// floating over the desktop / other apps.
pub fn set_overlay_visible(visible: bool) {
    let Ok(o) = overlay().lock() else { return; };
    let Some(child) = o.child_hwnd else { return; };
    let cmd = if visible { SW_SHOWNOACTIVATE } else { SW_HIDE };
    unsafe { let _ = ShowWindow(child, cmd); }
}

pub fn mpv_hide_overlay() -> Result<(), String> {
    let o = overlay().lock().map_err(|_| "overlay lock")?;
    let Some(child) = o.child_hwnd else { return Ok(()) };
    unsafe { let _ = ShowWindow(child, SW_HIDE); }
    // Pause the player so audio stops, but keep the context alive for fast re-show.
    if let Ok(slot) = global().lock() {
        if let Some(p) = slot.as_ref() {
            let _ = p.pause();
        }
    }
    Ok(())
}

pub fn mpv_set_paused(paused: bool) -> Result<(), String> {
    let slot = global().lock().map_err(|_| "global lock")?;
    let p = slot.as_ref().ok_or_else(|| "no active mpv player".to_string())?;
    if paused { p.pause() } else { p.play() }
}

} // mod imp
