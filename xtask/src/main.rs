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
  install         Build and install plugins to system plugin directories
  build-plugin    Build the CLAP and VST3 plugin bundles (requires cargo-nih-plug)
  bundle-plugin   Build and assemble plugin bundles without cargo-nih-plug
  install-plugin  Build (optional) and install to system plugin directories
  package-plugin  Create a macOS .pkg installer (macOS only)
  run-tauri       Run the Tauri desktop app in dev mode
  build-tauri     Build plugins, then build the Tauri distributable
  test            Build plugins if missing, then run cargo test
  run-turn        Start a local coturn TURN server

OPTIONS (install):
  --no-plugin-build  Skip plugin build; use existing bundles in target/bundled/

OPTIONS (build-plugin, install-plugin):
  --debug         Build in debug mode instead of release

OPTIONS (bundle-plugin):
  --debug         Build in debug mode instead of release
  --no-build      Skip compilation; assemble bundles from pre-built dylibs

OPTIONS (install-plugin):
  --no-build      Skip the build step; install existing bundles

OPTIONS (package-plugin):
  --no-build      Skip the build step; package existing bundles

OPTIONS (run-turn):
  --port <PORT>   Listening port          (default: 3478)
  --user <U:P>    Username:password       (default: wail:wailpass)
  --min-port <N>  Relay port range start  (default: 49152)
  --max-port <N>  Relay port range end    (default: 49252)

EXAMPLES:
  cargo xtask install
  cargo xtask install --no-plugin-build
  cargo xtask build-plugin
  cargo xtask bundle-plugin
  cargo xtask install-plugin
  cargo xtask install-plugin --no-build
  cargo xtask package-plugin
  cargo xtask package-plugin --no-build
  cargo xtask run-tauri
  cargo xtask build-tauri
  cargo xtask test
  cargo xtask test -- -p wail-net --ignored
  cargo xtask run-turn
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
        Some("bundle-plugin") => {
            args.remove(0);
            bundle_plugin(&args)
        }
        Some("install-plugin") => {
            args.remove(0);
            install_plugin(&args)
        }
        Some("package-plugin") => {
            args.remove(0);
            package_plugin(&args)
        }
        Some("run-tauri") => {
            args.remove(0);
            run_tauri()
        }
        Some("build-tauri") => {
            args.remove(0);
            build_tauri()
        }
        Some("test") => {
            args.remove(0);
            run_test(&args)
        }
        Some("run-turn") => {
            args.remove(0);
            run_turn(&args)
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
    let profile = if release { "release" } else { "debug" };

    for plugin in &["wail-plugin-send", "wail-plugin-recv"] {
        println!("Building {plugin} ({profile})...");
        let mut cmd = Command::new("cargo");
        cmd.args(["nih-plug", "bundle", plugin]);
        if release {
            cmd.arg("--release");
        }
        cmd.current_dir(workspace_dir());
        run_cmd(cmd).with_context(|| format!("cargo nih-plug bundle {plugin} failed"))?;
    }

    println!("\nPlugin bundles:");
    println!("  target/bundled/wail-plugin-send.clap");
    println!("  target/bundled/wail-plugin-send.vst3");
    println!("  target/bundled/wail-plugin-recv.clap");
    println!("  target/bundled/wail-plugin-recv.vst3");
    println!("\nBuilt with profile: {profile}");
    Ok(())
}

