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
    }

    tauri_build::build()
}
