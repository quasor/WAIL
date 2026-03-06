use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::State;
use tracing::{info, warn};

use crate::identity::PeerIdentity;
use crate::filelog::TelemetryHandle;
use crate::recorder::RecordingConfig;
use crate::session::{SessionCommand, SessionConfig, SessionHandle};
use crate::PluginInstallErrors;

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
    identity: State<'_, PeerIdentity>,
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
    stream_count: Option<u16>,
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
        identity: identity.0.clone(),
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
        stream_count: stream_count.unwrap_or(1),
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

#[tauri::command]
pub fn get_plugin_install_errors(state: State<'_, PluginInstallErrors>) -> Vec<String> {
    state.0.lock().map(|e| e.clone()).unwrap_or_default()
}
