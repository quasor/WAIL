mod commands;
pub mod events;
mod filelog;
mod hb;
mod identity;
mod peers;
mod plugin_install;
mod recorder;
mod session;
mod stream_names;
pub mod wslog;

use std::sync::Mutex;

use commands::SessionState;
use events::LogEntry;
use tauri::{Emitter, Manager};
use tracing_subscriber::prelude::*;

pub struct PluginInstallErrors(pub Mutex<Vec<String>>);

/// CLI arguments for test mode auto-join.
pub struct TestModeArgs {
    pub room: String,
    pub bpm: f64,
    pub display_name: Option<String>,
}

/// Emit a warning log to the frontend.
pub fn emit_log(app: &tauri::AppHandle, level: &str, message: String) {
    let _ = app.emit("log:entry", LogEntry {
        level: level.to_string(),
        message,
        peer_id: None,
        peer_name: None,
    });
}

pub fn emit_peer_log(
    app: &tauri::AppHandle,
    peer_id: &str,
    peer_name: Option<String>,
    level: &str,
    message: String,
) {
    let _ = app.emit("log:entry", LogEntry {
        level: level.to_string(),
        message,
        peer_id: Some(peer_id.to_string()),
        peer_name,
    });
}

pub fn run(test_args: Option<TestModeArgs>) {
    hb::init();
    hb::set_panic_hook();

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "wail=info,wail_tauri=info,wail_core=info,wail_net=info".into()
    });

    let fmt_layer = tracing_subscriber::fmt::layer().with_filter(env_filter);
    let (file_layer, telemetry_handle) = filelog::FileLogLayer::new();
    let (ws_log_layer, ws_log_handle) = wslog::new();

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(file_layer)
        .with(ws_log_layer)
        .init();

    let th = telemetry_handle.clone();
    tauri::Builder::default()
        .manage(SessionState::default())
        .manage(PluginInstallErrors(Mutex::new(Vec::new())))
        .manage(telemetry_handle)
        .manage(ws_log_handle)
        .setup(move |app| {
            let data_dir = app.path().app_data_dir()?;
            if let Err(e) = th.set_log_dir(&data_dir.join("logs")) {
                eprintln!("[filelog] failed to open log file: {e}");
            }
            let peer_identity = identity::get_or_create(&data_dir);
            app.manage(identity::PeerIdentity(peer_identity.clone()));

            let stream_names = stream_names::load(&data_dir);
            app.manage(stream_names::StreamNameConfig {
                data_dir: data_dir.clone(),
                names: std::sync::Mutex::new(stream_names),
            });
            // On Windows, plugin installation is handled by the NSIS setup.exe installer
            // (which runs elevated), so we skip runtime auto-install to avoid permission errors.
            #[cfg(not(target_os = "windows"))]
            let install_errors = {
                let resource_dir = app.path().resource_dir().ok();
                match plugin_install::find_plugin_dir(resource_dir.as_deref()) {
                    Some(plugin_dir) => plugin_install::install_if_missing(&plugin_dir),
                    None => {
                        if cfg!(debug_assertions) {
                            tracing::debug!("plugin_install: dev mode, skipping auto-install");
                            vec![]
                        } else {
                            tracing::warn!("plugin_install: no bundled plugins found");
                            vec!["Could not locate bundled plugins. Run wail-install-plugins to install manually.".to_string()]
                        }
                    }
                }
            };
            #[cfg(target_os = "windows")]
            let install_errors: Vec<String> = vec![];
            if !install_errors.is_empty() {
                if let Ok(mut state) = app.state::<PluginInstallErrors>().0.lock() {
                    *state = install_errors;
                }
            }

            // Auto-join test room if CLI args provided
            if let Some(ref test) = test_args {
                let random_suffix = &uuid::Uuid::new_v4().to_string()[..6];
                let display_name = test.display_name.clone()
                    .unwrap_or_else(|| format!("test-{random_suffix}"));
                let bpm = test.bpm;
                let room = test.room.clone();

                tracing::info!(
                    room = %room,
                    bpm = bpm,
                    display_name = %display_name,
                    "Auto-joining test room from CLI args"
                );

                let config = session::SessionConfig {
                    server: "wss://wail-signal.fly.dev".to_string(),
                    room: room.clone(),
                    password: None,
                    display_name,
                    identity: peer_identity,
                    bpm,
                    bars: 4,
                    quantum: 4.0,
                    ipc_port: 9191,
                    recording: None,
                    stream_count: 1,
                    test_mode: true,
                };

                match session::spawn_session(app.handle().clone(), config) {
                    Ok(handle) => {
                        let state = app.state::<SessionState>();
                        let _ = state.lock().map(|mut s| *s = Some(handle));
                    }
                    Err(e) => {
                        tracing::error!("Failed to spawn test session: {e}");
                    }
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::join_room,
            commands::disconnect,
            commands::change_bpm,
            commands::send_chat,
            commands::set_telemetry,
            commands::set_log_sharing,
            commands::list_public_rooms,
            commands::get_default_recording_dir,
            commands::cleanup_recordings,
            commands::get_active_session,
            commands::get_plugin_install_errors,
            commands::rename_stream,
        ])
        .run(tauri::generate_context!())
        .expect("error while running WAIL");
}
