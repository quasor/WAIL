use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{Manager, State};
use tracing::{info, warn};

use crate::loki::TelemetryHandle;
use crate::recorder::RecordingConfig;
use crate::session::{SessionCommand, SessionConfig, SessionHandle};

pub type SessionState = Mutex<Option<SessionHandle>>;

const SIGNALING_URL: &str = "https://wail.val.run/";

#[derive(Debug, Serialize, Deserialize)]
pub struct JoinResult {
    pub peer_id: String,
    pub room: String,
    pub bpm: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PublicRoomInfo {
    pub room: String,
    pub peer_count: u32,
    pub bpm: Option<f64>,
    pub display_names: Vec<String>,
    pub created_at: i64,
}

#[tauri::command]
pub async fn list_public_rooms() -> Result<Vec<PublicRoomInfo>, String> {
    let rooms = wail_net::signaling::list_public_rooms(SIGNALING_URL)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rooms
        .into_iter()
        .map(|r| PublicRoomInfo {
            room: r.room,
            peer_count: r.peer_count,
            bpm: r.bpm,
            display_names: r.display_names,
            created_at: r.created_at,
        })
        .collect())
}

#[tauri::command]
pub fn join_room(
    app: tauri::AppHandle,
    state: State<'_, SessionState>,
    room: String,
    password: Option<String>,
    display_name: String,
    bpm: Option<f64>,
    bars: Option<u32>,
    quantum: Option<f64>,
    ipc_port: Option<u16>,
    test_tone: Option<bool>,
    recording_enabled: Option<bool>,
    recording_directory: Option<String>,
    recording_stems: Option<bool>,
    recording_retention_days: Option<u32>,
) -> Result<JoinResult, String> {
    let mut session = state.lock().map_err(|e| e.to_string())?;
    if session.is_some() {
        return Err("Already in a session. Disconnect first.".into());
    }

    let bpm = bpm.unwrap_or(120.0);
    let config = SessionConfig {
        server: SIGNALING_URL.to_string(),
        room,
        password,
        display_name,
        bpm,
        bars: bars.unwrap_or(4),
        quantum: quantum.unwrap_or(4.0),
        ipc_port: ipc_port.unwrap_or(9191),
        test_tone: test_tone.unwrap_or(false),
        recording: if recording_enabled.unwrap_or(false) {
            Some(RecordingConfig {
                enabled: true,
                directory: recording_directory
                    .unwrap_or_else(|| crate::recorder::default_recording_dir().unwrap_or_default()),
                stems: recording_stems.unwrap_or(false),
                retention_days: recording_retention_days.unwrap_or(30),
            })
        } else {
            None
        },
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
pub fn set_telemetry(handle: State<'_, TelemetryHandle>, enabled: bool) -> Result<(), String> {
    handle.set_enabled(enabled);
    info!(enabled, "Telemetry toggled");
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
    pub send_clap: bool,
    pub send_vst3: bool,
    pub recv_clap: bool,
    pub recv_vst3: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PluginPaths {
    pub send_clap_path: String,
    pub send_vst3_path: String,
    pub recv_clap_path: String,
    pub recv_vst3_path: String,
}

const PLUGIN_NAMES: [&str; 2] = ["wail-plugin-send", "wail-plugin-recv"];

#[tauri::command]
pub fn check_plugins_installed() -> Result<PluginStatus, String> {
    let (clap_dir, vst3_dir) = plugin_dirs().map_err(|e| e.to_string())?;
    Ok(PluginStatus {
        send_clap: clap_dir.join("wail-plugin-send.clap").exists(),
        send_vst3: vst3_dir.join("wail-plugin-send.vst3").exists(),
        recv_clap: clap_dir.join("wail-plugin-recv.clap").exists(),
        recv_vst3: vst3_dir.join("wail-plugin-recv.vst3").exists(),
    })
}

#[tauri::command]
pub fn install_plugins(app: tauri::AppHandle) -> Result<PluginPaths, String> {
    let resource_path = app
        .path()
        .resource_dir()
        .map_err(|e| format!("Cannot find resource dir: {e}"))?;

    let (clap_dir, vst3_dir) = plugin_dirs().map_err(|e| e.to_string())?;

    std::fs::create_dir_all(&clap_dir)
        .map_err(|e| format!("Could not create {}: {e}", clap_dir.display()))?;
    std::fs::create_dir_all(&vst3_dir)
        .map_err(|e| format!("Could not create {}: {e}", vst3_dir.display()))?;

    for plugin in &PLUGIN_NAMES {
        let clap_src = resource_path.join(format!("plugins/{plugin}.clap"));
        let vst3_src = resource_path.join(format!("plugins/{plugin}.vst3"));

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

        copy_bundle(&clap_src, &clap_dir).map_err(|e| e.to_string())?;
        copy_bundle(&vst3_src, &vst3_dir).map_err(|e| e.to_string())?;
    }

    Ok(PluginPaths {
        send_clap_path: clap_dir.join("wail-plugin-send.clap").to_string_lossy().into(),
        send_vst3_path: vst3_dir.join("wail-plugin-send.vst3").to_string_lossy().into(),
        recv_clap_path: clap_dir.join("wail-plugin-recv.clap").to_string_lossy().into(),
        recv_vst3_path: vst3_dir.join("wail-plugin-recv.vst3").to_string_lossy().into(),
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

#[tauri::command]
pub fn get_default_recording_dir() -> Result<String, String> {
    crate::recorder::default_recording_dir().map_err(|e| e.to_string())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CleanupResult {
    pub deleted_count: u32,
    pub freed_bytes: u64,
}

#[tauri::command]
pub async fn cleanup_recordings(directory: String, retention_days: u32) -> Result<CleanupResult, String> {
    tokio::task::spawn_blocking(move || {
        let (deleted_count, freed_bytes) =
            crate::recorder::cleanup_old_sessions(std::path::Path::new(&directory), retention_days)
                .map_err(|e| e.to_string())?;
        Ok(CleanupResult { deleted_count, freed_bytes })
    })
    .await
    .map_err(|e| e.to_string())?
}
