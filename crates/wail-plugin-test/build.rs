use std::path::PathBuf;

/// On macOS, a valid plugin bundle is a directory with Contents/MacOS/ inside.
/// An empty directory left over from a stale build is not valid.
/// On Linux/Windows, a valid bundle is a non-empty file.
fn bundle_is_valid(path: &PathBuf) -> bool {
    #[cfg(target_os = "macos")]
    return path.join("Contents/MacOS").is_dir();
    #[cfg(not(target_os = "macos"))]
    return path.is_file() && path.metadata().map(|m| m.len() > 0).unwrap_or(false);
}

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace_root = PathBuf::from(&manifest_dir)
        .parent()
        .unwrap() // crates/wail-plugin-test -> crates/
        .parent()
        .unwrap() // crates/ -> workspace root
        .to_path_buf();

    // Respect CARGO_TARGET_DIR (set by Conductor workspaces / git worktrees)
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root.join("target"));

    let recv_bundle = target_dir.join("bundled/wail-plugin-recv.clap");
    let send_bundle = target_dir.join("bundled/wail-plugin-send.clap");

    if !bundle_is_valid(&recv_bundle) || !bundle_is_valid(&send_bundle) {
        // NOTE: We cannot spawn `cargo xtask bundle-plugin` here because the
        // outer cargo process holds the workspace lock, causing the inner cargo
        // to block forever (deadlock). Instead, fail fast with a clear message.
        panic!(
            "Plugin bundles missing. Build them first:\n\
             \n  cargo xtask bundle-plugin --debug\n\
             \nOr use `cargo xtask test` which handles this automatically."
        );
    }

    // Rebuild if the plugin bundles are replaced
    println!("cargo:rerun-if-changed={}", recv_bundle.display());
    println!("cargo:rerun-if-changed={}", send_bundle.display());
    // Rebuild if plugin source changes
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root.join("crates/wail-plugin-recv/src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root.join("crates/wail-plugin-send/src").display()
    );
}
