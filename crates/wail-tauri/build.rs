fn main() {
    // Ensure plugin resource paths exist so tauri_build doesn't fail during
    // `cargo check` when plugins haven't been built yet. Real builds always
    // run `cargo xtask build-plugin` first which produces the actual files.
    let bundled = std::path::Path::new("../../target/bundled");
    for name in ["wail-plugin-send", "wail-plugin-recv"] {
        let clap = bundled.join(format!("{name}.clap"));
        if !clap.exists() {
            std::fs::create_dir_all(&clap).ok();
        }
        let vst3 = bundled.join(format!("{name}.vst3"));
        if !vst3.exists() {
            std::fs::create_dir_all(&vst3).ok();
        }
    }

    // Clean stale tauri_build resource output. CI caches the target/ directory
    // which includes OUT_DIR; if the resource mapping in tauri.conf.json changed,
    // stale file/directory entries cause conflicts (EEXIST, EISDIR).
    if let Ok(out_dir) = std::env::var("OUT_DIR") {
        let _ = std::fs::remove_dir_all(&out_dir);
        let _ = std::fs::create_dir_all(&out_dir);
    }

    // Hard-fail only when building via Tauri CLI (TAURI_CONFIG is set by `cargo tauri build/dev`).
    // During `cargo test`, TAURI_CONFIG is absent and tauri_build panics on plugin bundle
    // directories containing Mach-O dylibs — emit a warning instead.
    match tauri_build::try_build(tauri_build::Attributes::new()) {
        Ok(()) => {}
        Err(e) if std::env::var_os("TAURI_CONFIG").is_none() => {
            println!("cargo:warning=tauri_build: {e} (non-Tauri build, skipping)");
        }
        Err(e) => panic!("tauri_build failed: {e}"),
    }
}
