use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

// ---------------------------------------------------------------------------
// Help
// ---------------------------------------------------------------------------

const HELP: &str = "\
cargo xtask <TASK> [OPTIONS]

TASKS:
  install         Build, install plugins + wail-app CLI to PATH (full setup)
  build-plugin    Build the CLAP and VST3 plugin bundles
  install-plugin  Build (optional) and install to system plugin directories
  package-plugin  Create a macOS .pkg installer (macOS only)
  run-peer        Start a WAIL peer and join a room
  run-tauri       Run the Tauri desktop app in dev mode
  build-tauri     Build plugins, then build the Tauri distributable

OPTIONS (install):
  --no-plugin-build  Skip plugin build; use existing bundles in target/bundled/

OPTIONS (build-plugin, install-plugin):
  --debug         Build in debug mode instead of release

OPTIONS (install-plugin):
  --no-build      Skip the build step; install existing bundles

OPTIONS (package-plugin):
  --no-build      Skip the build step; package existing bundles

OPTIONS (run-peer): all flags are forwarded to `wail-app join`
  --room <NAME>   Room to join          (required)
  --bpm  <BPM>    Initial tempo         (default: 120)
  --ipc-port <N>  IPC port for plugin   (default: 9191)
  --server <URL>  Signaling server URL  (default: https://wail.val.run/)
  --bars <N>      Bars per interval     (default: 4)
  --quantum <F>   Quantum               (default: 4.0)
  --name <NAME>   Display name for this peer
  --password <PW> Room password (first peer sets it; others must match)

EXAMPLES:
  cargo xtask install
  cargo xtask install --no-plugin-build
  cargo xtask build-plugin
  cargo xtask install-plugin
  cargo xtask install-plugin --no-build
  cargo xtask package-plugin
  cargo xtask package-plugin --no-build
  cargo xtask run-peer --room jam --password secret
  cargo xtask run-peer --room jam --password secret --bpm 96 --ipc-port 9192
  cargo xtask run-peer --room jam --password secret --name Quasor
  cargo xtask run-tauri
  cargo xtask build-tauri
";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let mut args: Vec<String> = env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("install") => {
            args.remove(0);
            install_all(&args)
        }
        Some("build-plugin") => {
            args.remove(0);
            build_plugin(&args)
        }
        Some("install-plugin") => {
            args.remove(0);
            install_plugin(&args)
        }
        Some("package-plugin") => {
            args.remove(0);
            package_plugin(&args)
        }
        Some("run-peer") => {
            args.remove(0);
            run_peer(&args)
        }
        Some("run-tauri") => {
            args.remove(0);
            run_tauri()
        }
        Some("build-tauri") => {
            args.remove(0);
            build_tauri()
        }
        Some(task) => bail!("Unknown task: {task}\n\n{HELP}"),
        None => {
            print!("{HELP}");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

fn build_plugin(args: &[String]) -> Result<()> {
    let release = !args.contains(&"--debug".to_string());
    println!(
        "Building WAIL plugin ({})...",
        if release { "release" } else { "debug" }
    );

    let mut cmd = Command::new("cargo");
    cmd.args(["nih-plug", "bundle", "wail-plugin"]);
    if release {
        cmd.arg("--release");
    }
    cmd.current_dir(workspace_dir());
    run_cmd(cmd).context("cargo nih-plug bundle failed")?;

    let profile = if release { "release" } else { "debug" };
    println!("\nPlugin bundles:");
    println!("  target/bundled/wail-plugin.clap");
    println!("  target/bundled/wail-plugin.vst3");
    println!("\nBuilt with profile: {profile}");
    Ok(())
}

fn install_plugin(args: &[String]) -> Result<()> {
    let no_build = args.contains(&"--no-build".to_string());
    let debug = args.contains(&"--debug".to_string());

    if !no_build {
        let build_args: Vec<String> = if debug {
            vec!["--debug".to_string()]
        } else {
            vec![]
        };
        build_plugin(&build_args)?;
    }

    let root = workspace_dir();
    let clap_bundle = root.join("target/bundled/wail-plugin.clap");
    let vst3_bundle = root.join("target/bundled/wail-plugin.vst3");

    for path in [&clap_bundle, &vst3_bundle] {
        if !path.exists() {
            bail!(
                "{} not found — run `cargo xtask build-plugin` first",
                path.display()
            );
        }
    }

    let (clap_dir, vst3_dir) = plugin_dirs()?;
    fs::create_dir_all(&clap_dir)
        .with_context(|| format!("Could not create {}", clap_dir.display()))?;
    fs::create_dir_all(&vst3_dir)
        .with_context(|| format!("Could not create {}", vst3_dir.display()))?;

    copy_bundle(&clap_bundle, &clap_dir)?;
    copy_bundle(&vst3_bundle, &vst3_dir)?;

    println!("\nDone. Rescan plugins in your DAW to pick up the changes.");
    Ok(())
}

fn package_plugin(args: &[String]) -> Result<()> {
    #[cfg(not(target_os = "macos"))]
    bail!("package-plugin is only supported on macOS");

    #[cfg(target_os = "macos")]
    {
        let no_build = args.contains(&"--no-build".to_string());
        if !no_build {
            build_plugin(&[])?;
        }

        let root = workspace_dir();
        let version = cargo_version(&root)?;

        let clap_src = root.join("target/bundled/wail-plugin.clap");
        let vst3_src = root.join("target/bundled/wail-plugin.vst3");
        for path in [&clap_src, &vst3_src] {
            if !path.exists() {
                bail!(
                    "{} not found — run `cargo xtask build-plugin` first",
                    path.display()
                );
            }
        }

        let payload = root.join("target/pkg_payload");
        let clap_dest = payload.join("Library/Audio/Plug-Ins/CLAP");
        let vst3_dest = payload.join("Library/Audio/Plug-Ins/VST3");
        if payload.exists() {
            fs::remove_dir_all(&payload).context("Could not clean pkg_payload")?;
        }
        fs::create_dir_all(&clap_dest)?;
        fs::create_dir_all(&vst3_dest)?;
        copy_bundle(&clap_src, &clap_dest)?;
        copy_bundle(&vst3_src, &vst3_dest)?;

        let pkg_path = root.join(format!("target/wail-plugin-{version}-macos.pkg"));
        let mut pkgbuild = Command::new("pkgbuild");
        pkgbuild
            .arg("--identifier")
            .arg("com.wail.plugin")
            .arg("--version")
            .arg(&version)
            .arg("--root")
            .arg(&payload)
            .arg(&pkg_path);
        run_cmd(pkgbuild).context("pkgbuild failed")?;

        println!("Created: {}", pkg_path.display());
        Ok(())
    }
}

fn cargo_version(root: &Path) -> Result<String> {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version=1", "--no-deps"])
        .current_dir(root)
        .output()
        .context("failed to run cargo metadata")?;
    let meta: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("failed to parse cargo metadata")?;
    meta["packages"]
        .as_array()
        .context("packages not an array")?
        .iter()
        .find(|p| p["name"] == "wail-app")
        .and_then(|p| p["version"].as_str())
        .map(|s| s.to_owned())
        .context("Could not find wail-app version in cargo metadata")
}

fn run_peer(extra_args: &[String]) -> Result<()> {
    // Build up the default join args, then let extra_args override/extend them.
    // wail-app's clap parser accepts duplicate flags and uses the last value,
    // so passing defaults first and user args after achieves natural overriding.
    //
    // Strip any leading "--" separator that cargo passes through.
    let extra: Vec<&String> = extra_args.iter().filter(|a| a.as_str() != "--").collect();

    // Parse user-supplied flags so we can skip defaults they've overridden.
    let has_flag = |flag: &str| extra.iter().any(|a| a.as_str() == flag);

    let mut args: Vec<&str> = Vec::new();
    if !has_flag("--room") && !has_flag("-r") {
        eprintln!("Error: --room is required\n");
        print!("{HELP}");
        std::process::exit(1);
    }
    if !has_flag("--server") {
        args.extend(["--server", "https://wail.val.run/"]);
    }
    if !has_flag("--bpm") {
        args.extend(["--bpm", "120"]);
    }
    if !has_flag("--bars") {
        args.extend(["--bars", "4"]);
    }
    if !has_flag("--quantum") {
        args.extend(["--quantum", "4"]);
    }
    if !has_flag("--ipc-port") {
        args.extend(["--ipc-port", "9191"]);
    }

    println!("Starting WAIL peer...");
    let mut cmd = Command::new("cargo");
    cmd.args(["run", "-p", "wail-app", "--", "join"])
        .args(&args)
        .args(&extra)
        .env(
            "RUST_LOG",
            env::var("RUST_LOG")
                .unwrap_or_else(|_| "wail_app=info,wail_core=info,wail_net=info".into()),
        )
        .current_dir(workspace_dir());
    run_cmd(cmd)
}

fn run_tauri() -> Result<()> {
    println!("Starting WAIL Tauri app in dev mode...");
    let mut cmd = Command::new("cargo");
    cmd.args(["tauri", "dev", "-c", "crates/wail-tauri/tauri.conf.json"])
        .current_dir(workspace_dir());
    run_cmd(cmd)
}

fn build_tauri() -> Result<()> {
    // Build plugins first — they're bundled as resources
    println!("Building plugins first...");
    build_plugin(&[])?;

    println!("\nBuilding WAIL Tauri app...");
    let mut cmd = Command::new("cargo");
    cmd.args(["tauri", "build", "-c", "crates/wail-tauri/tauri.conf.json"])
        .current_dir(workspace_dir());
    run_cmd(cmd)
}

fn install_all(args: &[String]) -> Result<()> {
    let no_plugin_build = args.contains(&"--no-plugin-build".to_string());

    // Step 1: ensure cargo-nih-plug is available
    ensure_nih_plug()?;

    // Step 2: build + install plugins
    let plugin_args: Vec<String> = if no_plugin_build {
        vec!["--no-build".to_string()]
    } else {
        vec![]
    };
    install_plugin(&plugin_args)?;

    // Step 3: install wail-app binary to ~/.cargo/bin
    println!("\nInstalling wail-app binary...");
    let root = workspace_dir();
    let mut cmd = Command::new(env!("CARGO"));
    cmd.args(["install", "--path", "crates/wail-app", "--locked"])
        .current_dir(&root);
    run_cmd(cmd).context("cargo install wail-app failed")?;

    // Step 4: next-steps instructions
    println!("\n=== WAIL installed successfully ===");
    println!("To join a session:");
    println!("  wail-app join --room <ROOM> --password <PASSWORD>");
    println!();
    println!("Example:");
    println!("  wail-app join --room myband --password secret");
    Ok(())
}

fn ensure_nih_plug() -> Result<()> {
    let already_installed = Command::new("cargo")
        .args(["nih-plug", "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if already_installed {
        return Ok(());
    }

    println!("Installing cargo-nih-plug (this takes a few minutes the first time)...");
    let mut cmd = Command::new(env!("CARGO"));
    cmd.args([
        "install",
        "--git",
        "https://github.com/robbert-vdh/nih-plug.git",
        "cargo-nih-plug",
    ]);
    run_cmd(cmd).context("Failed to install cargo-nih-plug")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn workspace_dir() -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["locate-project", "--workspace", "--message-format=plain"])
        .output()
        .expect("failed to run cargo locate-project");
    let path = String::from_utf8(output.stdout).expect("non-utf8 path");
    Path::new(path.trim()).parent().unwrap().to_owned()
}

fn run_cmd(mut cmd: Command) -> Result<()> {
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd.status().context("failed to spawn command")?;
    if !status.success() {
        bail!("command exited with {status}");
    }
    Ok(())
}

/// Recursively copy a bundle (file or directory) to a destination directory.
fn copy_bundle(src: &Path, dest_dir: &Path) -> Result<()> {
    let dest = dest_dir.join(src.file_name().unwrap());

    if src.is_dir() {
        // Remove old version if present
        if dest.exists() {
            fs::remove_dir_all(&dest)
                .with_context(|| format!("Could not remove old {}", dest.display()))?;
        }
        copy_dir_all(src, &dest)?;
    } else {
        fs::copy(src, &dest)
            .with_context(|| format!("Could not copy {} to {}", src.display(), dest.display()))?;
    }

    println!("Installed: {}", dest.display());
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn plugin_dirs() -> Result<(PathBuf, PathBuf)> {
    #[cfg(target_os = "macos")]
    {
        let base = home_dir()?.join("Library/Audio/Plug-Ins");
        Ok((base.join("CLAP"), base.join("VST3")))
    }
    #[cfg(target_os = "linux")]
    {
        let home = home_dir()?;
        Ok((home.join(".clap"), home.join(".vst3")))
    }
    #[cfg(target_os = "windows")]
    {
        let common = PathBuf::from(
            env::var("COMMONPROGRAMFILES").context("COMMONPROGRAMFILES not set")?,
        );
        Ok((common.join("CLAP"), common.join("VST3")))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    bail!("Unsupported platform")
}

fn home_dir() -> Result<PathBuf> {
    env::var("HOME")
        .map(PathBuf::from)
        .context("HOME environment variable not set")
}
