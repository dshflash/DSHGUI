//! Shell settings persisted as JSON in the app config directory.

use std::path::PathBuf;

use tauri::{AppHandle, Manager};

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Explicit path to a deepseek-harness checkout. When empty the shell
    /// falls back to DSH_ENGINE_DIR, then to a checkout next to the app,
    /// then to the published package via npx.
    pub engine_dir: Option<String>,
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join("settings.json"))
        .map_err(|e| format!("cannot resolve app config dir: {e}"))
}

pub fn load(app: &AppHandle) -> Settings {
    let Ok(path) = settings_path(app) else { return Settings::default() };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let path = settings_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create config dir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(settings).map_err(|e| format!("cannot serialize settings: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("cannot write settings: {e}"))
}