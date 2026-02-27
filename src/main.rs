use eframe::egui;
use std::fs;
use serde::{Deserialize, Serialize};
use directories::ProjectDirs;
use anyhow::{Result, Context};

use arboard::Clipboard; // 读取剪贴板
use image::ImageFormat;
use reqwest::blocking::Client;
use reqwest::blocking::multipart;
use std::collections::HashMap;

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

fn save_config(cfg: &Config) -> Result<()> {
    if let Some(p) = config_path() {
        let s = serde_yaml::to_string(cfg)?;
        fs::write(p, s)?;
    }
    Ok(())
}

fn read_clipboard_image() -> Option<Vec<u8>> {
    let mut clipboard = Clipboard::new().ok()?;
    if let Ok(image) = clipboard.get_image() {
        // arboard 提供的 image 为 ImageData { width, height, bytes }
        // 这里我们将其编码为 PNG
        if let Ok(buf) = image_to_png(&image.bytes, image.width as u32, image.height as u32) {
            return Some(buf);
        }
    }
    None
}

fn image_to_png(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    // arboard bytes are RGBA32
    use image::{ColorType, ImageEncoder};
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    encoder
        .encode(bytes, width, height, ColorType::Rgba8)
        .context("PNG encode failed")?;
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

fn upload_image(cfg: &Config, img: Vec<u8>) -> Result<String> {
    let client = Client::new();
    let url = &cfg.upload_url;
    let file_field = cfg.file_field.clone().unwrap_or_else(|| "file".to_string());
    let part = multipart::Part::bytes(img).file_name("screenshot.png").mime_str("image/png")?;
    let form = multipart::Form::new().part(file_field, part);
    let mut req = match cfg.method.clone().unwrap_or_else(|| "POST".to_string()).as_str() {
        "PUT" | "put" => client.put(url),
        _ => client.post(url),
    };
    // add headers
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
    let resp = req.multipart(form).send()?;
    let status = resp.status();
    let text = resp.text()?;
    if status.is_success() {
        if let Some(url) = extract_url_from_text(cfg, &text) {
            return Ok(url);
        }
        return Ok(text);
    }
    Err(anyhow::anyhow!("请求失败: {}", status))
}

fn copy_text_to_clipboard(text: &str) -> bool {
    if let Ok(mut cb) = Clipboard::new() {
        if cb.set_text(text.to_string()).is_ok() {
            return true;
        }
    }
    false
}

fn main() -> Result<()> {
    let cfg = load_config();
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "剪贴板上传工具",
        options,
        Box::new(|_cc| Box::new(AppState { config: cfg, last_url: None, status: None }))
    )?;
    Ok(())
}

struct AppState {
    config: Config,
    last_url: Option<String>,
    status: Option<String>,
}

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("剪贴板上传工具（Rust）");
            ui.horizontal(|ui| {
                ui.label("上传地址:");
                let mut url = self.config.upload_url.clone();
                if ui.text_edit_singleline(&mut url).changed() {
                    self.config.upload_url = url.clone();
                }
            });
            ui.horizontal(|ui| {
                ui.label("方法:");
                let mut method = self.config.method.clone().unwrap_or_else(|| "POST".to_string());
                if ui.selectable_value(&mut method, "POST".to_string(), "POST").clicked() {}
                ui.same_line();
                if ui.selectable_value(&mut method, "PUT".to_string(), "PUT").clicked() {}
                self.config.method = Some(method);
            });
            ui.horizontal(|ui| {
                ui.label("文件字段:");
                let mut ff = self.config.file_field.clone().unwrap_or_else(|| "file".to_string());
                if ui.text_edit_singleline(&mut ff).changed() {
                    self.config.file_field = Some(ff.clone());
                }
            });
            ui.horizontal(|ui| {
                ui.label("响应解析(response):");
                let mut resp = self.config.response.clone().unwrap_or_else(|| "text".to_string());
                if ui.text_edit_singleline(&mut resp).changed() {
                    self.config.response = Some(resp.clone());
                }
                ui.label("(text 或 json.path 如 json.data.link)");
            });
            ui.horizontal(|ui| {
                let mut copy = self.config.copy_to_clipboard.unwrap_or(false);
                if ui.checkbox(&mut copy, "上传成功后复制链接到剪贴板").changed() {
                    self.config.copy_to_clipboard = Some(copy);
                }
            });
            if ui.button("保存配置").clicked() {
                match save_config(&self.config) {
                    Ok(_) => self.status = Some("配置已保存".to_string()),
                    Err(e) => self.status = Some(format!("保存配置失败: {}", e)),
                }
            }
            ui.separator();
            if ui.button("从剪贴板上传").clicked() {
                self.status = Some("正在读取剪贴板...".to_string());
                if let Some(img) = read_clipboard_image() {
                    self.status = Some("正在上传...".to_string());
                    match upload_image(&self.config, img) {
                        Ok(url) => {
                            self.last_url = Some(url.clone());
                            self.status = Some("上传成功".to_string());
                            if self.config.copy_to_clipboard.unwrap_or(false) {
                                if copy_text_to_clipboard(&url) {
                                    self.status = Some("上传成功，链接已复制".to_string());
                                } else {
                                    self.status = Some("上传成功，复制到剪贴板失败".to_string());
                                }
                            }
                        }
                        Err(e) => {
                            self.last_url = None;
                            self.status = Some(format!("上传失败: {}", e));
                        }
                    }
                } else {
                    self.status = Some("未检测到剪贴板图片".to_string());
                }
            }
            ui.separator();
            if let Some(s) = &self.status {
                ui.label(format!("状态: {}", s));
            }
            if let Some(url) = &self.last_url {
                ui.separator();
                ui.label("返回链接:");
                ui.monospace(url);
                if ui.button("复制链接").clicked() {
                    if copy_text_to_clipboard(url) {
                        self.status = Some("已复制到剪贴板".to_string());
                    } else {
                        self.status = Some("复制到剪贴板失败".to_string());
                    }
                }
            }
        });
    }
}
