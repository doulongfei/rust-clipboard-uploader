#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::fs;
use directories::ProjectDirs;
use anyhow::Result;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Config {
    upload_url: String,
    method: Option<String>,
    file_field: Option<String>,
    headers: Option<serde_json::Value>,
    response: Option<String>,
    copy_to_clipboard: Option<bool>,
}

fn config_path() -> Option<std::path::PathBuf> {
    if let Some(proj) = ProjectDirs::from("com", "example", "RustClipboardUploader") {
        let dir = proj.config_dir();
        let _ = fs::create_dir_all(dir);
        return Some(dir.join("config.yaml"));
    }
    None
}

fn load_config() -> Config {
    if let Some(p) = config_path() {
        if p.exists() {
            if let Ok(s) = fs::read_to_string(&p) {
                if let Ok(cfg) = serde_yaml::from_str(&s) {
                    return cfg;
                }
            }
        }
    }
    Config::default()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            upload_url: "https://0x0.st".to_string(),
            method: Some("POST".to_string()),
            file_field: Some("file".to_string()),
            headers: None,
            response: Some("text".to_string()),
            copy_to_clipboard: Some(false),
        }
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![get_config, save_config])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
fn get_config() -> Result<Config, String> {
    Ok(load_config())
}

#[tauri::command]
fn save_config(cfg: Config) -> Result<(), String> {
    if let Some(p) = config_path() {
        if let Ok(s) = serde_yaml::to_string(&cfg) {
            if fs::write(p, s).is_ok() {
                return Ok(());
            }
        }
    }
    Err("保存失败".to_string())
}
