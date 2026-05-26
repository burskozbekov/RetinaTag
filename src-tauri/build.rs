fn main() {
    // Force re-embed dist/ on every build
    println!("cargo:rerun-if-changed=../dist/index.html");

    // Windows: delay-load directml.dll so the app starts even if GPU is unavailable.
    // Without this, Windows kills the process before main() if directml.dll is missing.
    // With DELAYLOAD, the DLL is only loaded when DirectML functions are first called,
    // letting our Rust fallback code catch the error and use CPU instead.
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rustc-link-arg=/DELAYLOAD:directml.dll");
        println!("cargo:rustc-link-lib=delayimp");

        // v1.5.283 — libmpv embed for accurate HEVC HDR video playback.
        // We ship libmpv-2.dll (zhongfly LGPL build) under
        // third-party/mpv/ and load it at runtime via `libloading`
        // (see src/video_player.rs).  Loading dynamically lets the app
        // start even if the DLL is missing or fails to load, and avoids
        // the MinGW (.dll.a) vs MSVC (.lib) import-library mismatch.
    }

    tauri_build::build()
}