/// Assemble CLAP and VST3 plugin bundles without requiring an external
/// `cargo-nih-plug` installation. Intended for CI and Homebrew formula builds.
///
/// On macOS each format is a bundle directory:
///   PluginName.clap/Contents/MacOS/PluginName + Info.plist
///   PluginName.vst3/Contents/MacOS/PluginName + Info.plist
///
/// On Linux, CLAP is a flat renamed .so; VST3 uses the x86_64-linux layout.
/// On Windows, CLAP is a flat renamed .dll; VST3 uses the x86_64-win layout.
fn bundle_plugin(args: &[String]) -> Result<()> {
    let release = !args.contains(&"--debug".to_string());
    let no_build = args.contains(&"--no-build".to_string());
    let profile = if release { "release" } else { "debug" };

    let root = workspace_dir();
    #[cfg(target_os = "macos")]
    let version = cargo_version(&root)?;

    // (cargo-package-name, lib-stem, display-name, bundle-id-prefix)
    let plugins: &[(&str, &str, &str, &str)] = &[
        (
            "wail-plugin-send",
            "wail_plugin_send",
            "WAIL Send",
            "com.wail.wail-plugin-send",
        ),
        (
            "wail-plugin-recv",
            "wail_plugin_recv",
            "WAIL Recv",
            "com.wail.wail-plugin-recv",
        ),
    ];

    #[allow(unused_variables)]
    for &(pkg, lib, display_name, bundle_id) in plugins {
        if no_build {
            println!("Bundling {pkg} (skipping build)...");
        } else {
            println!("Building {pkg} ({profile})...");
            let mut cmd = Command::new(env!("CARGO"));
            cmd.args(["build", "--package", pkg, "--lib", "--locked"]);
            if release {
                cmd.arg("--release");
            }
            cmd.current_dir(&root);
            run_cmd(cmd).with_context(|| format!("cargo build {pkg} failed"))?;
        }

        let out = root.join("target").join(profile);
        let bundled = root.join("target/bundled");
        fs::create_dir_all(&bundled)
            .with_context(|| format!("create bundled dir: {}", bundled.display()))?;

        #[cfg(target_os = "macos")]
        {
            let dylib = out.join(format!("lib{lib}.dylib"));
            if !dylib.exists() {
                bail!("dylib not found: {}", dylib.display());
            }
            for ext in ["clap", "vst3"] {
                let bundle = bundled.join(format!("{pkg}.{ext}"));
                let macos_dir = bundle.join("Contents/MacOS");
                if bundle.exists() {
                    if bundle.is_dir() {
                        fs::remove_dir_all(&bundle)?;
                    } else {
                        fs::remove_file(&bundle)?;
                    }
                }
                fs::create_dir_all(&macos_dir)
                    .with_context(|| format!("create MacOS dir in {pkg}.{ext}"))?;
                fs::copy(&dylib, macos_dir.join(pkg))
                    .with_context(|| format!("copy dylib into {pkg}.{ext}"))?;
                fs::write(
                    bundle.join("Contents/Info.plist"),
                    make_plist(pkg, display_name, bundle_id, ext, &version),
                )
                .with_context(|| format!("write Info.plist for {pkg}.{ext}"))?;
                println!("  Bundled: {}", bundle.display());
            }
        }

        #[cfg(target_os = "linux")]
        {
            let so = out.join(format!("lib{lib}.so"));
            if !so.exists() {
                bail!("shared library not found: {}", so.display());
            }
            // CLAP on Linux: flat renamed .so
            let clap = bundled.join(format!("{pkg}.clap"));
            if clap.exists() {
                fs::remove_file(&clap)?;
            }
            fs::copy(&so, &clap)?;
            println!("  Bundled: {}", clap.display());
            // VST3 on Linux: bundle with x86_64-linux sub-dir
            let vst3 = bundled.join(format!("{pkg}.vst3"));
            if vst3.exists() {
                fs::remove_dir_all(&vst3)?;
            }
            let vst3_dir = vst3.join("Contents/x86_64-linux");
            fs::create_dir_all(&vst3_dir)?;
            fs::copy(&so, vst3_dir.join(format!("{pkg}.so")))?;
            println!("  Bundled: {}", vst3.display());
        }

        #[cfg(target_os = "windows")]
        {
            let dll = out.join(format!("{lib}.dll"));
            if !dll.exists() {
                bail!("dll not found: {}", dll.display());
            }
            // CLAP on Windows: flat renamed .dll
            let clap = bundled.join(format!("{pkg}.clap"));
            if clap.exists() {
                fs::remove_file(&clap)?;
            }
            fs::copy(&dll, &clap)?;
            println!("  Bundled: {}", clap.display());
            // VST3 on Windows: bundle with x86_64-win sub-dir
            let vst3 = bundled.join(format!("{pkg}.vst3"));
            if vst3.exists() {
                fs::remove_dir_all(&vst3)?;
            }
            let vst3_dir = vst3.join("Contents/x86_64-win");
            fs::create_dir_all(&vst3_dir)?;
            fs::copy(&dll, vst3_dir.join(format!("{pkg}.vst3")))?;
            println!("  Bundled: {}", vst3.display());
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        bail!("bundle-plugin is not supported on this platform");
    }

    println!("\nPlugin bundles (no cargo-nih-plug required):");
    println!("  target/bundled/wail-plugin-send.clap");
    println!("  target/bundled/wail-plugin-send.vst3");
    println!("  target/bundled/wail-plugin-recv.clap");
    println!("  target/bundled/wail-plugin-recv.vst3");
    println!("Built with profile: {profile}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn make_plist(
    executable: &str,
    display_name: &str,
    bundle_id: &str,
    ext: &str,
    version: &str,
) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{executable}</string>
    <key>CFBundleIdentifier</key>
    <string>{bundle_id}.{ext}</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundleDisplayName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleShortVersionString</key>
    <string>{version}</string>
    <key>CFBundleVersion</key>
    <string>{version}</string>
    <key>NSHumanReadableCopyright</key>
    <string>MIT</string>
</dict>
</plist>
"#
    )
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
    let (clap_dir, vst3_dir) = plugin_dirs()?;
    fs::create_dir_all(&clap_dir)
        .with_context(|| format!("Could not create {}", clap_dir.display()))?;
    fs::create_dir_all(&vst3_dir)
        .with_context(|| format!("Could not create {}", vst3_dir.display()))?;

    for plugin in &["wail-plugin-send", "wail-plugin-recv"] {
        let clap_bundle = root.join(format!("target/bundled/{plugin}.clap"));
        let vst3_bundle = root.join(format!("target/bundled/{plugin}.vst3"));

        for path in [&clap_bundle, &vst3_bundle] {
            if !path.exists() {
                bail!(
                    "{} not found — run `cargo xtask build-plugin` first",
                    path.display()
                );
            }
        }

        copy_bundle(&clap_bundle, &clap_dir)?;
        copy_bundle(&vst3_bundle, &vst3_dir)?;
    }

    println!("\nDone. Rescan plugins in your DAW to pick up the changes.");
    Ok(())
}

