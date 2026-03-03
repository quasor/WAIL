mod commands;
pub mod events;
mod hb;
mod recorder;
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
    hb::init();
    hb::set_panic_hook();

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
            commands::set_test_tone,
            commands::install_plugins,
            commands::check_plugins_installed,
            commands::list_public_rooms,
            commands::get_default_recording_dir,
            commands::cleanup_recordings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running WAIL");
}
