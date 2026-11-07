//! Lantern's desktop window: the daemon's control center in a native window, with a tray icon.
//!
//! The page itself is served by the daemon, so this app holds no UI code of its own. Links to
//! your tools (Open buttons, target=_blank) go to the default browser; only the control
//! center stays in the window. Closing the window hides it to the tray; Quit leaves the daemon
//! and every tool running. `--hidden` starts in the tray (for login autostart).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    net::{SocketAddr, TcpStream},
    time::Duration,
};
use tauri::{
    AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent,
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::NewWindowResponse,
};

const DEFAULT_DAEMON: &str = "http://127.0.0.1:4778";

fn daemon_url() -> Url {
    std::env::var("LANTERN_DAEMON_URL")
        .ok()
        .and_then(|u| Url::parse(&u).ok())
        .unwrap_or_else(|| Url::parse(DEFAULT_DAEMON).expect("default URL"))
}

fn daemon_up(url: &Url) -> bool {
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return false;
    };
    format!("{host}:{port}")
        .parse::<SocketAddr>()
        .is_ok_and(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok())
}

/// True for the control center itself (and the offline page); everything else is a tool.
fn is_ours(url: &Url, daemon: &Url) -> bool {
    url.origin() == daemon.origin()
        || matches!(url.scheme(), "tauri" | "about" | "data")
        || url.host_str() == Some("tauri.localhost")
}

fn show(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn main() {
    let hidden = std::env::args().any(|a| a == "--hidden");
    let daemon = daemon_url();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show(app)))
        .setup(move |app| {
            // Start on the page if the daemon answers, else on a local page that waits for it.
            let start = if daemon_up(&daemon) {
                WebviewUrl::External(daemon.clone())
            } else {
                WebviewUrl::App("index.html".into())
            };
            let nav_daemon = daemon.clone();
            let window = WebviewWindowBuilder::new(app, "main", start)
                .title("Lantern")
                .inner_size(1240.0, 840.0)
                .min_inner_size(720.0, 480.0)
                .visible(!hidden)
                .initialization_script(&format!(
                    "window.__LANTERN_DAEMON__ = {};",
                    serde_json_string(daemon.as_str())
                ))
                .on_navigation(move |url| {
                    if is_ours(url, &nav_daemon) {
                        true
                    } else {
                        let _ = open::that_detached(url.as_str());
                        false
                    }
                })
                .on_new_window(|url, _features| {
                    let _ = open::that_detached(url.as_str());
                    NewWindowResponse::Deny
                })
                .build()?;

            // Close hides to the tray; Quit in the tray menu really exits.
            let w = window.clone();
            window.on_window_event(move |event| {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = w.hide();
                }
            });

            let open_item = MenuItem::with_id(app, "open", "Open Lantern", true, None::<&str>)?;
            let browser_item = MenuItem::with_id(app, "browser", "Open in browser", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit (tools keep running)", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[&open_item, &browser_item, &PredefinedMenuItem::separator(app)?, &quit_item],
            )?;
            let tray_daemon = daemon.clone();
            TrayIconBuilder::with_id("lantern")
                .icon(Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .tooltip("Lantern")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "open" => show(app),
                    "browser" => {
                        let _ = open::that_detached(tray_daemon.as_str());
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        match app.get_webview_window("main") {
                            Some(w) if w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false) => {
                                let _ = w.hide();
                            }
                            _ => show(app),
                        }
                    }
                })
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Lantern desktop failed to start");
}

/// A JSON string literal, for the initialization script.
fn serde_json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
