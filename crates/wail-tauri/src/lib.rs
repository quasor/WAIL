mod commands;
pub mod events;
mod session;

use commands::SessionState;
use events::LogEntry;
use tauri::Emitter;

/// Emit a warning log to the frontend.
pub fn emit_log(app: &tauri::AppHandle, level: &str, message: String) {
    let _ = app.emit("log:entry", LogEntry {
        level: level.to_string(),
        message,
    });
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "wail=info,wail_tauri=info,wail_core=info,wail_net=info".into()
            }),
        )
        .init();

    tauri::Builder::default()
        .manage(SessionState::default())
        .invoke_handler(tauri::generate_handler![
            commands::join_room,
            commands::disconnect,
            commands::change_bpm,
            commands::install_plugins,
            commands::check_plugins_installed,
        ])
        .run(tauri::generate_context!())
        .expect("error while running WAIL");
}
