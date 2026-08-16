//! dsh-desktop: a Tauri shell that runs the DeepSeek Harness `dsh web`
//! engine as a managed child process and embeds its Web UI in the window.

mod commands;
mod engine;
mod settings;

use std::sync::Arc;

use engine::{EngineManager, ManagedEngine};
use tauri::Manager;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let shell_settings = settings::load(app.handle());
            // The shell must always be able to return to the splash page
            // when the engine stops. The webview's own URL is not usable at
            // setup time (the page has not loaded yet, so it reads
            // about:blank), therefore the splash URL is constructed from the
            // known local origins instead; a captured value is only a fallback.
            let splash_url = resolve_splash_url(app.handle());
            let engine = EngineManager::new(app.handle().clone(), shell_settings, splash_url);
            app.manage(ManagedEngine(Arc::clone(&engine)));
            let _ = engine.start();
            Ok(())
        })
        .menu(|handle| {
            let open = MenuItem::with_id(handle, "open-browser", "Open in Browser", true, None::<&str>)?;
            let restart = MenuItem::with_id(handle, "restart-engine", "Restart Engine", true, None::<&str>)?;
            let stop = MenuItem::with_id(handle, "stop-engine", "Stop Engine", true, None::<&str>)?;
            let quit = PredefinedMenuItem::quit(handle, Some("Quit"))?;
            let engine_menu = Submenu::with_items(handle, "Engine", true, &[&open, &restart, &stop, &quit])?;
            Menu::with_items(handle, &[&engine_menu])
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open-browser" => {
                if let Some(engine) = app.try_state::<ManagedEngine>() {
                    let state = engine.0.snapshot();
                    if let Some(url) = state.url {
                        let _ = commands::open_external(url);
                    }
                }
            }
            "restart-engine" => {
                if let Some(engine) = app.try_state::<ManagedEngine>() {
                    let _ = engine.0.restart();
                }
            }
            "stop-engine" => {
                if let Some(engine) = app.try_state::<ManagedEngine>() {
                    engine.0.stop();
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::get_log,
            commands::get_settings,
            commands::start_engine,
            commands::restart_engine,
            commands::stop_engine,
            commands::setup_engine,
            commands::set_engine_dir,
            commands::pick_engine_dir,
            commands::open_external,
        ])
        .build(tauri::generate_context!())
        .expect("error while building dsh-desktop")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(engine) = app.try_state::<ManagedEngine>() {
                    engine.0.stop();
                }
            }
        });
}

/// Resolve the local splash page URL: the captured webview URL when it is
/// already meaningful, otherwise the dev server URL in dev builds and the
/// embedded-assets origin in release builds.
fn resolve_splash_url(app: &tauri::AppHandle) -> Option<url::Url> {
    let captured = app
        .get_webview_window("main")
        .and_then(|window| window.url().ok())
        .filter(|u| u.as_str() != "about:blank");
    if captured.is_some() {
        return captured;
    }
    if tauri::is_dev() {
        return app.config().build.dev_url.clone();
    }
    #[cfg(target_os = "windows")]
    {
        url::Url::parse("http://tauri.localhost").ok()
    }
    #[cfg(not(target_os = "windows"))]
    {
        url::Url::parse("tauri://localhost").ok()
    }
}