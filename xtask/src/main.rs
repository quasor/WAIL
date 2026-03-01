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
  build-plugin    Build the CLAP and VST3 plugin bundles
  install-plugin  Build (optional) and install to system plugin directories
  run-signaling   Start the WAIL signaling server on :9090
  run-peer        Start a WAIL peer and join a room

OPTIONS (build-plugin, install-plugin):
  --debug         Build in debug mode instead of release

OPTIONS (install-plugin):
  --no-build      Skip the build step; install existing bundles

OPTIONS (run-peer): all flags are forwarded to `wail-app join`
  --room <NAME>   Room to join          (default: test)
  --bpm  <BPM>    Initial tempo         (default: 120)
  --ipc-port <N>  IPC port for plugin   (default: 9191)
  --server <URL>  Signaling server URL  (default: ws://localhost:9090)
  --bars <N>      Bars per interval     (default: 4)
  --quantum <F>   Quantum               (default: 4.0)

EXAMPLES:
  cargo xtask build-plugin
  cargo xtask install-plugin
  cargo xtask install-plugin --no-build
  cargo xtask run-signaling
  cargo xtask run-peer
  cargo xtask run-peer --bpm 96 --ipc-port 9192
";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let mut args: Vec<String> = env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("build-plugin") => {
            args.remove(0);
            build_plugin(&args)
        }
        Some("install-plugin") => {
            args.remove(0);
            install_plugin(&args)
        }
        Some("run-signaling") => run_signaling(),
        Some("run-peer") => {
            args.remove(0);
            run_peer(&args)
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

fn run_signaling() -> Result<()> {
    println!("Starting WAIL signaling server on :9090 ...");
    let mut cmd = Command::new("cargo");
    cmd.args(["run", "-p", "wail-signaling"])
        .env(
            "RUST_LOG",
            env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        )
        .current_dir(workspace_dir());
    run_cmd(cmd)
}

fn run_peer(extra_args: &[String]) -> Result<()> {
    // Build up the default join args, then let extra_args override/extend them.
    // wail-app's clap parser accepts duplicate flags and uses the last value,
    // so passing defaults first and user args after achieves natural overriding.
    let defaults = [
        "--room", "test",
        "--server", "ws://localhost:9090",
        "--bpm", "120",
        "--bars", "4",
        "--quantum", "4",
        "--ipc-port", "9191",
    ];

    println!("Starting WAIL peer...");
    let mut cmd = Command::new("cargo");
    cmd.args(["run", "-p", "wail-app", "--", "join"])
        .args(defaults)
        .args(extra_args)
        .env(
            "RUST_LOG",
            env::var("RUST_LOG")
                .unwrap_or_else(|_| "wail_app=info,wail_core=info,wail_net=info".into()),
        )
        .current_dir(workspace_dir());
    run_cmd(cmd)
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
