use std::sync::OnceLock;

use honeybadger::{ConfigBuilder, Honeybadger};

const API_KEY: &str = "hbp_0GYAg4zTkkp5dnhFf4k3Ke9rvmfvA62vKf8O";

static HB_INIT: OnceLock<()> = OnceLock::new();

pub fn init() {
    HB_INIT.get_or_init(|| ());
}

fn make_client() -> Option<Honeybadger> {
    if HB_INIT.get().is_none() {
        return None;
    }
    let config = ConfigBuilder::new(API_KEY).with_env("production").build();
    Honeybadger::new(config).ok()
}

fn make_notice(message: &str) -> honeybadger::notice::Error {
    honeybadger::notice::Error {
        class: message.to_string(),
        message: Some(message.to_string()),
        causes: None,
    }
}

/// Drive a honeybadger notification to completion on a dedicated tokio 0.1 runtime.
/// honeybadger's hyper 0.12 client requires tokio 0.1's reactor for I/O.
fn notify_blocking(hb: Honeybadger, notice: honeybadger::notice::Error) {
    if let Ok(mut rt) = tokio01::runtime::Runtime::new() {
        let _ = rt.block_on(hb.notify(notice, None));
    }
}

/// Report an error string to Honeybadger from an async context.
pub async fn report(message: &str) {
    let Some(hb) = make_client() else { return };
    let notice = make_notice(message);
    let _ = tokio::task::spawn_blocking(move || notify_blocking(hb, notice)).await;
}

/// Install a global panic hook that fires Honeybadger synchronously before unwinding.
pub fn set_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        let Some(hb) = make_client() else { return };
        notify_blocking(hb, make_notice(&msg));
    }));
}