fn package_plugin(args: &[String]) -> Result<()> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = args;
        println!("package-plugin is only supported on macOS (creates .pkg installer).");
        println!("On Linux, use `cargo xtask install-plugin` to install plugins to ~/.clap and ~/.vst3.");
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let no_build = args.contains(&"--no-build".to_string());
        if !no_build {
            build_plugin(&[])?;
        }

        let root = workspace_dir();
        let version = cargo_version(&root)?;

        let payload = root.join("target/pkg_payload");
        let clap_dest = payload.join("Library/Audio/Plug-Ins/CLAP");
        let vst3_dest = payload.join("Library/Audio/Plug-Ins/VST3");
        if payload.exists() {
            fs::remove_dir_all(&payload).context("Could not clean pkg_payload")?;
        }
        fs::create_dir_all(&clap_dest)?;
        fs::create_dir_all(&vst3_dest)?;

        for plugin in &["wail-plugin-send", "wail-plugin-recv"] {
            let clap_src = root.join(format!("target/bundled/{plugin}.clap"));
            let vst3_src = root.join(format!("target/bundled/{plugin}.vst3"));
            for path in [&clap_src, &vst3_src] {
                if !path.exists() {
                    bail!(
                        "{} not found — run `cargo xtask build-plugin` first",
                        path.display()
                    );
                }
            }
            copy_bundle(&clap_src, &clap_dest)?;
            copy_bundle(&vst3_src, &vst3_dest)?;
        }

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
        .find(|p| p["name"] == "wail-plugin-send")
        .and_then(|p| p["version"].as_str())
        .map(|s| s.to_owned())
        .context("Could not find wail-plugin-send version in cargo metadata")
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

    // Ensure opus.dll placeholder exists for Tauri resource bundling.
    // On Windows the real opus.dll should already be in the build environment;
    // on other platforms an empty placeholder prevents Tauri from erroring on
    // the missing resource mapping entry in tauri.conf.json.
    let opus_placeholder = workspace_dir().join("target/bundled/opus.dll");
    if !opus_placeholder.exists() {
        fs::write(&opus_placeholder, b"")?;
    }

    println!("\nBuilding WAIL Tauri app...");
    let mut cmd = Command::new("cargo");
    cmd.args(["tauri", "build", "-c", "crates/wail-tauri/tauri.conf.json"])
        .current_dir(workspace_dir());
    run_cmd(cmd)
}

/// Two-phase test runner: builds plugin bundles if missing, then runs `cargo test`.
/// This avoids the deadlock that occurs when `wail-plugin-test/build.rs` tries to
/// spawn a nested cargo process while the outer cargo holds the workspace lock.
///
/// All arguments after `--` are forwarded to `cargo test`.
fn run_test(args: &[String]) -> Result<()> {
    let root = workspace_dir();
    let recv_bundle = root.join("target/bundled/wail-plugin-recv.clap");
    let send_bundle = root.join("target/bundled/wail-plugin-send.clap");

    let bundle_valid = |p: &Path| {
        #[cfg(target_os = "macos")]
        return p.is_dir();
        #[cfg(not(target_os = "macos"))]
        return p.is_file() && p.metadata().map(|m| m.len() > 0).unwrap_or(false);
    };

    if !bundle_valid(&recv_bundle) || !bundle_valid(&send_bundle) {
        println!("Plugin bundles missing — building them first...");
        bundle_plugin(&["--debug".to_string()])?;
    }

    println!("\nRunning cargo test...");
    let mut cmd = Command::new(env!("CARGO"));
    cmd.arg("test");
    cmd.args(args);
    cmd.current_dir(&root);
    run_cmd(cmd)
}

