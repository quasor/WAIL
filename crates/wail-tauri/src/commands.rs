use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{Manager, State};
use tracing::{info, warn};

use crate::session::{SessionCommand, SessionConfig, SessionHandle};

pub type SessionState = Mutex<Option<SessionHandle>>;

#[derive(Debug, Serialize, Deserialize)]
pub struct JoinResult {
    pub peer_id: String,
    pub room: String,
    pub bpm: f64,
}

#[tauri::command]
pub fn join_room(
    app: tauri::AppHandle,
    state: State<'_, SessionState>,
    server: String,
    room: String,
    password: String,
    display_name: Option<String>,
    bpm: Option<f64>,
    bars: Option<u32>,
    quantum: Option<f64>,
    ipc_port: Option<u16>,
    test_tone: Option<bool>,
    turn_url: Option<String>,
    turn_username: Option<String>,
    turn_credential: Option<String>,
) -> Result<JoinResult, String> {
    let mut session = state.lock().map_err(|e| e.to_string())?;
    if session.is_some() {
        return Err("Already in a session. Disconnect first.".into());
    }

    let bpm = bpm.unwrap_or(120.0);
    let config = SessionConfig {
        server,
        room,
        password,
        display_name,
        bpm,
        bars: bars.unwrap_or(4),
        quantum: quantum.unwrap_or(4.0),
        ipc_port: ipc_port.unwrap_or(9191),
        test_tone: test_tone.unwrap_or(false),
        turn_url,
        turn_username,
        turn_credential,
    };

    let handle = crate::session::spawn_session(app, config).map_err(|e| e.to_string())?;
    let result = JoinResult {
        peer_id: handle.peer_id.clone(),
        room: handle.room.clone(),
        bpm,
    };
    *session = Some(handle);
    Ok(result)
}

#[tauri::command]
pub fn disconnect(state: State<'_, SessionState>) -> Result<(), String> {
    let mut session = state.lock().map_err(|e| e.to_string())?;
    if let Some(handle) = session.take() {
        let _ = handle.cmd_tx.send(SessionCommand::Disconnect);
        info!("Disconnect command sent");
    }
    Ok(())
}

#[tauri::command]
pub fn change_bpm(state: State<'_, SessionState>, bpm: f64) -> Result<(), String> {
    let session = state.lock().map_err(|e| e.to_string())?;
    if let Some(ref handle) = *session {
        let _ = handle.cmd_tx.send(SessionCommand::ChangeBpm(bpm));
    } else {
        warn!("No active session for BPM change");
    }
    Ok(())
}

#[tauri::command]
pub fn set_test_tone(state: State<'_, SessionState>, enabled: bool) -> Result<(), String> {
    let session = state.lock().map_err(|e| e.to_string())?;
    if let Some(ref handle) = *session {
        let _ = handle.cmd_tx.send(SessionCommand::SetTestTone(enabled));
    } else {
        warn!("No active session for test tone toggle");
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PluginStatus {
    pub clap: bool,
    pub vst3: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PluginPaths {
    pub clap_path: String,
    pub vst3_path: String,
}

#[tauri::command]
pub fn check_plugins_installed() -> Result<PluginStatus, String> {
    let (clap_dir, vst3_dir) = plugin_dirs().map_err(|e| e.to_string())?;
    Ok(PluginStatus {
        clap: clap_dir.join("wail-plugin.clap").exists(),
        vst3: vst3_dir.join("wail-plugin.vst3").exists(),
    })
}

#[tauri::command]
pub fn install_plugins(app: tauri::AppHandle) -> Result<PluginPaths, String> {
    let resource_path = app
        .path()
        .resource_dir()
        .map_err(|e| format!("Cannot find resource dir: {e}"))?;

    let clap_src = resource_path.join("plugins/wail-plugin.clap");
    let vst3_src = resource_path.join("plugins/wail-plugin.vst3");

    if !clap_src.exists() {
        return Err(format!(
            "CLAP plugin not found in app bundle at {}",
            clap_src.display()
        ));
    }
    if !vst3_src.exists() {
        return Err(format!(
            "VST3 plugin not found in app bundle at {}",
            vst3_src.display()
        ));
    }

    let (clap_dir, vst3_dir) = plugin_dirs().map_err(|e| e.to_string())?;

    std::fs::create_dir_all(&clap_dir)
        .map_err(|e| format!("Could not create {}: {e}", clap_dir.display()))?;
    std::fs::create_dir_all(&vst3_dir)
        .map_err(|e| format!("Could not create {}: {e}", vst3_dir.display()))?;

    copy_bundle(&clap_src, &clap_dir).map_err(|e| e.to_string())?;
    copy_bundle(&vst3_src, &vst3_dir).map_err(|e| e.to_string())?;

    Ok(PluginPaths {
        clap_path: clap_dir.join("wail-plugin.clap").to_string_lossy().into(),
        vst3_path: vst3_dir.join("wail-plugin.vst3").to_string_lossy().into(),
    })
}

fn plugin_dirs() -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    #[cfg(target_os = "macos")]
    {
        let home = home_dir()?;
        let base = home.join("Library/Audio/Plug-Ins");
        Ok((base.join("CLAP"), base.join("VST3")))
    }
    #[cfg(target_os = "linux")]
    {
        let home = home_dir()?;
        Ok((home.join(".clap"), home.join(".vst3")))
    }
    #[cfg(target_os = "windows")]
    {
        let common = std::path::PathBuf::from(
            std::env::var("COMMONPROGRAMFILES")
                .map_err(|_| anyhow::anyhow!("COMMONPROGRAMFILES not set"))?,
        );
        Ok((common.join("CLAP"), common.join("VST3")))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("Unsupported platform")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn home_dir() -> anyhow::Result<std::path::PathBuf> {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| anyhow::anyhow!("HOME environment variable not set"))
}

fn copy_bundle(src: &std::path::Path, dest_dir: &std::path::Path) -> anyhow::Result<()> {
    let dest = dest_dir.join(src.file_name().unwrap());
    if src.is_dir() {
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        copy_dir_all(src, &dest)?;
    } else {
        std::fs::copy(src, &dest)?;
    }
    info!(path = %dest.display(), "Plugin installed");
    Ok(())
}

fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}
