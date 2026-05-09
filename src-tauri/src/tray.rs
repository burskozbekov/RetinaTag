//! System tray + taskbar progress/badge support.
//!
//! On Windows (the only platform we ship for right now) this wires:
//!   1. A persistent tray icon with a context menu ("Show / Scan / Quit")
//!      and a left-click action that restores the main window.
//!   2. A `set_tray_progress(percent)` command the frontend can call during
//!      long-running scans / tagging to update the tray tooltip.
//!
//! The tray is declared lazily in `run()` so it only appears on desktop
//! builds and we don't pay its cost in mobile/headless.

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, Runtime,
};

/// Build the tray icon + menu and hook it up to the running app. Call once
/// from `setup(|app| ...)`. Errors are non-fatal — a missing tray just
/// means no tray icon; the window stays usable.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "show", "Show RetinaTag", true, None::<&str>)?;
    let scan_item = MenuItem::with_id(app, "scan", "Add folder…", true, None::<&str>)?;
    let watch_scan_item = MenuItem::with_id(app, "scan_watch", "Scan watch folders now", true, None::<&str>)?;
    let hide_item = MenuItem::with_id(app, "hide", "Hide", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&show_item, &scan_item, &watch_scan_item, &hide_item, &quit_item],
    )?;

    let _tray = TrayIconBuilder::with_id("retina-main-tray")
        .tooltip("RetinaTag")
        .icon(app.default_window_icon().cloned().unwrap_or_else(|| {
            // Fallback to an empty 1×1 icon if the default can't be loaded.
            // The tray library still requires *some* icon or it won't render.
            tauri::image::Image::new_owned(vec![0, 0, 0, 0], 1, 1)
        }))
        .menu(&menu)
        .on_menu_event(move |app: &AppHandle<R>, event| match event.id.as_ref() {
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                }
            }
            "hide" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.hide();
                }
            }
            "scan" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                    let _ = w.emit("tray-add-folder", ());
                }
            }
            "scan_watch" => {
                // Don't force the window open — the whole point is the user
                // wants a background scan. Frontend listens and kicks off
                // scans on each enabled watch folder.
                let _ = app.emit("tray-scan-watch-folders", ());
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray: &TrayIcon<R>, event| {
            // Left-click = show/restore window (common Windows convention).
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                }
            }
        })
        .build(app)?;

    Ok(())
}

/// Update the tray tooltip with a progress label. Called from commands.rs
/// while scans or tagging are in flight. `progress_pct` is 0..100 or None to
/// clear.
pub fn set_tray_tooltip<R: Runtime>(app: &AppHandle<R>, label: &str) {
    if let Some(tray) = app.tray_by_id("retina-main-tray") {
        let _ = tray.set_tooltip(Some(label));
    }
}