fn run_turn(args: &[String]) -> Result<()> {
    // Parse optional flags
    let get_flag = |flag: &str, default: &str| -> String {
        args.windows(2)
            .find(|w| w[0] == flag)
            .map(|w| w[1].clone())
            .unwrap_or_else(|| default.to_string())
    };

    let port = get_flag("--port", "3478");
    let user = get_flag("--user", "wail:wailpass");
    let min_port = get_flag("--min-port", "49152");
    let max_port = get_flag("--max-port", "49252");
    let realm = "wail";

    // Detect local IP
    let local_ip = detect_local_ip().unwrap_or_else(|| "0.0.0.0".to_string());

    // Detect public IP
    println!("Detecting public IP...");
    let public_ip = detect_public_ip().unwrap_or_else(|| {
        eprintln!("Warning: Could not detect public IP. Using local IP.");
        local_ip.clone()
    });

    let username = user.split(':').next().unwrap_or("wail");
    let password = user.split(':').nth(1).unwrap_or("wailpass");

    println!("Local IP:  {local_ip}");
    println!("Public IP: {public_ip}");
    println!("TURN port: {port}");
    println!("Relay ports: {min_port}-{max_port}");
    println!("Credentials: {username}:{password}");
    println!();
    println!("Configure your WAIL client with:");
    println!("  TURN Server:   turn:{public_ip}:{port}");
    println!("  TURN Username: {username}");
    println!("  TURN Password: {password}");
    println!();
    println!("Make sure to forward ports {port} (TCP+UDP) and {min_port}-{max_port} (UDP) on your router.");
    println!();

    // Compute lt-cred-mech key: MD5(username:realm:password)
    let key = {
        use std::io::Write;
        let mut ctx = md5::Context::new();
        write!(ctx, "{username}:{realm}:{password}").unwrap();
        format!("0x{:x}", ctx.compute())
    };

    // Find turnserver binary
    let turnserver = which_turnserver()?;

    let mut cmd = Command::new(turnserver);
    cmd.arg("-n") // no config file
        .arg("--log-file=stdout")
        .arg("--verbose")
        .arg(format!("--listening-port={port}"))
        .arg(format!("--listening-ip={local_ip}"))
        .arg(format!("--external-ip={public_ip}/{local_ip}"))
        .arg(format!("--realm={realm}"))
        .arg(format!("--user={username}:{key}"))
        .arg("--lt-cred-mech")
        .arg("--no-tls")
        .arg("--no-dtls")
        .arg(format!("--min-port={min_port}"))
        .arg(format!("--max-port={max_port}"));

    println!("Starting coturn TURN server...\n");
    run_cmd(cmd)
}

fn which_turnserver() -> Result<String> {
    // Check common locations
    for path in &[
        "/opt/homebrew/opt/coturn/bin/turnserver",
        "/opt/homebrew/bin/turnserver",
        "/usr/local/bin/turnserver",
        "/usr/bin/turnserver",
    ] {
        if Path::new(path).exists() {
            return Ok(path.to_string());
        }
    }
    // Try PATH
    let output = Command::new("which")
        .arg("turnserver")
        .output()
        .context("Could not locate turnserver")?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    #[cfg(target_os = "macos")]
    bail!("coturn not found. Install with: brew install coturn");
    #[cfg(target_os = "linux")]
    bail!("coturn not found. Install with: sudo apt install coturn");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("coturn not found. See https://github.com/coturn/coturn for installation instructions.")
}

fn detect_local_ip() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        // Try en0 first (Wi-Fi on macOS), then en1
        for iface in &["en0", "en1"] {
            let output = Command::new("ipconfig")
                .args(["getifaddr", iface])
                .output()
                .ok()?;
            if output.status.success() {
                let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !ip.is_empty() {
                    return Some(ip);
                }
            }
        }
        None
    }

    #[cfg(target_os = "linux")]
    {
        // hostname -I returns space-separated non-loopback IPs
        let output = Command::new("hostname").arg("-I").output().ok()?;
        if output.status.success() {
            let ips = String::from_utf8_lossy(&output.stdout);
            if let Some(ip) = ips.split_whitespace().next() {
                if !ip.is_empty() {
                    return Some(ip.to_string());
                }
            }
        }
        None
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

fn detect_public_ip() -> Option<String> {
    // Try IPv4 first
    let output = Command::new("curl")
        .args(["-s", "-4", "--max-time", "5", "https://api.ipify.org"])
        .output()
        .ok()?;
    if output.status.success() {
        let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !ip.is_empty() && !ip.contains(':') {
            return Some(ip);
        }
    }
    None
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

    println!("\n=== WAIL plugins installed successfully ===");
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
