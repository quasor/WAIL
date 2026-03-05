fn main() {
    // Ensure plugin resource paths exist so tauri_build doesn't fail during
    // `cargo check` when plugins haven't been built yet. Real builds always
    // run `cargo xtask build-plugin` first which produces the actual files.
    let bundled = std::path::Path::new("../../target/bundled");
    for name in ["wail-plugin-send", "wail-plugin-recv"] {
        let clap = bundled.join(format!("{name}.clap"));
        if !clap.exists() {
            std::fs::create_dir_all(bundled).ok();
            std::fs::write(&clap, []).ok();
        }
        let vst3 = bundled.join(format!("{name}.vst3"));
        if !vst3.exists() {
            std::fs::create_dir_all(&vst3).ok();
        }
    }

    tauri_build::build();
}
