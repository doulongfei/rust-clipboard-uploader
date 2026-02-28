use eframe::egui;
use std::fs;
use serde::{Deserialize, Serialize};
use directories::ProjectDirs;
use anyhow::{Result, Context};

use arboard::Clipboard;
use image::{ColorType, ImageEncoder};
use reqwest::blocking::Client;
use reqwest::blocking::multipart;

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
    let image = clipboard.get_image().ok()?;
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    encoder
        .write_image(&image.bytes, image.width as u32, image.height as u32, ColorType::Rgba8)
        .ok()?;
    Some(out)
}

fn extract_url_from_response(cfg: &Config, text: &str) -> Option<String> {
    match cfg.response.as_deref().unwrap_or("text") {
        "text" => Some(text.trim().to_string()),
        resp if resp.starts_with("json.") => {
            let path = &resp["json.".len()..];
            let j: serde_json::Value = serde_json::from_str(text).ok()?;
            let mut cur = &j;
            for key in path.split('.') {
                cur = cur.get(key)?;
            }
            if cur.is_string() {
                cur.as_str().map(|s| s.to_string())
            } else {
                Some(cur.to_string())
            }
        }
        _ => Some(text.trim().to_string()),
    }
}

fn upload_image(cfg: &Config, img: Vec<u8>) -> Result<String> {
    let client = Client::new();
    let file_field = cfg.file_field.clone().unwrap_or_else(|| "file".to_string());
    let part = multipart::Part::bytes(img)
        .file_name("screenshot.png")
        .mime_str("image/png")?;
    let form = multipart::Form::new().part(file_field, part);

    let method = cfg.method.as_deref().unwrap_or("POST");
    let mut req = if method.eq_ignore_ascii_case("PUT") {
        client.put(&cfg.upload_url)
    } else {
        client.post(&cfg.upload_url)
    };

    if let Some(hv) = &cfg.headers {
        if let Some(obj) = hv.as_object() {
            for (k, v) in obj.iter() {
                let val = v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string());
                req = req.header(k, val);
            }
        }
    }

    let resp = req.multipart(form).send().context("发送请求失败")?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();

    if status.is_success() {
        return Ok(extract_url_from_response(cfg, &text)
            .unwrap_or(text));
    }
    Err(anyhow::anyhow!("请求失败: {} — {}", status, text.trim()))
}

fn copy_text_to_clipboard(text: &str) -> bool {
    Clipboard::new()
        .and_then(|mut cb| cb.set_text(text.to_string()))
        .is_ok()
}

fn main() -> Result<()> {
    let cfg = load_config();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([480.0, 320.0])
            .with_title("剪贴板上传工具"),
        ..Default::default()
    };
    eframe::run_native(
        "剪贴板上传工具",
        options,
        Box::new(|_cc| {
            Ok(Box::new(AppState {
                config: cfg,
                last_url: None,
                status: None,
            }))
        }),
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
            ui.heading("剪贴板上传工具");
            ui.add_space(8.0);

            egui::Grid::new("config_grid")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.label("上传地址:");
                    ui.add(egui::TextEdit::singleline(&mut self.config.upload_url).desired_width(300.0));
                    ui.end_row();

                    ui.label("请求方法:");
                    ui.horizontal(|ui| {
                        let method = self.config.method.get_or_insert_with(|| "POST".to_string());
                        ui.selectable_value(method, "POST".to_string(), "POST");
                        ui.selectable_value(method, "PUT".to_string(), "PUT");
                    });
                    ui.end_row();

                    ui.label("文件字段:");
                    let ff = self.config.file_field.get_or_insert_with(|| "file".to_string());
                    ui.add(egui::TextEdit::singleline(ff).desired_width(300.0));
                    ui.end_row();

                    ui.label("响应解析:");
                    ui.vertical(|ui| {
                        let resp = self.config.response.get_or_insert_with(|| "text".to_string());
                        ui.add(egui::TextEdit::singleline(resp).desired_width(300.0));
                        ui.label(egui::RichText::new("text 或 json.data.link").small().weak());
                    });
                    ui.end_row();

                    ui.label("");
                    let copy = self.config.copy_to_clipboard.get_or_insert(false);
                    ui.checkbox(copy, "上传成功后自动复制链接");
                    ui.end_row();
                });

            ui.add_space(8.0);

            ui.horizontal(|ui| {
                if ui.button("💾 保存配置").clicked() {
                    match save_config(&self.config) {
                        Ok(_) => self.status = Some("✅ 配置已保存".to_string()),
                        Err(e) => self.status = Some(format!("❌ 保存失败: {}", e)),
                    }
                }

                if ui.button("📋 从剪贴板上传").clicked() {
                    match read_clipboard_image() {
                        Some(img) => {
                            self.status = Some("⏳ 正在上传...".to_string());
                            match upload_image(&self.config, img) {
                                Ok(url) => {
                                    let auto_copy = self.config.copy_to_clipboard.unwrap_or(false);
                                    if auto_copy && copy_text_to_clipboard(&url) {
                                        self.status = Some("✅ 上传成功，链接已复制".to_string());
                                    } else {
                                        self.status = Some("✅ 上传成功".to_string());
                                    }
                                    self.last_url = Some(url);
                                }
                                Err(e) => {
                                    self.last_url = None;
                                    self.status = Some(format!("❌ 上传失败: {}", e));
                                }
                            }
                        }
                        None => {
                            self.status = Some("⚠️ 剪贴板中未检测到图片".to_string());
                        }
                    }
                }
            });

            ui.add_space(4.0);
            ui.separator();

            if let Some(s) = &self.status {
                ui.label(s);
            }

            if let Some(url) = self.last_url.clone() {
                ui.add_space(4.0);
                ui.label("返回链接:");
                ui.horizontal(|ui| {
                    ui.monospace(&url);
                    if ui.small_button("📋 复制").clicked() {
                        if copy_text_to_clipboard(&url) {
                            self.status = Some("✅ 已复制到剪贴板".to_string());
                        } else {
                            self.status = Some("❌ 复制到剪贴板失败".to_string());
                        }
                    }
                });
            }
        });
    }
}
