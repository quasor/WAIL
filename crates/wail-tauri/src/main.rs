#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;

#[derive(Parser)]
#[command(name = "wail", about = "WAIL — WebRTC Audio Interchange for Link")]
struct Args {
    /// Join a room in test mode immediately on launch (generates synthetic audio, no DAW needed)
    #[arg(long)]
    test_room: Option<String>,

    /// BPM for test mode session
    #[arg(long, default_value = "120")]
    test_bpm: f64,

    /// Display name for test mode session
    #[arg(long)]
    test_name: Option<String>,

    /// Run as instance N (port = 9191+N, separate data dir). Allows multiple instances on one host.
    #[arg(long, default_value = "0")]
    instance: u16,
}

fn main() {
    let args = Args::parse();
    let test_args = if args.test_room.is_some() {
        Some(wail_tauri::TestModeArgs {
            room: args.test_room.unwrap(),
            bpm: args.test_bpm,
            display_name: args.test_name,
        })
    } else {
        None
    };
    wail_tauri::run(test_args, args.instance);
}
