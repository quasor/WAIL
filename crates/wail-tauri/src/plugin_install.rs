use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// All plugin bundle names we expect to find.
const PLUGIN_NAMES: &[&str] = &[
    "wail-plugin-send.clap",
    "wail-plugin-recv.clap",
    "wail-plugin-send.vst3",
    "wail-plugin-recv.vst3",
];

struct PluginDirs {
    clap: PathBuf,
    vst3: PathBuf,
}

fn system_plugin_dirs() -> Option<PluginDirs> {
    let home = dirs::home_dir()?;
    #[cfg(target_os = "macos")]
    {
        let base = home.join("Library/Audio/Plug-Ins");
        Some(PluginDirs { clap: base.join("CLAP"), vst3: base.join("VST3") })
    }
    #[cfg(target_os = "linux")]
    {
        Some(PluginDirs { clap: home.join(".clap"), vst3: home.join(".vst3") })
    }
    #[cfg(target_os = "windows")]
    {
        let base = PathBuf::from(std::env::var("COMMONPROGRAMFILES").ok()?);
        Some(PluginDirs { clap: base.join("CLAP"), vst3: base.join("VST3") })
    }
}

/// Returns true if `dir` contains at least one expected plugin bundle.
fn dir_has_plugins(dir: &Path) -> bool {
    PLUGIN_NAMES.iter().any(|name| dir.join(name).exists())
}

/// Locate the directory containing bundled plugin files.
///
/// Search order:
/// 1. Tauri resource dir: `{resource_dir}/plugins/`
/// 2. Homebrew-style: `{exe_dir}/../lib/` (resolves Cellar symlinks)
pub fn find_plugin_dir(resource_dir: Option<&Path>) -> Option<PathBuf> {
    // 1. Tauri bundle: resource_dir/plugins/
    if let Some(rd) = resource_dir {
        let p = rd.join("plugins");
        if dir_has_plugins(&p) {
            tracing::debug!("plugin_install: found plugins in Tauri resource dir: {}", p.display());
            return Some(p);
        }
    }

    // 2. Homebrew: {exe}/../lib/
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(canon) = exe.canonicalize() {
            if let Some(bin) = canon.parent() {
                let lib = bin.join("../lib");
                let lib = lib.canonicalize().unwrap_or(lib);
                if dir_has_plugins(&lib) {
                    tracing::debug!("plugin_install: found plugins in sibling lib dir: {}", lib.display());
                    return Some(lib);
                }
            }
        }
    }

    None
}

/// Install plugins from a source directory if they are not already present in
/// the system plugin directories. `plugin_dir` should directly contain the
/// plugin bundles (e.g., `wail-plugin-send.clap`).
///
/// Returns a list of error messages for any plugins that failed to install
/// (already-present and missing-source plugins are not errors).
pub fn install_if_missing(plugin_dir: &Path) -> Vec<String> {
    let Some(dirs) = system_plugin_dirs() else {
        let msg = "Could not determine system plugin directories. Please install WAIL Send and WAIL Recv manually using cargo xtask install-plugin.".to_string();
        warn!("plugin_install: {msg}");
        return vec![msg];
    };

    let plugins: &[(&str, &Path)] = &[
        ("wail-plugin-send.clap", &dirs.clap),
        ("wail-plugin-recv.clap", &dirs.clap),
        ("wail-plugin-send.vst3", &dirs.vst3),
        ("wail-plugin-recv.vst3", &dirs.vst3),
    ];

    let mut errors = Vec::new();
    for (name, dest_dir) in plugins {
        let src = plugin_dir.join(name);
        let dest = dest_dir.join(name);

        if dest.exists() {
            continue;
        }
        if !src.exists() {
            warn!("plugin_install: bundled plugin not found: {}", src.display());
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(dest_dir) {
            let msg = format!("Failed to create plugin directory {}: {e}", dest_dir.display());
            warn!("plugin_install: {msg}");
            errors.push(msg);
            continue;
        }
        match copy_path(&src, &dest) {
            Ok(()) => info!("plugin_install: installed {name} → {}", dest_dir.display()),
            Err(e) => {
                let msg = format!("Failed to install {name}: {e}");
                warn!("plugin_install: {msg}");
                errors.push(msg);
            }
        }
    }
    errors
}

/// Copy a file or directory recursively.
fn copy_path(src: &Path, dest: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_path(&entry.path(), &dest.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(src, dest)?;
    }
    Ok(())
}
