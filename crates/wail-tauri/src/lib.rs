mod commands;
pub mod events;
mod filelog;
mod hb;
mod identity;
mod peers;
mod plugin_install;
mod recorder;
mod session;

use std::sync::Mutex;

use commands::SessionState;
use events::LogEntry;
use tauri::{Emitter, Manager};
use tracing_subscriber::prelude::*;

pub struct PluginInstallErrors(pub Mutex<Vec<String>>);

/// Emit a warning log to the frontend.
pub fn emit_log(app: &tauri::AppHandle, level: &str, message: String) {
    let _ = app.emit("log:entry", LogEntry {
        level: level.to_string(),
        message,
    });
}

pub fn run() {
    hb::init();
    hb::set_panic_hook();

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "wail=info,wail_tauri=info,wail_core=info,wail_net=info".into()
    });

    let fmt_layer = tracing_subscriber::fmt::layer().with_filter(env_filter);
    let (file_layer, telemetry_handle) = filelog::FileLogLayer::new();

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(file_layer)
        .init();

    let th = telemetry_handle.clone();
    tauri::Builder::default()
        .manage(SessionState::default())
        .manage(PluginInstallErrors(Mutex::new(Vec::new())))
        .manage(telemetry_handle)
        .setup(move |app| {
            let data_dir = app.path().app_data_dir()?;
            if let Err(e) = th.set_log_dir(&data_dir.join("logs")) {
                eprintln!("[filelog] failed to open log file: {e}");
            }
            let peer_identity = identity::get_or_create(&data_dir);
            app.manage(identity::PeerIdentity(peer_identity));
            let install_errors = match app.path().resource_dir() {
                Ok(resource_dir) => plugin_install::install_if_missing(&resource_dir),
                Err(e) => {
                    tracing::warn!("plugin_install: could not resolve resource directory, skipping auto-install: {e}");
                    vec![format!("Could not locate bundled plugins (resource directory unavailable: {e}). Please install WAIL Send and WAIL Recv manually using cargo xtask install-plugin.")]
                }
            };
            if !install_errors.is_empty() {
                if let Ok(mut state) = app.state::<PluginInstallErrors>().0.lock() {
                    *state = install_errors;
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::join_room,
            commands::disconnect,
            commands::change_bpm,
            commands::set_test_tone,
            commands::set_telemetry,
            commands::list_public_rooms,
            commands::get_default_recording_dir,
            commands::cleanup_recordings,
            commands::get_plugin_install_errors,
        ])
        .run(tauri::generate_context!())
        .expect("error while running WAIL");
}
