//! Typed commands exposed to the shell frontend. The remote harness UI gets
//! none of these: it runs unprivileged in the webview.

use std::process::Command;

use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

use crate::engine::{EngineStateView, ManagedEngine};
use crate::settings::{self, Settings};

#[tauri::command]
pub fn get_state(engine: State<'_, ManagedEngine>) -> EngineStateView {
    engine.0.snapshot()
}

#[tauri::command]
pub fn get_log(engine: State<'_, ManagedEngine>, from: usize) -> Vec<String> {
    engine.0.log_lines(from)
}

#[tauri::command]
pub fn get_settings(app: AppHandle) -> Settings {
    settings::load(&app)
}

#[tauri::command]
pub fn start_engine(engine: State<'_, ManagedEngine>) -> Result<(), String> {
    engine.0.start()
}

#[tauri::command]
pub fn restart_engine(engine: State<'_, ManagedEngine>) -> Result<(), String> {
    engine.0.restart()
}

#[tauri::command]
pub fn stop_engine(engine: State<'_, ManagedEngine>) -> Result<(), String> {
    engine.0.stop();
    Ok(())
}

#[tauri::command]
pub fn setup_engine(engine: State<'_, ManagedEngine>) -> Result<(), String> {
    engine.0.setup_engine()
}

#[tauri::command]
pub fn set_engine_dir(
    app: AppHandle,
    engine: State<'_, ManagedEngine>,
    dir: Option<String>,
) -> Result<(), String> {
    let mut current = settings::load(&app);
    current.engine_dir = dir.filter(|d| !d.trim().is_empty());
    settings::save(&app, &current)?;
    engine.0.set_settings(current);
    engine.0.restart()
}

/// Native folder picker; async so the blocking dialog stays off the main thread.
#[tauri::command]
pub async fn pick_engine_dir(app: AppHandle) -> Option<String> {
    app.dialog()
        .file()
        .blocking_pick_folder()
        .and_then(|picked| picked.into_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn open_external(url: String) -> Result<(), String> {
    let status = open_in_browser(&url).map_err(|e| format!("cannot open browser: {e}"))?;
    if !status.success() {
        return Err(format!("failed to open {url}"));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_in_browser(url: &str) -> std::io::Result<std::process::ExitStatus> {
    Command::new("cmd").args(["/C", "start", "", url]).status()
}

#[cfg(target_os = "macos")]
fn open_in_browser(url: &str) -> std::io::Result<std::process::ExitStatus> {
    Command::new("open").arg(url).status()
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn open_in_browser(url: &str) -> std::io::Result<std::process::ExitStatus> {
    Command::new("xdg-open").arg(url).status()
}