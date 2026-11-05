//! flex: a Windows shell for Rex servers.
//!
//! Usage: `flex [SERVER]`, or set `FLEX_SERVER`. Without either, the
//! shell attaches to a local Rex server if one is running, and otherwise shows
//! whatever server list it saved last time.

mod background;
mod fonts;
mod icons;
mod keymap;
mod paint;
mod settings;
mod shortcuts;
mod terminal;
mod theme;
mod win32;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use flex_engine::Engine;
use gpui::{px, size, App, AppContext as _, Application, Bounds, WindowBounds, WindowOptions};

fn main() {
    let address = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("FLEX_SERVER").ok());

    Application::new().run(move |cx: &mut App| {
        // The engine calls this from its own threads; the UI wakes on its loop.
        let (wake_tx, wake_rx) = async_channel::unbounded::<()>();
        let engine = Arc::new(Engine::new(
            data_dir(),
            "flex",
            Arc::new(move || {
                let _ = wake_tx.try_send(());
            }),
        ));

        match &address {
            Some(address) => {
                // Addresses are saved between runs; adding one again would leave
                // a duplicate entry, and each duplicate is another connection.
                let saved = engine
                    .state()
                    .servers
                    .iter()
                    .any(|s| s.address.trim() == address.trim());
                if !saved {
                    if let Err(e) = engine.add_server("", address) {
                        eprintln!("flex: {address}: {e}");
                    }
                }
            }
            None => {
                engine.add_local_server_if_present();
            }
        }

        // We draw the title bar ourselves, so the system one is turned off; the
        // shell then gets the whole window to paint and its own min/max/close.
        // A fixed logical size overflows a scaled display: at 150% a 1400pt
        // window is 2100px, wider than the screen, and centering then pushes the
        // title bar off the top. Take a fraction of the display instead.
        let (aw, ah) = cx
            .primary_display()
            .map(|d| {
                let s = d.bounds().size;
                (f32::from(s.width), f32::from(s.height))
            })
            .unwrap_or((1920.0, 1080.0));
        let bounds = Bounds::centered(
            None,
            size(
                px((aw * 0.86).clamp(720.0, 1500.0)),
                px((ah * 0.88).clamp(460.0, 1000.0)),
            ),
            cx,
        );
        let options = WindowOptions {
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("flex".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(720.0), px(460.0))),
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
        .map(|d| d.join("flex"))?;
    // The shell used to be called rexshell; keep the server list it saved.
    let old = dir.with_file_name("rexshell").join("shrew.json");
    let new = dir.join("shrew.json");
    if !new.exists() && old.exists() {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::copy(&old, &new);
    }
    Some(dir)
}
