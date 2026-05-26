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
pub fn mpv_test_open(app: tauri::AppHandle, path: String) -> Result<String, String> {
    #[cfg(target_os = "windows")] { imp::mpv_test_open(app, path) }
    #[cfg(not(target_os = "windows"))] {
        let _ = (app, path);
        Err("libmpv is Windows-only in this build".into())
    }
}

#[tauri::command]
pub fn mpv_close() -> Result<(), String> {
    #[cfg(target_os = "windows")] { imp::mpv_close() }
    #[cfg(not(target_os = "windows"))] { Ok(()) }
}

// v1.5.294 — In-window overlay commands (mpv_show_in_window /
// mpv_set_rect / mpv_hide_overlay / mpv_set_paused) and the
// window-event helpers (on_main_window_geometry_changed /
// set_overlay_visible) are removed from the public Tauri command
// surface.  Their Rust impl stays in `imp` (marked allow(dead_code))
// as scaffolding for the future WebGL-canvas inline approach.

#[cfg(target_os = "windows")]
#[allow(dead_code)]  // overlay code stays for the future WebGL-canvas approach
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
        // parent.  v1.5.288: pass the full 64-bit HWND value; mpv on
        // Windows expects the native handle as int64.  The earlier
        // `as u32 -> as i64` round-trip was the recommended workaround
        // for a 0.36-era sign-extension bug (issue #10189) that no
        // longer affects current builds and silently truncates any
        // HWND with high bits set.
        if let Some(hwnd) = parent_hwnd {
            let mut wid_i64: i64 = hwnd as i64;
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
pub fn mpv_test_open(app: tauri::AppHandle, path: String) -> Result<String, String> {
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
    // v1.5.288 — show on-screen-controller (play/pause/seek/volume) so
    // the user doesn't see an interactionless surface.  We turned it
    // off in `new()` for the embed path; the standalone window WANTS
    // controls visible.
    player.set_option_str("osc", "yes")?;
    player.set_option_str("force-window", "yes")?;
    player.set_option_str("title", &format!("RetinaTag — {}", p.file_name().unwrap_or_default().to_string_lossy()))?;
    player.load_file(&p)?;

    let mut slot = global().lock().map_err(|_| "global lock")?;
    *slot = Some(player);

    // v1.5.288 — mpv's window steals foreground focus when it first
    // appears, so Tauri loses keyboard focus and ← / → no longer
    // navigates the gallery.  Bring the Tauri main window back to the
    // foreground (mpv's video stays painted thanks to its own DComp
    // surface — only the keyboard focus moves).
    if let Ok(parent) = main_window_hwnd(&app) {
        use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
        unsafe { let _ = SetForegroundWindow(parent); }
    }

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

/// Register the window class for the mpv overlay.  Background brush
/// is magenta as a diagnostic — if you see magenta where the video
/// should be, the overlay WS_CHILD is visible but mpv isn't painting;
/// if you don't see magenta either, the overlay is occluded or never
/// created.
fn ensure_window_class() -> Vec<u16> {
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Gdi::CreateSolidBrush;
    static CLASS_NAME: OnceLock<Vec<u16>> = OnceLock::new();
    CLASS_NAME
        .get_or_init(|| {
            let name: Vec<u16> = "RetinaTagMpvOverlay\0".encode_utf16().collect();
            let magenta = unsafe { CreateSolidBrush(COLORREF(0x00FF00FF)) };
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(overlay_wndproc),
                lpszClassName: PCWSTR(name.as_ptr()),
                hbrBackground: magenta,
                ..Default::default()
            };
            unsafe { RegisterClassExW(&wc) };
            name
        })
        .clone()
}

/// Create a WS_CHILD window parented to the Tauri main HWND.
///
/// v1.5.287 — switched from WS_POPUP top-level back to WS_CHILD now
/// that the main window is configured with `transparent: true`.  With
/// that, WebView2 composites with alpha — wherever the HTML paints
/// transparent pixels, the WS_CHILD HWND BELOW the WebView2 surface
/// shows through.  This produces a *truly inline* video player: it
/// follows the window automatically (Windows handles WS_CHILD layout
/// for us) and lives inside the same window so Alt+Tab / move /
/// minimise just work.
unsafe fn create_overlay_window(parent: HWND, x: i32, y: i32, w: i32, h: i32) -> Result<HWND, String> {
    let class_name = ensure_window_class();
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::UI::WindowsAndMessaging::HMENU;
    let style = WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS;
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(class_name.as_ptr()),
        PCWSTR(std::ptr::null()),
        style,
        x, y, w, h,
        parent,
        HMENU::default(),
        HINSTANCE::default(),
        None,
    )
    .map_err(|e| format!("CreateWindowExW: {}", e))?;
    Ok(hwnd)
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
    eprintln!("[mpv] show_in_window: path={} rect=({},{},{},{}) muted={}", path, x, y, w, h, muted);
    let file = std::path::PathBuf::from(&path);
    if !file.exists() {
        eprintln!("[mpv] file not found: {}", path);
        return Err(format!("file not found: {}", path));
    }

    let parent = main_window_hwnd(&app)?;
    eprintln!("[mpv] parent hwnd={:?}", parent.0);

    let mut o = overlay().lock().map_err(|_| "overlay lock")?;
    o.last_client_rect = Some((x, y, w, h));
    let child_hwnd = match o.child_hwnd {
        Some(existing) => {
            eprintln!("[mpv] reusing overlay hwnd={:?}", existing.0);
            unsafe {
                SetWindowPos(existing, HWND_TOP, x, y, w, h, SWP_NOACTIVATE)
                    .map_err(|e| e.to_string())?;
            }
            existing
        }
        None => {
            let new_hwnd = unsafe { create_overlay_window(parent, x, y, w, h)? };
            eprintln!("[mpv] created overlay hwnd={:?}", new_hwnd.0);
            // Bring the child above WebView2 in the parent's Z-order so
            // it isn't immediately occluded.
            unsafe {
                let _ = SetWindowPos(new_hwnd, HWND_TOP, x, y, w, h, SWP_NOACTIVATE);
            }
            o.child_hwnd = Some(new_hwnd);
            new_hwnd
        }
    };
    unsafe { let _ = ShowWindow(child_hwnd, SW_SHOWNOACTIVATE); }
    eprintln!("[mpv] overlay shown");

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

pub fn mpv_set_rect(_app: tauri::AppHandle, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
    let mut o = overlay().lock().map_err(|_| "overlay lock")?;
    let Some(child) = o.child_hwnd else { return Ok(()) };
    o.last_client_rect = Some((x, y, w, h));
    unsafe {
        SetWindowPos(child, HWND_TOP, x, y, w, h, SWP_NOACTIVATE)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Called by lib.rs from `on_window_event` when the main window moves
/// or resizes.  WS_CHILD windows are repositioned automatically by
/// the OS when the parent moves — but Resized events can still
/// change layout that the frontend hasn't re-measured yet.  This
/// no-op preserves the API hook so lib.rs's event handler doesn't
/// need to be re-wired for the WS_CHILD switch.
pub fn reposition_after_main_window_event(_app: &tauri::AppHandle) {
    // WS_CHILD: positioning happens inside the parent's client area
    // and follows the parent automatically.  JS calls mpv_set_rect
    // explicitly when the user-visible rect inside the lightbox
    // needs to change.
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
