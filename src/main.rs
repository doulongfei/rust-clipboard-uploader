use eframe::egui;
use std::fs;
use serde::{Deserialize, Serialize};
use directories::ProjectDirs;
use anyhow::{Result, Context};

use arboard::Clipboard;
use image::{ExtendedColorType, ImageEncoder};
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
        .write_image(&image.bytes, image.width as u32, image.height as u32, ExtendedColorType::Rgba8)
        .ok()?;
    Some(out)
}

/// 从响应文本中按配置提取 URL
/// 支持：
///   text          → 直接返回整个响应体
///   json.foo.bar  → 从 JSON 对象按路径提取
///   json[0].foo   → 从 JSON 数组取第 0 个元素再按路径提取
fn extract_url_from_response(cfg: &Config, text: &str) -> Option<String> {
    let resp = cfg.response.as_deref().unwrap_or("text");
    if resp == "text" {
        return Some(text.trim().to_string());
    }
    if !resp.starts_with("json") {
        return Some(text.trim().to_string());
    }

    let j: serde_json::Value = serde_json::from_str(text).ok()?;

    // 如果顶层是数组，先取第一个元素
    let root = if j.is_array() {
        j.get(0)?
    } else {
        &j
    };

    // 取 "json." 之后的路径，如果没有路径则直接返回 root
    let path_str = resp.strip_prefix("json.").unwrap_or("").trim();
    if path_str.is_empty() {
        return Some(root.to_string());
    }

    let mut cur = root;
    for key in path_str.split('.') {
        cur = cur.get(key)?;
    }

    if cur.is_string() {
        cur.as_str().map(|s| s.to_string())
    } else {
        Some(cur.to_string())
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
        return Ok(extract_url_from_response(cfg, &text).unwrap_or(text));
    }
    Err(anyhow::anyhow!("请求失败: {} — {}", status, text.trim()))
}

fn copy_text_to_clipboard(text: &str) -> bool {
    Clipboard::new()
        .and_then(|mut cb| cb.set_text(text.to_string()))
        .is_ok()
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    let candidates: &[&str] = &[
        // macOS
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/Library/Fonts/Arial Unicode MS.ttf",
        // Linux
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/arphic/uming.ttc",
        // Windows
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simsun.ttc",
    ];

    for path in candidates {
        if let Ok(data) = std::fs::read(path) {
            fonts.font_data.insert(
                "cjk".to_owned(),
                egui::FontData::from_owned(data).into(),
            );
            fonts
                .families
                .get_mut(&egui::FontFamily::Proportional)
                .unwrap()
                .insert(0, "cjk".to_owned());
            fonts
                .families
                .get_mut(&egui::FontFamily::Monospace)
                .unwrap()
                .push("cjk".to_owned());
            break;
        }
    }

    ctx.set_fonts(fonts);
}

fn main() {
    let cfg = load_config();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 380.0])
            .with_title("剪贴板上传工具"),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native(
        "剪贴板上传工具",
        options,
        Box::new(|cc| {
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(AppState {
                config: cfg,
                last_url: None,
                status: None,
                uploading: false,
            }))
        }),
    ) {
        eprintln!("启动失败: {e}");
        std::process::exit(1);
    }
}

struct AppState {
    config: Config,
    last_url: Option<String>,
    status: Option<(bool, String)>, // (is_error, message)
    uploading: bool,
}

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("剪贴板上传工具");
            ui.add_space(8.0);

            // ── 配置区 ──────────────────────────────────────────
            egui::Grid::new("config_grid")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.label("上传地址:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.upload_url)
                            .desired_width(f32::INFINITY),
                    );
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
                    ui.add(egui::TextEdit::singleline(ff).desired_width(f32::INFINITY));
                    ui.end_row();

                    ui.label("响应解析:");
                    ui.vertical(|ui| {
                        let resp = self.config.response.get_or_insert_with(|| "text".to_string());
                        ui.add(egui::TextEdit::singleline(resp).desired_width(f32::INFINITY));
                        ui.label(
                            egui::RichText::new("text | json.url | json.data.link")
                                .small()
                                .weak(),
                        );
                    });
                    ui.end_row();

                    ui.label("");
                    let copy = self.config.copy_to_clipboard.get_or_insert(false);
                    ui.checkbox(copy, "上传成功后自动复制链接");
                    ui.end_row();
                });

            ui.add_space(10.0);

            // ── 操作按钮 ────────────────────────────────────────
            ui.horizontal(|ui| {
                if ui.button("💾 保存配置").clicked() {
                    match save_config(&self.config) {
                        Ok(_) => self.status = Some((false, "配置已保存".to_string())),
                        Err(e) => self.status = Some((true, format!("保存失败: {}", e))),
                    }
                }

                let btn_label = if self.uploading { "⏳ 上传中..." } else { "📤 从剪贴板上传" };
                let upload_btn = ui.add_enabled(!self.uploading, egui::Button::new(btn_label));

                if upload_btn.clicked() {
                    match read_clipboard_image() {
                        Some(img) => {
                            self.uploading = true;
                            self.status = None;
                            self.last_url = None;
                            ctx.request_repaint();

                            match upload_image(&self.config, img) {
                                Ok(url) => {
                                    let auto_copy = self.config.copy_to_clipboard.unwrap_or(false);
                                    if auto_copy && copy_text_to_clipboard(&url) {
                                        self.status = Some((false, "上传成功，链接已复制".to_string()));
                                    } else {
                                        self.status = Some((false, "上传成功".to_string()));
                                    }
                                    self.last_url = Some(url);
                                }
                                Err(e) => {
                                    self.status = Some((true, format!("上传失败: {}", e)));
                                }
                            }
                            self.uploading = false;
                        }
                        None => {
                            self.status = Some((true, "剪贴板中未检测到图片".to_string()));
                        }
                    }
                }
            });

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);

            // ── 状态栏 ──────────────────────────────────────────
            if let Some((is_err, msg)) = &self.status {
                let icon = if *is_err { "❌" } else { "✅" };
                let color = if *is_err {
                    egui::Color32::from_rgb(220, 80, 80)
                } else {
                    egui::Color32::from_rgb(80, 200, 120)
                };
                ui.label(egui::RichText::new(format!("{} {}", icon, msg)).color(color));
                ui.add_space(4.0);
            }

            // ── 返回链接区 ──────────────────────────────────────
            if let Some(url) = self.last_url.clone() {
                ui.label(egui::RichText::new("返回链接").strong());
                ui.add_space(2.0);

                egui::Frame::default()
                    .fill(egui::Color32::from_gray(30))
                    .rounding(egui::Rounding::same(4.0))
                    .inner_margin(egui::Margin::same(8.0))
                    .show(ui, |ui: &mut egui::Ui| {
                        ui.set_width(ui.available_width());
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&url)
                                    .monospace()
                                    .color(egui::Color32::from_rgb(100, 200, 255)),
                            )
                            .wrap(),
                        );
                    });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("📋 复制链接").clicked() {
                        if copy_text_to_clipboard(&url) {
                            self.status = Some((false, "已复制到剪贴板".to_string()));
                        } else {
                            self.status = Some((true, "复制到剪贴板失败".to_string()));
                        }
                    }
                    // 可点击的超链接
                    ui.hyperlink_to("🔗 在浏览器打开", &url);
                });
            }
        });
    }
}
