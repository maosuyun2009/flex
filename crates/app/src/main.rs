//! rexshell: a Windows shell for Rex servers.
//!
//! Usage: `rexshell [SERVER]`, or set `REXSHELL_SERVER`. Without either, the
//! shell attaches to a local Rex server if one is running, and otherwise shows
//! whatever server list it saved last time.

mod keymap;
mod paint;
mod terminal;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{px, size, App, AppContext as _, Application, Bounds, WindowBounds, WindowOptions};
use rexshell_engine::Engine;

fn main() {
    let address = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("REXSHELL_SERVER").ok());

    Application::new().run(move |cx: &mut App| {
        // The engine calls this from its own threads; the UI wakes on its loop.
        let (wake_tx, wake_rx) = async_channel::unbounded::<()>();
        let engine = Arc::new(Engine::new(
            data_dir(),
            "rexshell",
            Arc::new(move || {
                let _ = wake_tx.try_send(());
            }),
        ));

        match &address {
            Some(address) => {
                if let Err(e) = engine.add_server("", address) {
                    eprintln!("rexshell: {address}: {e}");
                }
            }
            None => {
                engine.add_local_server_if_present();
            }
        }

        let bounds = Bounds::centered(None, size(px(1280.0), px(760.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        let rx = wake_rx.clone();
        cx.open_window(options, move |_window, cx| {
            cx.new(|cx| workspace::Workspace::new(engine.clone(), rx.clone(), cx))
        })
        .expect("open window");
        cx.activate(true);
    });
}

/// Where the saved server list lives. `None` would keep it in memory only.
fn data_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("rexshell"))?;
    Some(dir)
}
