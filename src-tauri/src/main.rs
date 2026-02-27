#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::fs;
use directories::ProjectDirs;
use anyhow::{Result, Context};

use arboard::Clipboard;
use image::ColorType;
use reqwest::blocking::{Client, multipart};

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Config {
    upload_url: String,
    method: Option<String>,
    file_field: Option<String>,
    headers: Option<serde_json::Value>,
    response: Option<String>,
    copy_to_clipboard: Option<bool>,
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

fn save_config_internal(cfg: &Config) -> Result<()> {
    if let Some(p) = config_path() {
        let s = serde_yaml::to_string(cfg)?;
        fs::write(p, s)?;
    }
    Ok(())
}

fn read_clipboard_png() -> Result<Vec<u8>> {
    let mut cb = Clipboard::new().context("无法打开剪贴板")?;
    let img = cb.get_image().context("剪贴板中没有图片或读取失败")?;
    // arboard gives RGBA32 bytes
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    encoder.encode(&img.bytes, img.width as u32, img.height as u32, ColorType::Rgba8)
        .context("PNG 编码失败")?;
    Ok(out)
}

fn extract_url_from_text(cfg: &Config, text: &str) -> Option<String> {
    if let Some(resp_mode) = &cfg.response {
        if resp_mode == "text" {
            return Some(text.trim().to_string());
        }
        if resp_mode.starts_with("json") {
            if let Some(idx) = resp_mode.find('.') {
                let path = &resp_mode[idx+1..];
                if let Ok(j) = serde_json::from_str::<serde_json::Value>(text) {
                    let mut cur = &j;
                    for p in path.split('.') {
                        if let Some(next) = cur.get(p) {
                            cur = next;
                        } else {
                            return None;
                        }
                    }
                    if cur.is_string() {
                        return cur.as_str().map(|s| s.to_string());
                    } else {
                        return Some(cur.to_string());
                    }
                }
            }
        }
    }
    None
}

fn upload_bytes(cfg: &Config, img: Vec<u8>) -> Result<String> {
    let client = Client::new();
    let url = &cfg.upload_url;
    let file_field = cfg.file_field.clone().unwrap_or_else(|| "file".to_string());
    let part = multipart::Part::bytes(img).file_name("screenshot.png").mime_str("image/png")?;
    let form = multipart::Form::new().part(file_field, part);
    let mut req = match cfg.method.clone().unwrap_or_else(|| "POST".to_string()).as_str() {
        "PUT" | "put" => client.put(url),
        _ => client.post(url),
    };
    if let Some(hv) = &cfg.headers {
        if let Some(obj) = hv.as_object() {
            for (k, v) in obj.iter() {
                if let Some(s) = v.as_str() {
                    req = req.header(k, s);
                } else {
                    req = req.header(k, v.to_string());
                }
            }
        }
    }
    let resp = req.multipart(form).send().context("上传请求失败")?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if status.is_success() {
        if let Some(url) = extract_url_from_text(cfg, &text) {
            return Ok(url);
        }
        return Ok(text);
    }
    Err(anyhow::anyhow!("请求失败: {}", status))
}

fn copy_to_clipboard(text: &str) -> bool {
    if let Ok(mut cb) = Clipboard::new() {
        cb.set_text(text.to_string()).is_ok()
    } else {
        false
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![get_config, save_config, upload_clipboard])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
fn get_config() -> Result<Config, String> {
    Ok(load_config())
}

#[tauri::command]
fn save_config(cfg: Config) -> Result<(), String> {
    save_config_internal(&cfg).map_err(|e| format!("保存失败: {}", e))?;
    Ok(())
}

#[tauri::command]
fn upload_clipboard() -> Result<String, String> {
    let cfg = load_config();
    let img = read_clipboard_png().map_err(|e| format!("读取剪贴板失败: {}", e))?;
    let url = upload_bytes(&cfg, img).map_err(|e| format!("上传失败: {}", e))?;
    if cfg.copy_to_clipboard.unwrap_or(false) {
        let _ = copy_to_clipboard(&url);
    }
    Ok(url)
}
