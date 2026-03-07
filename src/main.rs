use anyhow::Result;
use arboard::Clipboard;
use chrono::Local;
use directories::ProjectDirs;
use eframe::egui;
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager,
};
use image::{ExtendedColorType, ImageEncoder};
use reqwest::blocking::multipart;
use reqwest::blocking::Client;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu},
    Icon as TrayIconImg, TrayIcon, TrayIconBuilder,
};

// ── 上传任务 ──────────────────────────────────────────────
#[derive(Debug, Clone)]
enum TaskStatus {
    Uploading,
    Processing, // 已上传，服务端处理中（如大模型命名）
    Retrying {
        message: String,
        attempt: u8,
        max_retries: u8,
        wait_seconds: u64,
    },
    Success(String, String), // (url, src)
    Failed {
        message: String,
        retryable: bool,
    },
}

#[derive(Debug, Clone)]
struct UploadTask {
    id: usize,
    status: TaskStatus,
    image_data: Vec<u8>,              // 用于失败重试；完成后清空
    created_at: String,               // 格式 %H:%M:%S
    data_expires_at: Option<Instant>, // 失败任务图片数据过期时间（5分钟后自动释放）
    retry_count: u8,                  // 已执行的自动重试次数
}

// ── 消息类型（后台线程 → 主线程）──────────────────────────
#[derive(Debug)]
enum AppEvent {
    TaskProgress(usize, TaskStatus), // (task_id, new_status)
    WatchUpload(Vec<u8>),            // 监听线程捕获到新图片，主线程分配任务
    RetryTask(usize),                // 自动重试定时器回到主线程
    TrayShowWindow,
    TrayUpload,
    TrayToggleWatch,
    TrayQuit,
    TrayCopyUrl(String), // 点击最近上传项 → 复制URL
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ThemeMode {
    System,
    Light,
    Dark,
}

impl ThemeMode {
    fn label(self) -> &'static str {
        match self {
            Self::System => "跟随系统",
            Self::Light => "浅色",
            Self::Dark => "深色",
        }
    }

    fn preference(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum AccentColor {
    Blue,
    Green,
    Orange,
    Pink,
    Purple,
}

impl AccentColor {
    fn label(self) -> &'static str {
        match self {
            Self::Blue => "系统蓝",
            Self::Green => "青绿",
            Self::Orange => "橙色",
            Self::Pink => "玫粉",
            Self::Purple => "紫色",
        }
    }
}

// ── 配置 ─────────────────────────────────────────────────
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Config {
    upload_url: String,
    method: Option<String>,
    file_field: Option<String>,
    headers: Option<serde_json::Value>,
    response: Option<String>,
    copy_to_clipboard: Option<bool>,
    auto_watch: Option<bool>,
    auto_retry: Option<bool>,
    notify_on_success: Option<bool>,
    close_to_tray: Option<bool>,
    theme_mode: Option<ThemeMode>,
    accent_color: Option<AccentColor>,
    hotkey: Option<String>,
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
            auto_watch: Some(false),
            auto_retry: Some(true),
            notify_on_success: Some(false),
            close_to_tray: Some(true),
            theme_mode: Some(ThemeMode::System),
            accent_color: Some(AccentColor::Blue),
            hotkey: None,
        }
    }
}

const AUTO_RETRY_MAX_RETRIES: u8 = 2;

fn auto_retry_enabled(config: &Config) -> bool {
    config.auto_retry.unwrap_or(true)
}

fn auto_retry_delay_seconds(retry_attempt: u8) -> u64 {
    match retry_attempt {
        1 => 2,
        2 => 5,
        _ => 8,
    }
}

fn task_is_active(status: &TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Uploading | TaskStatus::Processing | TaskStatus::Retrying { .. }
    )
}

// ── 配置文件路径 ──────────────────────────────────────────
fn config_path() -> Option<std::path::PathBuf> {
    ProjectDirs::from("com", "example", "RustClipboardUploader").map(|proj| {
        let dir = proj.config_dir().to_path_buf();
        let _ = fs::create_dir_all(&dir);
        dir.join("config.yaml")
    })
}

fn data_dir() -> Option<std::path::PathBuf> {
    ProjectDirs::from("com", "example", "RustClipboardUploader").map(|proj| {
        let dir = proj.data_dir().to_path_buf();
        let _ = fs::create_dir_all(&dir);
        dir
    })
}

fn db_path() -> Option<std::path::PathBuf> {
    data_dir().map(|d| d.join("history.db"))
}

// ── 历史记录 ──────────────────────────────────────────────
#[derive(Clone, Debug)]
struct HistoryRecord {
    id: i64,
    url: String,
    src: String,
    uploaded_at: String,
}

fn open_db() -> Option<Connection> {
    let path = db_path()?;
    let conn = Connection::open(path).ok()?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS history (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            url         TEXT NOT NULL,
            src         TEXT NOT NULL DEFAULT '',
            uploaded_at TEXT NOT NULL
        );",
    )
    .ok()?;
    Some(conn)
}

fn db_insert(conn: &Connection, url: &str, src: &str) {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let _ = conn.execute(
        "INSERT INTO history (url, src, uploaded_at) VALUES (?1, ?2, ?3)",
        params![url, src, now],
    );
}

fn db_load(conn: &Connection, limit: usize) -> Vec<HistoryRecord> {
    let mut stmt = match conn
        .prepare("SELECT id, url, src, uploaded_at FROM history ORDER BY id DESC LIMIT ?1")
    {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map(params![limit as i64], |row| {
        Ok(HistoryRecord {
            id: row.get(0)?,
            url: row.get(1)?,
            src: row.get(2)?,
            uploaded_at: row.get(3)?,
        })
    })
    .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
    .unwrap_or_default()
}

fn db_delete(conn: &Connection, id: i64) {
    let _ = conn.execute("DELETE FROM history WHERE id = ?1", params![id]);
}

fn db_clear(conn: &Connection) {
    let _ = conn.execute("DELETE FROM history", []);
}

fn load_config() -> Config {
    config_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_yaml::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &Config) -> Result<()> {
    if let Some(p) = config_path() {
        fs::write(p, serde_yaml::to_string(cfg)?)?;
    }
    Ok(())
}

// ── 剪贴板图片读取 ────────────────────────────────────────
fn read_clipboard_image() -> Option<Vec<u8>> {
    let mut clipboard = Clipboard::new().ok()?;
    let image = clipboard.get_image().ok()?;
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    encoder
        .write_image(
            &image.bytes,
            image.width as u32,
            image.height as u32,
            ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(out)
}

/// 图片内容的简单指纹（长度 + 前64字节）
fn image_fingerprint(data: &[u8]) -> u64 {
    let len = data.len() as u64;
    let prefix: u64 = data
        .iter()
        .take(64)
        .enumerate()
        .fold(0u64, |acc, (i, &b)| acc ^ ((b as u64) << (i % 8 * 8)));
    len ^ (prefix.wrapping_mul(0x9e3779b97f4a7c15))
}

/// 轻量剪贴板指纹：只读原始 RGBA bytes 做指纹，不做 PNG 编码，避免高频内存分配
fn clipboard_raw_fingerprint() -> Option<u64> {
    let mut cb = Clipboard::new().ok()?;
    let img = cb.get_image().ok()?;
    Some(image_fingerprint(&img.bytes))
}

// ── 响应解析 ─────────────────────────────────────────────
/// 支持路径格式：
///   "text"          → 原始文本
///   "[0].url"       → JSON 数组第0项的 url 字段
///   "json.url"      → JSON 对象的 url 字段
///   "json.data.url" → 嵌套字段
fn extract_url_from_response(cfg: &Config, text: &str) -> Option<String> {
    let resp = cfg.response.as_deref().unwrap_or("text").trim();
    if resp == "text" || resp.is_empty() {
        return Some(text.trim().to_string());
    }
    let j: serde_json::Value = serde_json::from_str(text).ok()?;
    let path_str = if resp.starts_with("json.") {
        resp.strip_prefix("json.").unwrap_or("").to_string()
    } else if resp == "json" {
        String::new()
    } else {
        resp.replace('[', "").replace(']', "")
    };

    let mut cur = &j;
    for seg in path_str.split('.') {
        if seg.is_empty() {
            continue;
        }
        if let Ok(idx) = seg.parse::<usize>() {
            cur = cur.get(idx)?;
        } else {
            if cur.is_array() {
                cur = cur.get(0)?;
            }
            cur = cur.get(seg)?;
        }
    }
    if path_str.is_empty() {
        if cur.is_array() {
            cur = cur.get(0)?;
        }
    }
    if cur.is_string() {
        cur.as_str().map(|s| s.to_string())
    } else {
        Some(cur.to_string())
    }
}

/// 从响应中提取 src 字段（JSON 数组格式 [{"src":"...","url":"..."}]）
fn extract_src_from_response(text: &str) -> String {
    let j: serde_json::Value = serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
    let root = if j.is_array() {
        j.get(0).unwrap_or(&j)
    } else {
        &j
    };
    root.get("src")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

// ── 上传：构建请求（不发送）────────────────────────────
fn build_upload_request(
    client: &Client,
    cfg: &Config,
    img: Vec<u8>,
) -> Result<reqwest::blocking::RequestBuilder> {
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
                let val = v
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| v.to_string());
                req = req.header(k, val);
            }
        }
    }
    Ok(req.multipart(form))
}

#[derive(Debug)]
struct UploadFailure {
    message: String,
    retryable: bool,
}

// ── 上传：解析响应 ────────────────────────────────────
fn parse_upload_response(
    cfg: &Config,
    resp: reqwest::blocking::Response,
) -> std::result::Result<(String, String), UploadFailure> {
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if status.is_success() {
        let url = extract_url_from_response(cfg, &text).unwrap_or_else(|| text.clone());
        let src = extract_src_from_response(&text);
        return Ok((url, src));
    }
    Err(UploadFailure {
        message: format!("请求失败: {} — {}", status, text.trim()),
        retryable: status.as_u16() == 429 || status.is_server_error(),
    })
}

// ── 剪贴板写入 ───────────────────────────────────────────
fn copy_text_to_clipboard(text: &str) -> bool {
    Clipboard::new()
        .and_then(|mut cb| cb.set_text(text.to_string()))
        .is_ok()
}

// ── 系统通知 ─────────────────────────────────────────────
fn send_notification(title: &str, body: &str) {
    let _ = notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .timeout(notify_rust::Timeout::Milliseconds(4000))
        .show();
}

// ── 字体加载 ─────────────────────────────────────────────
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // 内嵌 Noto Emoji 字体（编译时打包）
    fonts.font_data.insert(
        "noto_emoji".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/NotoEmoji-Regular.ttf")).into(),
    );

    // CJK 字体候选（系统字体）
    let candidates: &[&str] = &[
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/Library/Fonts/Arial Unicode MS.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simsun.ttc",
    ];
    for path in candidates {
        if let Ok(data) = fs::read(path) {
            fonts
                .font_data
                .insert("cjk".to_owned(), egui::FontData::from_owned(data).into());
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

    // Noto Emoji 作为 fallback（放在最后，找不到字形时用它）
    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .unwrap()
        .push("noto_emoji".to_owned());
    fonts
        .families
        .get_mut(&egui::FontFamily::Monospace)
        .unwrap()
        .push("noto_emoji".to_owned());

    ctx.set_fonts(fonts);
}

fn apple_green() -> egui::Color32 {
    egui::Color32::from_rgb(52, 199, 89)
}

fn apple_orange() -> egui::Color32 {
    egui::Color32::from_rgb(255, 159, 10)
}

fn apple_red() -> egui::Color32 {
    egui::Color32::from_rgb(255, 69, 58)
}

#[derive(Clone, Copy)]
struct ThemePalette {
    accent: egui::Color32,
    bg: egui::Color32,
    card: egui::Color32,
    soft: egui::Color32,
    row: egui::Color32,
    border: egui::Color32,
    text: egui::Color32,
    muted: egui::Color32,
    shadow: egui::Color32,
    glow_a: egui::Color32,
    glow_b: egui::Color32,
}

fn accent_color(accent: AccentColor) -> egui::Color32 {
    match accent {
        AccentColor::Blue => egui::Color32::from_rgb(10, 132, 255),
        AccentColor::Green => egui::Color32::from_rgb(48, 209, 88),
        AccentColor::Orange => egui::Color32::from_rgb(255, 159, 10),
        AccentColor::Pink => egui::Color32::from_rgb(255, 55, 95),
        AccentColor::Purple => egui::Color32::from_rgb(191, 90, 242),
    }
}

fn accent_color_soft(accent: AccentColor, dark_mode: bool) -> egui::Color32 {
    let color = accent_color(accent);
    if dark_mode {
        color.gamma_multiply(0.28)
    } else {
        color.gamma_multiply(0.12)
    }
}

fn config_theme_mode(config: &Config) -> ThemeMode {
    config.theme_mode.unwrap_or(ThemeMode::System)
}

fn config_accent_color(config: &Config) -> AccentColor {
    config.accent_color.unwrap_or(AccentColor::Blue)
}

fn theme_palette(theme: egui::Theme, accent: AccentColor) -> ThemePalette {
    let accent = accent_color(accent);
    match theme {
        egui::Theme::Light => ThemePalette {
            accent,
            bg: egui::Color32::from_rgb(242, 244, 248),
            card: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 244),
            soft: egui::Color32::from_rgb(247, 249, 252),
            row: egui::Color32::from_rgb(251, 252, 255),
            border: egui::Color32::from_rgba_unmultiplied(15, 23, 42, 22),
            text: egui::Color32::from_rgb(29, 31, 36),
            muted: egui::Color32::from_rgb(111, 118, 132),
            shadow: egui::Color32::from_black_alpha(14),
            glow_a: egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 22),
            glow_b: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 160),
        },
        egui::Theme::Dark => ThemePalette {
            accent,
            bg: egui::Color32::from_rgb(17, 19, 24),
            card: egui::Color32::from_rgba_unmultiplied(28, 31, 38, 242),
            soft: egui::Color32::from_rgb(33, 37, 46),
            row: egui::Color32::from_rgb(37, 42, 52),
            border: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 22),
            text: egui::Color32::from_rgb(240, 243, 248),
            muted: egui::Color32::from_rgb(154, 160, 173),
            shadow: egui::Color32::from_black_alpha(46),
            glow_a: egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 34),
            glow_b: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18),
        },
    }
}

fn panel_card_frame(palette: ThemePalette) -> egui::Frame {
    egui::Frame::none()
        .fill(palette.card)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .rounding(egui::Rounding::same(24.0))
        .shadow(egui::Shadow {
            offset: egui::vec2(0.0, 10.0),
            blur: 26.0,
            spread: 0.0,
            color: palette.shadow,
        })
        .inner_margin(egui::Margin::same(20.0))
}

fn soft_card_frame(palette: ThemePalette) -> egui::Frame {
    egui::Frame::none()
        .fill(palette.soft)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .rounding(egui::Rounding::same(18.0))
        .inner_margin(egui::Margin::same(14.0))
}

fn setting_row_frame(palette: ThemePalette) -> egui::Frame {
    egui::Frame::none()
        .fill(palette.row)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .rounding(egui::Rounding::same(16.0))
        .inner_margin(egui::Margin::symmetric(14.0, 12.0))
}

fn pill_frame(fill: egui::Color32, stroke: egui::Color32) -> egui::Frame {
    egui::Frame::none()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .rounding(egui::Rounding::same(999.0))
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
}

fn render_pill(ui: &mut egui::Ui, text: &str, fill: egui::Color32, text_color: egui::Color32) {
    pill_frame(fill, fill).show(ui, |ui| {
        ui.label(
            egui::RichText::new(text)
                .size(12.5)
                .color(text_color)
                .strong(),
        );
    });
}

fn render_metric_tile(
    ui: &mut egui::Ui,
    value: &str,
    label: &str,
    tint: egui::Color32,
    palette: ThemePalette,
) {
    soft_card_frame(palette)
        .fill(if ui.visuals().dark_mode {
            tint.gamma_multiply(0.22)
        } else {
            tint.gamma_multiply(0.10)
        })
        .stroke(egui::Stroke::new(1.0, tint.gamma_multiply(0.35)))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(value)
                        .size(24.0)
                        .color(palette.text)
                        .strong(),
                );
                ui.label(egui::RichText::new(label).size(12.5).color(palette.muted));
            });
        });
}

fn render_summary_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    tint: egui::Color32,
    palette: ThemePalette,
) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).size(13.5).color(palette.muted));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            render_pill(
                ui,
                value,
                if ui.visuals().dark_mode {
                    tint.gamma_multiply(0.26)
                } else {
                    tint.gamma_multiply(0.12)
                },
                tint.gamma_multiply(0.95),
            );
        });
    });
}

fn render_window_controls(ui: &mut egui::Ui) {
    let colors = [apple_red(), apple_orange(), apple_green()];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for color in colors {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 5.0, color);
        }
    });
}

fn render_panel_header(ui: &mut egui::Ui, title: &str, subtitle: &str, palette: ThemePalette) {
    ui.label(
        egui::RichText::new(title)
            .size(20.0)
            .color(palette.text)
            .strong(),
    );
    ui.label(
        egui::RichText::new(subtitle)
            .size(13.0)
            .color(palette.muted),
    );
}

fn render_setting_row(
    ui: &mut egui::Ui,
    title: &str,
    detail: &str,
    palette: ThemePalette,
    add_control: impl FnOnce(&mut egui::Ui),
) {
    let compact = ui.available_width() < 430.0;
    setting_row_frame(palette).show(ui, |ui| {
        if compact {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(title).color(palette.text).strong());
                ui.label(egui::RichText::new(detail).size(12.5).color(palette.muted));
                ui.add_space(8.0);
                add_control(ui);
            });
        } else {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(title).color(palette.text).strong());
                    ui.label(egui::RichText::new(detail).size(12.5).color(palette.muted));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    add_control(ui);
                });
            });
        }
    });
}

fn render_setting_stack_row(
    ui: &mut egui::Ui,
    title: &str,
    detail: &str,
    palette: ThemePalette,
    add_control: impl FnOnce(&mut egui::Ui),
) {
    setting_row_frame(palette).show(ui, |ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).color(palette.text).strong());
            ui.label(egui::RichText::new(detail).size(12.5).color(palette.muted));
            ui.add_space(10.0);
            add_control(ui);
        });
    });
}

fn apple_toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let desired_size = egui::vec2(46.0, 28.0);
    let (rect, mut response) = ui.allocate_exact_size(desired_size, egui::Sense::click());

    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }

    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool(response.id, *on);
        let rounding = egui::Rounding::same(rect.height() / 2.0);
        let track_fill = if *on {
            apple_green()
        } else {
            egui::Color32::from_rgb(160, 167, 179)
        };
        let track_stroke = egui::Stroke::new(
            1.0,
            if response.hovered() {
                if ui.visuals().dark_mode {
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 48)
                } else {
                    egui::Color32::from_rgba_unmultiplied(15, 23, 42, 36)
                }
            } else {
                egui::Color32::TRANSPARENT
            },
        );
        ui.painter().rect(rect, rounding, track_fill, track_stroke);

        let knob_radius = 10.0;
        let knob_x = rect.left() + 14.0 + how_on * (rect.width() - 28.0);
        let knob_center = egui::pos2(knob_x, rect.center().y);
        ui.painter()
            .circle_filled(knob_center, knob_radius, egui::Color32::WHITE);
        ui.painter().circle_stroke(
            knob_center,
            knob_radius,
            egui::Stroke::new(
                1.0,
                if ui.visuals().dark_mode {
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 28)
                } else {
                    egui::Color32::from_rgba_unmultiplied(15, 23, 42, 24)
                },
            ),
        );
    }

    response
}

fn secondary_button(text: impl Into<String>, palette: ThemePalette) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.into())
            .color(palette.text)
            .strong(),
    )
    .fill(palette.row)
    .stroke(egui::Stroke::new(1.0, palette.border))
    .rounding(egui::Rounding::same(14.0))
}

fn primary_button(text: impl Into<String>, palette: ThemePalette) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.into())
            .color(egui::Color32::WHITE)
            .strong(),
    )
    .fill(palette.accent)
    .stroke(egui::Stroke::new(1.0, palette.accent))
    .rounding(egui::Rounding::same(14.0))
}

fn render_singleline_input(
    ui: &mut egui::Ui,
    value: &mut String,
    hint: &str,
    palette: ThemePalette,
) -> egui::Response {
    let dark_mode = ui.visuals().dark_mode;
    let input_fill = if dark_mode {
        egui::Color32::from_rgb(20, 24, 32)
    } else {
        egui::Color32::from_rgb(248, 250, 253)
    };
    let hover_fill = if dark_mode {
        egui::Color32::from_rgb(24, 29, 38)
    } else {
        egui::Color32::from_rgb(252, 253, 255)
    };
    let idle_stroke = if dark_mode {
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 24)
    } else {
        egui::Color32::from_rgba_unmultiplied(15, 23, 42, 22)
    };
    let hover_stroke = egui::Color32::from_rgba_unmultiplied(
        palette.accent.r(),
        palette.accent.g(),
        palette.accent.b(),
        if dark_mode { 136 } else { 88 },
    );
    let input_width = ui.available_width().max(180.0);

    ui.scope(|ui| {
        let widgets = &mut ui.style_mut().visuals.widgets;
        widgets.inactive.bg_fill = input_fill;
        widgets.inactive.weak_bg_fill = input_fill;
        widgets.inactive.bg_stroke = egui::Stroke::new(1.0, idle_stroke);
        widgets.inactive.rounding = egui::Rounding::same(16.0);

        widgets.hovered = widgets.inactive;
        widgets.hovered.bg_fill = hover_fill;
        widgets.hovered.weak_bg_fill = hover_fill;
        widgets.hovered.bg_stroke = egui::Stroke::new(1.0, hover_stroke);

        widgets.active = widgets.hovered;
        widgets.active.bg_fill = input_fill;
        widgets.active.weak_bg_fill = input_fill;
        widgets.active.bg_stroke = egui::Stroke::new(1.25, palette.accent);
        widgets.open = widgets.active;

        ui.add(
            egui::TextEdit::singleline(value)
                .hint_text(hint)
                .desired_width(input_width)
                .min_size(egui::vec2(input_width, 44.0))
                .margin(egui::Margin::symmetric(14.0, 11.0))
                .vertical_align(egui::Align::Center),
        )
    })
    .inner
}

fn render_segmented_choice(
    ui: &mut egui::Ui,
    selected: &mut String,
    options: &[&str],
    palette: ThemePalette,
) -> bool {
    let mut changed = false;
    egui::Frame::none()
        .fill(palette.soft)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .rounding(egui::Rounding::same(14.0))
        .inner_margin(egui::Margin::same(4.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for option in options {
                    let is_selected = selected.as_str() == *option;
                    let button = egui::Button::new(
                        egui::RichText::new(*option)
                            .color(if is_selected {
                                egui::Color32::WHITE
                            } else {
                                palette.text
                            })
                            .strong(),
                    )
                    .fill(if is_selected {
                        palette.accent
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::new(
                        1.0,
                        if is_selected {
                            palette.accent
                        } else {
                            egui::Color32::TRANSPARENT
                        },
                    ))
                    .rounding(egui::Rounding::same(11.0))
                    .min_size(egui::vec2(72.0, 34.0));
                    if ui.add(button).clicked() && !is_selected {
                        *selected = (*option).to_string();
                        changed = true;
                    }
                }
            });
        });
    changed
}

fn render_segmented_tab_bar(
    ui: &mut egui::Ui,
    selected: &mut BottomTab,
    task_label: &str,
    history_label: &str,
    palette: ThemePalette,
) -> bool {
    let mut changed = false;
    soft_card_frame(palette).show(ui, |ui| {
        let tab_width = ((ui.available_width() - ui.spacing().item_spacing.x) / 2.0).max(120.0);
        ui.horizontal(|ui| {
            let task_selected = *selected == BottomTab::Tasks;
            let task_button = egui::Button::new(
                egui::RichText::new(task_label)
                    .color(if task_selected {
                        egui::Color32::WHITE
                    } else {
                        palette.text
                    })
                    .strong(),
            )
            .fill(if task_selected {
                palette.accent
            } else {
                palette.row
            })
            .stroke(egui::Stroke::new(
                1.0,
                if task_selected {
                    palette.accent
                } else {
                    palette.border
                },
            ))
            .rounding(egui::Rounding::same(14.0))
            .min_size(egui::vec2(tab_width, 40.0));
            if ui.add(task_button).clicked() && !task_selected {
                *selected = BottomTab::Tasks;
                changed = true;
            }

            let history_selected = *selected == BottomTab::History;
            let history_button = egui::Button::new(
                egui::RichText::new(history_label)
                    .color(if history_selected {
                        egui::Color32::WHITE
                    } else {
                        palette.text
                    })
                    .strong(),
            )
            .fill(if history_selected {
                palette.accent
            } else {
                palette.row
            })
            .stroke(egui::Stroke::new(
                1.0,
                if history_selected {
                    palette.accent
                } else {
                    palette.border
                },
            ))
            .rounding(egui::Rounding::same(14.0))
            .min_size(egui::vec2(tab_width, 40.0));
            if ui.add(history_button).clicked() && !history_selected {
                *selected = BottomTab::History;
                changed = true;
            }
        });
    });
    changed
}

fn render_settings_tab_bar(
    ui: &mut egui::Ui,
    selected: &mut SettingsTab,
    palette: ThemePalette,
) -> bool {
    let mut changed = false;
    soft_card_frame(palette).show(ui, |ui| {
        let tab_width = ((ui.available_width() - ui.spacing().item_spacing.x) / 2.0).max(140.0);
        ui.horizontal(|ui| {
            for tab in [SettingsTab::Config, SettingsTab::Appearance] {
                let is_selected = *selected == tab;
                let button = egui::Button::new(
                    egui::RichText::new(tab.label())
                        .color(if is_selected {
                            egui::Color32::WHITE
                        } else {
                            palette.text
                        })
                        .strong(),
                )
                .fill(if is_selected {
                    palette.accent
                } else {
                    palette.row
                })
                .stroke(egui::Stroke::new(
                    1.0,
                    if is_selected {
                        palette.accent
                    } else {
                        palette.border
                    },
                ))
                .rounding(egui::Rounding::same(14.0))
                .min_size(egui::vec2(tab_width, 40.0));
                if ui.add(button).clicked() && !is_selected {
                    *selected = tab;
                    changed = true;
                }
            }
        });
    });
    changed
}

fn render_page_tab_bar(ui: &mut egui::Ui, selected: &mut AppTab, palette: ThemePalette) -> bool {
    let mut changed = false;
    soft_card_frame(palette).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            for tab in [
                AppTab::Overview,
                AppTab::Upload,
                AppTab::Activity,
                AppTab::Settings,
            ] {
                let is_selected = *selected == tab;
                let button = egui::Button::new(
                    egui::RichText::new(tab.label())
                        .color(if is_selected {
                            egui::Color32::WHITE
                        } else {
                            palette.text
                        })
                        .strong(),
                )
                .fill(if is_selected {
                    palette.accent
                } else {
                    palette.row
                })
                .stroke(egui::Stroke::new(
                    1.0,
                    if is_selected {
                        palette.accent
                    } else {
                        palette.border
                    },
                ))
                .rounding(egui::Rounding::same(14.0))
                .min_size(egui::vec2(100.0, 40.0));
                if ui.add(button).clicked() && !is_selected {
                    *selected = tab;
                    changed = true;
                }
            }
        });
    });
    changed
}

fn build_theme_style(theme: egui::Theme, accent: AccentColor) -> egui::Style {
    let palette = theme_palette(theme, accent);
    let mut style = theme.default_style();
    style.override_text_style = Some(egui::TextStyle::Body);
    style.spacing.item_spacing = egui::vec2(12.0, 12.0);
    style.spacing.button_padding = egui::vec2(14.0, 10.0);
    style.spacing.interact_size.y = 38.0;
    style.spacing.window_margin = egui::Margin::same(18.0);
    style.spacing.menu_margin = egui::Margin::same(12.0);
    style.visuals.override_text_color = Some(palette.text);
    style.visuals.panel_fill = palette.bg;
    style.visuals.window_fill = palette.card;
    style.visuals.extreme_bg_color = palette.soft;
    style.visuals.faint_bg_color = palette.row;
    style.visuals.code_bg_color = palette.row;
    style.visuals.window_rounding = egui::Rounding::same(24.0);
    style.visuals.menu_rounding = egui::Rounding::same(18.0);
    style.visuals.window_shadow = egui::Shadow {
        offset: egui::vec2(0.0, 18.0),
        blur: 48.0,
        spread: 0.0,
        color: palette.shadow,
    };
    style.visuals.popup_shadow = egui::Shadow {
        offset: egui::vec2(0.0, 12.0),
        blur: 28.0,
        spread: 0.0,
        color: palette.shadow.gamma_multiply(0.8),
    };
    style.visuals.window_stroke = egui::Stroke::new(1.0, palette.border);
    style.visuals.selection.bg_fill = palette.accent;
    style.visuals.selection.stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
    style.visuals.button_frame = true;
    style.visuals.widgets.noninteractive.rounding = egui::Rounding::same(16.0);
    style.visuals.widgets.noninteractive.bg_fill = palette.card;
    style.visuals.widgets.noninteractive.weak_bg_fill = palette.soft;
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, palette.border);
    style.visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, palette.text);

    style.visuals.widgets.inactive = style.visuals.widgets.noninteractive;
    style.visuals.widgets.inactive.bg_fill = palette.row;
    style.visuals.widgets.inactive.weak_bg_fill = palette.row;

    style.visuals.widgets.hovered = style.visuals.widgets.inactive;
    style.visuals.widgets.hovered.bg_fill = palette.row;
    style.visuals.widgets.hovered.weak_bg_fill = palette.row;
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(
        1.0,
        egui::Color32::from_rgba_unmultiplied(
            palette.accent.r(),
            palette.accent.g(),
            palette.accent.b(),
            if theme == egui::Theme::Dark { 148 } else { 96 },
        ),
    );

    style.visuals.widgets.active = style.visuals.widgets.hovered;
    style.visuals.widgets.active.bg_fill = palette.accent;
    style.visuals.widgets.active.weak_bg_fill = palette.accent;
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, palette.accent);
    style.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
    style.visuals.widgets.open = style.visuals.widgets.hovered;

    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::new(28.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(15.5, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(14.5, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::new(12.5, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::new(13.5, egui::FontFamily::Monospace),
    );

    style
}

fn apply_theme(ctx: &egui::Context, mode: ThemeMode, accent: AccentColor) -> ThemePalette {
    ctx.set_theme(mode.preference());
    ctx.set_style_of(
        egui::Theme::Light,
        build_theme_style(egui::Theme::Light, accent),
    );
    ctx.set_style_of(
        egui::Theme::Dark,
        build_theme_style(egui::Theme::Dark, accent),
    );
    theme_palette(ctx.theme(), accent)
}

fn paint_background(ctx: &egui::Context, palette: ThemePalette) {
    let rect = ctx.screen_rect();
    let painter = ctx.layer_painter(egui::LayerId::background());
    painter.rect_filled(rect, 0.0, palette.bg);
    painter.circle_filled(
        egui::pos2(rect.left() + rect.width() * 0.18, rect.top() + 120.0),
        180.0,
        palette.glow_a,
    );
    painter.circle_filled(
        egui::pos2(rect.right() - 150.0, rect.top() + 80.0),
        140.0,
        palette.glow_b,
    );
    painter.circle_filled(
        egui::pos2(rect.right() - 120.0, rect.bottom() - 100.0),
        220.0,
        if ctx.theme() == egui::Theme::Dark {
            egui::Color32::from_rgba_unmultiplied(52, 199, 89, 26)
        } else {
            egui::Color32::from_rgba_unmultiplied(52, 199, 89, 16)
        },
    );
}

// ── 解析快捷键字符串 ──────────────────────────────────────
fn parse_hotkey(s: &str) -> Option<HotKey> {
    let parts: Vec<&str> = s.split('+').collect();
    if parts.is_empty() {
        return None;
    }
    let key_str = parts.last()?;
    let code = match key_str.to_lowercase().as_str() {
        "a" => Code::KeyA,
        "b" => Code::KeyB,
        "c" => Code::KeyC,
        "d" => Code::KeyD,
        "e" => Code::KeyE,
        "f" => Code::KeyF,
        "g" => Code::KeyG,
        "h" => Code::KeyH,
        "i" => Code::KeyI,
        "j" => Code::KeyJ,
        "k" => Code::KeyK,
        "l" => Code::KeyL,
        "m" => Code::KeyM,
        "n" => Code::KeyN,
        "o" => Code::KeyO,
        "p" => Code::KeyP,
        "q" => Code::KeyQ,
        "r" => Code::KeyR,
        "s" => Code::KeyS,
        "t" => Code::KeyT,
        "u" => Code::KeyU,
        "v" => Code::KeyV,
        "w" => Code::KeyW,
        "x" => Code::KeyX,
        "y" => Code::KeyY,
        "z" => Code::KeyZ,
        "f1" => Code::F1,
        "f2" => Code::F2,
        "f3" => Code::F3,
        "f4" => Code::F4,
        "f5" => Code::F5,
        "f6" => Code::F6,
        _ => return None,
    };
    let mut mods = Modifiers::empty();
    for part in &parts[..parts.len() - 1] {
        match part.to_lowercase().as_str() {
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "shift" => mods |= Modifiers::SHIFT,
            "alt" | "option" => mods |= Modifiers::ALT,
            "super" | "meta" | "cmd" | "command" => mods |= Modifiers::SUPER,
            _ => {}
        }
    }
    Some(HotKey::new(Some(mods), code))
}

// ── 执行上传，只发 TaskProgress（db_insert 在此线程自行 open_db）──
fn do_upload_task(
    client: &Client,
    cfg: &Config,
    tx: &mpsc::Sender<AppEvent>,
    task_id: usize,
    img: Vec<u8>,
) {
    // 1. 构建并发送请求（文件传输阶段）
    let req = match build_upload_request(client, cfg, img) {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(AppEvent::TaskProgress(
                task_id,
                TaskStatus::Failed {
                    message: format!("构建请求失败: {}", e),
                    retryable: false,
                },
            ));
            return;
        }
    };
    let resp = match req.send() {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(AppEvent::TaskProgress(
                task_id,
                TaskStatus::Failed {
                    message: format!("发送请求失败: {}", e),
                    retryable: true,
                },
            ));
            return;
        }
    };
    // 2. 文件已传完，服务端处理中（大模型命名等）
    let _ = tx.send(AppEvent::TaskProgress(task_id, TaskStatus::Processing));
    // 3. 等待服务端返回（阻塞读取响应体）
    match parse_upload_response(cfg, resp) {
        Ok((url, src)) => {
            if let Some(conn) = open_db() {
                db_insert(&conn, &url, &src);
            }
            let _ = tx.send(AppEvent::TaskProgress(
                task_id,
                TaskStatus::Success(url, src),
            ));
        }
        Err(err) => {
            let _ = tx.send(AppEvent::TaskProgress(
                task_id,
                TaskStatus::Failed {
                    message: format!("上传失败: {}", err.message),
                    retryable: err.retryable,
                },
            ));
        }
    }
}

// ── 托盘菜单句柄 ────────────────────────────────────────
struct TrayMenuHandles {
    item_upload: MenuItem,
    item_watch: CheckMenuItem,
    item_status: MenuItem,
    recent_submenu: Submenu,
    recent_items: Vec<MenuItem>,
}

fn build_tray_menu(watch_active: bool) -> (Menu, TrayMenuHandles, MenuItem, MenuItem) {
    let tray_menu = Menu::new();

    let item_upload = MenuItem::new("从剪贴板上传", true, None);
    let _ = tray_menu.append(&item_upload);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());

    let item_watch = CheckMenuItem::new("自动监听剪贴板", true, watch_active, None);
    let _ = tray_menu.append(&item_watch);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());

    let recent_submenu = Submenu::new("最近上传", true);
    let _ = recent_submenu.append(&MenuItem::new("暂无记录", false, None));
    let _ = tray_menu.append(&recent_submenu);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());

    let item_status = MenuItem::new("状态: 空闲", false, None);
    let _ = tray_menu.append(&item_status);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());

    let item_show = MenuItem::new("打开设置窗口", true, None);
    let item_quit = MenuItem::new("退出", true, None);
    let _ = tray_menu.append(&item_show);
    let _ = tray_menu.append(&item_quit);

    let handles = TrayMenuHandles {
        item_upload,
        item_watch,
        item_status,
        recent_submenu,
        recent_items: vec![],
    };
    (tray_menu, handles, item_show, item_quit)
}

// macOS 菜单栏图标推荐 22x22，这里统一使用该尺寸构建 RGBA 托盘图标。
const TRAY_ICON_SIZE: u32 = 22;

fn build_tray_icon_rgba() -> (Vec<u8>, u32, u32) {
    let (icon_w, icon_h) = (TRAY_ICON_SIZE, TRAY_ICON_SIZE);
    let mut icon_rgba = vec![0u8; (icon_w * icon_h * 4) as usize];

    // 使用更适合菜单栏的极简上传 glyph：
    // 上方是向上的箭头，下方是打开的托盘，缩小后比剪贴板细节更清晰。
    const MASK: [&str; TRAY_ICON_SIZE as usize] = [
        "......................",
        "......................",
        "..........##..........",
        ".........####.........",
        "........##..##........",
        ".......##....##.......",
        "..........##..........",
        "..........##..........",
        "..........##..........",
        ".....##...##...##.....",
        "....###...##...###....",
        "....##..........##....",
        "....##..........##....",
        "....##..........##....",
        "....##..........##....",
        "....##..........##....",
        "....##..........##....",
        "....##############....",
        ".....############.....",
        "......................",
        "......................",
        "......................",
    ];

    for (y, row) in MASK.iter().enumerate() {
        debug_assert_eq!(row.len(), icon_w as usize);
        for (x, ch) in row.bytes().enumerate() {
            let alpha = match ch {
                b'#' => 255,
                _ => 0,
            };
            if alpha == 0 {
                continue;
            }
            let idx = ((y as u32 * icon_w + x as u32) * 4) as usize;
            icon_rgba[idx] = 255;
            icon_rgba[idx + 1] = 255;
            icon_rgba[idx + 2] = 255;
            icon_rgba[idx + 3] = alpha;
        }
    }

    (icon_rgba, icon_w, icon_h)
}

fn create_tray_icon(tray_menu: Menu) -> Option<TrayIcon> {
    let (icon_rgba, icon_w, icon_h) = build_tray_icon_rgba();
    let icon = match TrayIconImg::from_rgba(icon_rgba, icon_w, icon_h) {
        Ok(icon) => icon,
        Err(err) => {
            eprintln!(
                "托盘图标 RGBA 数据创建失败 ({}x{}): {}",
                icon_w, icon_h, err
            );
            return None;
        }
    };

    match TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_icon(icon)
        .with_icon_as_template(cfg!(target_os = "macos"))
        .with_tooltip("剪贴板上传工具")
        .build()
    {
        Ok(icon) => Some(icon),
        Err(err) => {
            eprintln!("TrayIcon 创建失败: {}", err);
            None
        }
    }
}

// ── 底部 Tab ──────────────────────────────────────────────
#[derive(Debug, Clone, PartialEq)]
enum BottomTab {
    Tasks,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Config,
    Appearance,
}

impl SettingsTab {
    fn label(self) -> &'static str {
        match self {
            Self::Config => "配置",
            Self::Appearance => "外观",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppTab {
    Overview,
    Upload,
    Activity,
    Settings,
}

impl AppTab {
    fn label(self) -> &'static str {
        match self {
            Self::Overview => "概览",
            Self::Upload => "上传",
            Self::Activity => "活动",
            Self::Settings => "设置",
        }
    }
}

// ── 应用状态 ─────────────────────────────────────────────
struct AppState {
    config: Config,
    shared_config: Arc<Mutex<Config>>,
    last_url: Option<String>,
    status: Option<(bool, String)>,
    status_clear_at: Option<Instant>,
    rx: mpsc::Receiver<AppEvent>,
    tx: mpsc::Sender<AppEvent>,
    watch_active: bool,
    quit_requested: bool,
    history: Vec<HistoryRecord>,
    tasks: Vec<UploadTask>,
    next_task_id: usize,
    active_tab: AppTab,
    settings_tab: SettingsTab,
    bottom_tab: BottomTab,
    config_dirty: bool,
    db: Option<Connection>,
    http_client: Arc<Client>,    // 复用的 HTTP 客户端
    watch_stop: Arc<AtomicBool>, // watch 线程优雅退出信号
    tray_handles: Option<TrayMenuHandles>,
    shared_recent_urls: Arc<Mutex<Vec<String>>>,
}

impl AppState {
    /// 设置会自动消失的状态提示
    fn set_status_auto_clear(&mut self, is_err: bool, msg: String) {
        self.status = Some((is_err, msg));
        self.status_clear_at = Some(Instant::now() + Duration::from_secs(3));
    }

    /// 设置不会自动消失的状态提示
    fn set_status_sticky(&mut self, is_err: bool, msg: String) {
        self.status = Some((is_err, msg));
        self.status_clear_at = None;
    }

    /// 分配任务ID，推入队列，启动上传线程
    fn spawn_task(&mut self, img: Vec<u8>) -> usize {
        let task_id = self.next_task_id;
        self.next_task_id += 1;
        let created_at = Local::now().format("%H:%M:%S").to_string();
        self.tasks.push(UploadTask {
            id: task_id,
            status: TaskStatus::Uploading,
            image_data: img.clone(),
            created_at,
            data_expires_at: Some(Instant::now() + Duration::from_secs(300)), // 5分钟后过期
            retry_count: 0,
        });
        let cfg = self.config.clone();
        let tx = self.tx.clone();
        let client = Arc::clone(&self.http_client);
        thread::spawn(move || do_upload_task(&client, &cfg, &tx, task_id, img));
        task_id
    }

    fn trigger_upload(&mut self) {
        let has_uploading = self.tasks.iter().any(|t| task_is_active(&t.status));
        if has_uploading {
            return;
        }
        let img = match read_clipboard_image() {
            Some(d) => d,
            None => {
                self.set_status_sticky(true, "剪贴板中未检测到图片".to_string());
                return;
            }
        };
        self.status = None;
        self.status_clear_at = None;
        self.last_url = None;
        self.active_tab = AppTab::Activity;
        self.bottom_tab = BottomTab::Tasks;
        self.spawn_task(img);
    }

    fn schedule_auto_retry(&mut self, task_id: usize, message: &str) -> bool {
        if !auto_retry_enabled(&self.config) {
            return false;
        }
        let Some(task) = self.tasks.iter_mut().find(|t| t.id == task_id) else {
            return false;
        };
        if task.retry_count >= AUTO_RETRY_MAX_RETRIES {
            return false;
        }

        let next_retry = task.retry_count + 1;
        let delay = auto_retry_delay_seconds(next_retry);
        task.status = TaskStatus::Retrying {
            message: message.to_string(),
            attempt: next_retry,
            max_retries: AUTO_RETRY_MAX_RETRIES,
            wait_seconds: delay,
        };
        task.data_expires_at = Some(Instant::now() + Duration::from_secs(300));

        let tx = self.tx.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(delay));
            let _ = tx.send(AppEvent::RetryTask(task_id));
        });
        self.set_status_sticky(
            true,
            format!(
                "上传失败，{} 秒后自动重试（{}/{}）",
                delay, next_retry, AUTO_RETRY_MAX_RETRIES
            ),
        );
        true
    }

    fn start_retry_task(&mut self, task_id: usize, manual: bool) {
        // 若 image_data 已清空则重新读剪贴板
        let img = match self.tasks.iter().find(|t| t.id == task_id) {
            Some(t) if !t.image_data.is_empty() => t.image_data.clone(),
            Some(_) => match read_clipboard_image() {
                Some(d) => d,
                None => {
                    self.set_status_sticky(true, "剪贴板中未检测到图片，无法重试".to_string());
                    return;
                }
            },
            None => return,
        };
        self.status = None;
        self.status_clear_at = None;
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task_id) {
            t.status = TaskStatus::Uploading;
            t.image_data = img.clone();
            t.data_expires_at = Some(Instant::now() + Duration::from_secs(300));
            if manual {
                t.retry_count = 0;
            } else {
                t.retry_count = t.retry_count.saturating_add(1);
            }
        }
        let cfg = self.config.clone();
        let tx = self.tx.clone();
        let client = Arc::clone(&self.http_client);
        thread::spawn(move || do_upload_task(&client, &cfg, &tx, task_id, img));
    }

    fn retry_task(&mut self, task_id: usize) {
        self.start_retry_task(task_id, true);
    }

    fn retry_task_auto(&mut self, task_id: usize) {
        self.start_retry_task(task_id, false);
    }

    fn sync_tray_menu(&mut self) {
        let handles = match self.tray_handles.as_mut() {
            Some(h) => h,
            None => return,
        };

        // 同步 watch 勾选状态
        let watch = self.config.auto_watch.unwrap_or(false);
        if handles.item_watch.is_checked() != watch {
            handles.item_watch.set_checked(watch);
        }

        // 同步状态文字
        let has_uploading = self.tasks.iter().any(|t| task_is_active(&t.status));
        let status_text = if has_uploading {
            "状态: 上传中...".to_string()
        } else if let Some((is_err, ref msg)) = self.status {
            if is_err {
                format!("状态: {}", msg)
            } else {
                format!("状态: {}", msg)
            }
        } else {
            "状态: 空闲".to_string()
        };
        let _ = handles.item_status.set_text(&status_text);

        // 同步上传按钮
        handles.item_upload.set_enabled(!has_uploading);
        if has_uploading {
            let _ = handles.item_upload.set_text("上传中...");
        } else {
            let _ = handles.item_upload.set_text("从剪贴板上传");
        }

        // 同步最近上传子菜单
        let recent_urls: Vec<String> = self.history.iter().take(5).map(|r| r.url.clone()).collect();
        let current_texts: Vec<String> = handles
            .recent_items
            .iter()
            .map(|item| item.text())
            .collect();
        let needs_rebuild = current_texts.len() != recent_urls.len()
            || current_texts
                .iter()
                .zip(recent_urls.iter())
                .any(|(a, b)| a != b);

        if needs_rebuild {
            // 更新共享URL数据（供事件线程读取）
            *self.shared_recent_urls.lock().unwrap() = recent_urls.clone();

            // 清除旧的子菜单项
            for item in handles.recent_items.drain(..) {
                let _ = handles.recent_submenu.remove(&item);
            }
            // 清除占位项（如"暂无记录"）
            while handles.recent_submenu.remove_at(0).is_some() {}

            if recent_urls.is_empty() {
                let _ = handles
                    .recent_submenu
                    .append(&MenuItem::new("暂无记录", false, None));
            } else {
                for (i, url) in recent_urls.iter().enumerate() {
                    let display = if url.len() > 50 {
                        format!("{}...", &url[..47])
                    } else {
                        url.clone()
                    };
                    let item = MenuItem::with_id(
                        MenuId::new(format!("recent_url_{}", i)),
                        &display,
                        true,
                        None,
                    );
                    let _ = handles.recent_submenu.append(&item);
                    handles.recent_items.push(item);
                }
            }
        }
    }

    fn render_config_panel(&mut self, ui: &mut egui::Ui, palette: ThemePalette) {
        render_panel_header(ui, "配置", "网络参数、响应解析和自动化偏好。", palette);
        ui.add_space(8.0);

        soft_card_frame(palette).show(ui, |ui| {
            render_setting_stack_row(
                ui,
                "上传地址",
                "图床或上传接口的完整地址。",
                palette,
                |ui| {
                    if render_singleline_input(
                        ui,
                        &mut self.config.upload_url,
                        "https://example.com/upload",
                        palette,
                    )
                    .changed()
                    {
                        self.config_dirty = true;
                    }
                },
            );

            let method = self.config.method.get_or_insert_with(|| "POST".to_string());
            render_setting_row(
                ui,
                "请求方法",
                "常见图床使用 POST，特殊接口可切换到 PUT。",
                palette,
                |ui| {
                    if render_segmented_choice(ui, method, &["POST", "PUT"], palette) {
                        self.config_dirty = true;
                    }
                },
            );

            let file_field = self
                .config
                .file_field
                .get_or_insert_with(|| "file".to_string());
            render_setting_stack_row(
                ui,
                "文件字段",
                "multipart 表单中的图片字段名。",
                palette,
                |ui| {
                    if render_singleline_input(ui, file_field, "file", palette).changed() {
                        self.config_dirty = true;
                    }
                },
            );

            let response = self
                .config
                .response
                .get_or_insert_with(|| "text".to_string());
            render_setting_stack_row(
                ui,
                "响应解析",
                "支持 text、json.url、json.data.link。",
                palette,
                |ui| {
                    if render_singleline_input(ui, response, "text 或 json.data.url", palette)
                        .changed()
                    {
                        self.config_dirty = true;
                    }
                },
            );

            let hotkey = self.config.hotkey.get_or_insert_with(String::new);
            render_setting_stack_row(
                ui,
                "全局快捷键",
                "保存后重启生效，留空则不注册。",
                palette,
                |ui| {
                    if render_singleline_input(ui, hotkey, "ctrl+shift+u", palette).changed() {
                        self.config_dirty = true;
                    }
                },
            );
        });

        ui.add_space(10.0);

        soft_card_frame(palette).show(ui, |ui| {
            let copy = self.config.copy_to_clipboard.get_or_insert(false);
            render_setting_row(
                ui,
                "自动复制",
                "上传成功后立即把链接写入剪贴板。",
                palette,
                |ui| {
                    if apple_toggle_switch(ui, copy).changed() {
                        self.config_dirty = true;
                    }
                },
            );

            let auto_retry = self.config.auto_retry.get_or_insert(true);
            render_setting_row(
                ui,
                "失败自动重试",
                "仅对网络错误、429 和 5xx 响应生效，最多自动重试 2 次。",
                palette,
                |ui| {
                    if apple_toggle_switch(ui, auto_retry).changed() {
                        self.config_dirty = true;
                    }
                },
            );

            let notify = self.config.notify_on_success.get_or_insert(false);
            render_setting_row(
                ui,
                "系统通知",
                "上传完成后显示桌面通知。",
                palette,
                |ui| {
                    if apple_toggle_switch(ui, notify).changed() {
                        self.config_dirty = true;
                    }
                },
            );

            let close_tray = self.config.close_to_tray.get_or_insert(true);
            render_setting_row(
                ui,
                "关闭窗口收起到托盘",
                "关闭主窗口时保持后台运行。",
                palette,
                |ui| {
                    if apple_toggle_switch(ui, close_tray).changed() {
                        self.config_dirty = true;
                    }
                },
            );

            let mut watch_changed = false;
            {
                let watch = self.config.auto_watch.get_or_insert(false);
                render_setting_row(
                    ui,
                    "自动监听剪贴板",
                    "检测到新的截图后直接发起上传。",
                    palette,
                    |ui| {
                        ui.horizontal(|ui| {
                            if apple_toggle_switch(ui, watch).changed() {
                                self.watch_active = *watch;
                                self.config_dirty = true;
                                watch_changed = true;
                            }
                            render_pill(
                                ui,
                                if self.watch_active {
                                    "运行中"
                                } else {
                                    "未开启"
                                },
                                if self.watch_active {
                                    apple_green().gamma_multiply(0.12)
                                } else {
                                    egui::Color32::from_rgba_unmultiplied(111, 118, 132, 24)
                                },
                                if self.watch_active {
                                    apple_green()
                                } else {
                                    palette.muted
                                },
                            );
                        });
                    },
                );
            }
            if watch_changed {
                *self.shared_config.lock().unwrap() = self.config.clone();
            }
        });
    }

    fn render_control_panel(
        &mut self,
        ui: &mut egui::Ui,
        palette: ThemePalette,
        has_uploading: bool,
        success_count: usize,
        close_to_tray: bool,
        auto_copy: bool,
        notify_on_success: bool,
    ) {
        render_panel_header(
            ui,
            "上传中心",
            "保存配置、查看状态，并快速执行上传。",
            palette,
        );
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let save_text = if self.config_dirty {
                "保存更改"
            } else {
                "保存配置"
            };
            if ui
                .add(secondary_button(save_text, palette).min_size(egui::vec2(120.0, 42.0)))
                .clicked()
            {
                *self.shared_config.lock().unwrap() = self.config.clone();
                match save_config(&self.config) {
                    Ok(_) => {
                        self.config_dirty = false;
                        self.set_status_auto_clear(false, "配置已保存".to_string());
                    }
                    Err(e) => self.set_status_sticky(true, format!("保存失败: {}", e)),
                }
            }

            let upload_text = if has_uploading {
                "上传中..."
            } else {
                "从剪贴板上传"
            };
            if ui
                .add_enabled(
                    !has_uploading,
                    primary_button(upload_text, palette).min_size(egui::vec2(160.0, 42.0)),
                )
                .clicked()
            {
                self.trigger_upload();
            }
        });

        let (banner_fill, banner_stroke, banner_icon, banner_title, banner_detail) =
            if let Some((is_err, msg)) = &self.status {
                if *is_err {
                    (
                        apple_red().gamma_multiply(0.10),
                        apple_red().gamma_multiply(0.35),
                        "失败",
                        msg.clone(),
                        "请检查网络、响应解析规则或上传地址。",
                    )
                } else {
                    (
                        apple_green().gamma_multiply(0.10),
                        apple_green().gamma_multiply(0.35),
                        "完成",
                        msg.clone(),
                        "状态提示会在几秒后自动清除。",
                    )
                }
            } else if has_uploading {
                (
                    palette.accent.gamma_multiply(0.12),
                    palette.accent.gamma_multiply(0.35),
                    "处理中",
                    "正在上传新的剪贴板图片".to_string(),
                    "文件已读取，等待远端完成响应。",
                )
            } else {
                (
                    palette.row,
                    palette.border,
                    "待命",
                    "应用已就绪".to_string(),
                    "截图后可以直接上传，也可以交给自动监听。",
                )
            };

        egui::Frame::none()
            .fill(banner_fill)
            .stroke(egui::Stroke::new(1.0, banner_stroke))
            .rounding(egui::Rounding::same(18.0))
            .inner_margin(egui::Margin::same(14.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    render_pill(ui, banner_icon, banner_fill, banner_stroke);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(banner_title)
                                .color(palette.text)
                                .strong(),
                        );
                        ui.label(
                            egui::RichText::new(banner_detail)
                                .size(12.5)
                                .color(palette.muted),
                        );
                    });
                });
            });

        match self.last_url.clone() {
            Some(url) => {
                soft_card_frame(palette).show(ui, |ui| {
                    render_panel_header(ui, "最近链接", "最近一次成功上传的地址。", palette);
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.hyperlink_to("打开", &url);
                            if ui.add(secondary_button("复制", palette)).clicked()
                                && copy_text_to_clipboard(&url)
                            {
                                self.set_status_auto_clear(false, "已复制到剪贴板".to_string());
                            }
                        });
                    });
                    ui.label(egui::RichText::new(url).monospace().color(palette.accent));
                });
            }
            None => {
                soft_card_frame(palette).show(ui, |ui| {
                    render_panel_header(ui, "最近链接", "最近一次成功上传的地址。", palette);
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("还没有成功上传的链接，下一次上传成功后会显示在这里。")
                            .size(12.5)
                            .color(palette.muted),
                    );
                });
            }
        }

        soft_card_frame(palette).show(ui, |ui| {
            render_panel_header(ui, "偏好状态", "当前桌面行为的摘要。", palette);
            ui.add_space(6.0);
            render_summary_row(
                ui,
                "自动复制",
                if auto_copy { "开启" } else { "关闭" },
                if auto_copy {
                    apple_green()
                } else {
                    palette.muted
                },
                palette,
            );
            render_summary_row(
                ui,
                "系统通知",
                if notify_on_success {
                    "开启"
                } else {
                    "关闭"
                },
                if notify_on_success {
                    palette.accent
                } else {
                    palette.muted
                },
                palette,
            );
            render_summary_row(
                ui,
                "关闭窗口",
                if close_to_tray {
                    "收起到托盘"
                } else {
                    "直接退出"
                },
                apple_orange(),
                palette,
            );
            render_summary_row(
                ui,
                "已完成任务",
                &success_count.to_string(),
                apple_green(),
                palette,
            );
        });
    }

    fn render_appearance_panel(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        palette: ThemePalette,
    ) {
        render_panel_header(ui, "外观", "主题模式、主题色与系统外观同步。", palette);
        ui.add_space(8.0);

        soft_card_frame(palette).show(ui, |ui| {
            let mut theme_mode = config_theme_mode(&self.config);
            render_setting_row(
                ui,
                "主题模式",
                "支持跟随系统、浅色与深色三种模式。",
                palette,
                |ui| {
                    let mut selected = theme_mode.label().to_string();
                    if render_segmented_choice(
                        ui,
                        &mut selected,
                        &[
                            ThemeMode::System.label(),
                            ThemeMode::Light.label(),
                            ThemeMode::Dark.label(),
                        ],
                        palette,
                    ) {
                        theme_mode = match selected.as_str() {
                            "浅色" => ThemeMode::Light,
                            "深色" => ThemeMode::Dark,
                            _ => ThemeMode::System,
                        };
                        self.config.theme_mode = Some(theme_mode);
                        self.config_dirty = true;
                    }
                },
            );

            let system_theme_text = match ctx.system_theme() {
                Some(egui::Theme::Dark) => "系统当前为深色",
                Some(egui::Theme::Light) => "系统当前为浅色",
                None => "当前平台没有提供系统主题信息",
            };
            render_setting_row(ui, "系统主题", system_theme_text, palette, |ui| {
                render_pill(
                    ui,
                    system_theme_text,
                    accent_color_soft(config_accent_color(&self.config), ui.visuals().dark_mode),
                    palette.accent,
                );
            });
        });

        ui.add_space(10.0);

        soft_card_frame(palette).show(ui, |ui| {
            let mut accent = config_accent_color(&self.config);
            render_setting_row(
                ui,
                "主题色",
                "切换按钮、高亮和选中状态的主色。",
                palette,
                |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for candidate in [
                            AccentColor::Blue,
                            AccentColor::Green,
                            AccentColor::Orange,
                            AccentColor::Pink,
                            AccentColor::Purple,
                        ] {
                            let is_selected = accent == candidate;
                            let candidate_color = accent_color(candidate);
                            let button = egui::Button::new(
                                egui::RichText::new(candidate.label())
                                    .color(if is_selected {
                                        egui::Color32::WHITE
                                    } else {
                                        palette.text
                                    })
                                    .strong(),
                            )
                            .fill(if is_selected {
                                candidate_color
                            } else {
                                accent_color_soft(candidate, ui.visuals().dark_mode)
                            })
                            .stroke(egui::Stroke::new(
                                1.0,
                                if is_selected {
                                    candidate_color
                                } else {
                                    palette.border
                                },
                            ))
                            .rounding(egui::Rounding::same(999.0))
                            .min_size(egui::vec2(92.0, 34.0));
                            if ui.add(button).clicked() && !is_selected {
                                accent = candidate;
                                self.config.accent_color = Some(candidate);
                                self.config_dirty = true;
                            }
                        }
                    });
                },
            );
        });
    }

    fn render_activity_page(
        &mut self,
        ui: &mut egui::Ui,
        palette: ThemePalette,
        uploading_count: usize,
        failed_count: usize,
    ) {
        panel_card_frame(palette).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    render_panel_header(ui, "活动", "查看当前上传任务和最近历史。", palette);
                });
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| match self.bottom_tab {
                        BottomTab::Tasks => {
                            if !self.tasks.is_empty()
                                && ui.add(secondary_button("清空已完成", palette)).clicked()
                            {
                                self.tasks.retain(|t| task_is_active(&t.status));
                            }
                        }
                        BottomTab::History => {
                            if !self.history.is_empty()
                                && ui.add(secondary_button("清空历史", palette)).clicked()
                            {
                                if let Some(ref conn) = self.db {
                                    db_clear(conn);
                                }
                                self.history.clear();
                            }
                        }
                    },
                );
            });

            let task_label = if uploading_count > 0 {
                format!("任务 {}", uploading_count)
            } else if failed_count > 0 {
                format!("任务 {} 失败", failed_count)
            } else {
                format!("任务 {}", self.tasks.len())
            };
            let history_label = format!("历史 {}", self.history.len());

            if render_segmented_tab_bar(
                ui,
                &mut self.bottom_tab,
                &task_label,
                &history_label,
                palette,
            ) && self.bottom_tab == BottomTab::History
            {
                if let Some(ref conn) = self.db {
                    self.history = db_load(conn, 50);
                }
            }

            match self.bottom_tab {
                BottomTab::Tasks => {
                    if self.tasks.is_empty() {
                        soft_card_frame(palette).show(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.label(egui::RichText::new("📷").size(28.0));
                                ui.label(
                                    egui::RichText::new("还没有上传任务")
                                        .size(18.0)
                                        .color(palette.text)
                                        .strong(),
                                );
                                ui.label(
                                    egui::RichText::new(
                                        "截图后点击上传，或开启自动监听让应用自动接手。",
                                    )
                                    .size(13.0)
                                    .color(palette.muted),
                                );
                            });
                        });
                    } else {
                        let mut retry_id: Option<usize> = None;
                        let mut remove_id: Option<usize> = None;
                        let mut copied_task_url: Option<String> = None;

                        for task in self.tasks.iter().rev() {
                            let (badge_text, badge_fill, badge_color, card_fill, card_stroke) =
                                match &task.status {
                                    TaskStatus::Uploading => {
                                        (
                                            "上传中",
                                            apple_orange().gamma_multiply(
                                                if ui.visuals().dark_mode { 0.25 } else { 0.12 },
                                            ),
                                            apple_orange(),
                                            if ui.visuals().dark_mode {
                                                egui::Color32::from_rgb(53, 39, 25)
                                            } else {
                                                egui::Color32::from_rgb(255, 250, 242)
                                            },
                                            apple_orange().gamma_multiply(0.30),
                                        )
                                    }
                                    TaskStatus::Processing => (
                                        "处理中",
                                        accent_color_soft(
                                            config_accent_color(&self.config),
                                            ui.visuals().dark_mode,
                                        ),
                                        palette.accent,
                                        if ui.visuals().dark_mode {
                                            egui::Color32::from_rgb(25, 39, 55)
                                        } else {
                                            egui::Color32::from_rgb(243, 249, 255)
                                        },
                                        palette.accent.gamma_multiply(0.30),
                                    ),
                                    TaskStatus::Success(_, _) => {
                                        (
                                            "已完成",
                                            apple_green().gamma_multiply(
                                                if ui.visuals().dark_mode { 0.25 } else { 0.12 },
                                            ),
                                            apple_green(),
                                            if ui.visuals().dark_mode {
                                                egui::Color32::from_rgb(23, 43, 31)
                                            } else {
                                                egui::Color32::from_rgb(244, 252, 246)
                                            },
                                            apple_green().gamma_multiply(0.30),
                                        )
                                    }
                                    TaskStatus::Retrying { .. } => (
                                        "自动重试",
                                        apple_orange().gamma_multiply(if ui.visuals().dark_mode {
                                            0.25
                                        } else {
                                            0.12
                                        }),
                                        apple_orange(),
                                        if ui.visuals().dark_mode {
                                            egui::Color32::from_rgb(53, 39, 25)
                                        } else {
                                            egui::Color32::from_rgb(255, 250, 242)
                                        },
                                        apple_orange().gamma_multiply(0.30),
                                    ),
                                    TaskStatus::Failed { .. } => (
                                        "失败",
                                        apple_red().gamma_multiply(if ui.visuals().dark_mode {
                                            0.25
                                        } else {
                                            0.12
                                        }),
                                        apple_red(),
                                        if ui.visuals().dark_mode {
                                            egui::Color32::from_rgb(57, 28, 31)
                                        } else {
                                            egui::Color32::from_rgb(255, 245, 245)
                                        },
                                        apple_red().gamma_multiply(0.30),
                                    ),
                                };

                            egui::Frame::none()
                                .fill(card_fill)
                                .stroke(egui::Stroke::new(1.0, card_stroke))
                                .rounding(egui::Rounding::same(18.0))
                                .inner_margin(egui::Margin::same(14.0))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        render_pill(ui, badge_text, badge_fill, badge_color);
                                        ui.label(
                                            egui::RichText::new(&task.created_at)
                                                .size(12.5)
                                                .color(palette.muted),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| match &task.status {
                                                TaskStatus::Success(url, _) => {
                                                    if ui.small_button("移除").clicked() {
                                                        remove_id = Some(task.id);
                                                    }
                                                    if ui.small_button("复制").clicked() {
                                                        copied_task_url = Some(url.clone());
                                                    }
                                                    ui.hyperlink_to("打开", url);
                                                }
                                                TaskStatus::Retrying { .. } => {
                                                    if ui.small_button("移除").clicked() {
                                                        remove_id = Some(task.id);
                                                    }
                                                }
                                                TaskStatus::Failed { .. } => {
                                                    if ui.small_button("移除").clicked() {
                                                        remove_id = Some(task.id);
                                                    }
                                                    if ui.small_button("重试").clicked() {
                                                        retry_id = Some(task.id);
                                                    }
                                                }
                                                TaskStatus::Uploading | TaskStatus::Processing => {}
                                            },
                                        );
                                    });

                                    match &task.status {
                                        TaskStatus::Uploading => {
                                            ui.horizontal(|ui| {
                                                ui.spinner();
                                                ui.label(
                                                    egui::RichText::new(
                                                        "正在上传剪贴板图片到服务器。",
                                                    )
                                                    .color(palette.muted),
                                                );
                                            });
                                        }
                                        TaskStatus::Processing => {
                                            ui.horizontal(|ui| {
                                                ui.spinner();
                                                ui.label(
                                                    egui::RichText::new(
                                                        "文件已发送完成，正在等待服务端处理结果。",
                                                    )
                                                    .color(palette.muted),
                                                );
                                            });
                                        }
                                        TaskStatus::Retrying {
                                            message,
                                            attempt,
                                            max_retries,
                                            wait_seconds,
                                        } => {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "最近一次失败: {}",
                                                    message
                                                ))
                                                .size(13.0)
                                                .color(apple_orange()),
                                            );
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "将在 {} 秒后自动重试（第 {}/{} 次）。",
                                                    wait_seconds, attempt, max_retries
                                                ))
                                                .size(12.5)
                                                .color(palette.muted),
                                            );
                                        }
                                        TaskStatus::Success(url, src) => {
                                            ui.label(
                                                egui::RichText::new(url)
                                                    .monospace()
                                                    .color(palette.accent),
                                            );
                                            if !src.is_empty() {
                                                ui.label(
                                                    egui::RichText::new(format!("src: {}", src))
                                                        .size(12.5)
                                                        .color(palette.muted),
                                                );
                                            }
                                        }
                                        TaskStatus::Failed { message, retryable } => {
                                            ui.label(
                                                egui::RichText::new(message)
                                                    .size(13.0)
                                                    .color(apple_red()),
                                            );
                                            if !retryable {
                                                ui.label(
                                                    egui::RichText::new(
                                                        "这类错误不会自动重试，请检查上传地址、请求头或响应解析配置。",
                                                    )
                                                    .size(12.0)
                                                    .color(palette.muted),
                                                );
                                            }
                                            if task.image_data.is_empty() {
                                                ui.label(
                                                    egui::RichText::new(
                                                        "图片缓存已释放，重试时会重新读取剪贴板。",
                                                    )
                                                    .size(12.0)
                                                    .color(palette.muted),
                                                );
                                            }
                                        }
                                    }
                                });
                        }

                        if let Some(url) = copied_task_url {
                            if copy_text_to_clipboard(&url) {
                                self.set_status_auto_clear(false, "已复制任务链接".to_string());
                            }
                        }
                        if let Some(id) = remove_id {
                            self.tasks.retain(|t| t.id != id);
                        }
                        if let Some(id) = retry_id {
                            self.retry_task(id);
                        }
                    }
                }
                BottomTab::History => {
                    if self.history.is_empty() {
                        soft_card_frame(palette).show(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.label(egui::RichText::new("🕘").size(28.0));
                                ui.label(
                                    egui::RichText::new("还没有上传历史")
                                        .size(18.0)
                                        .color(palette.text)
                                        .strong(),
                                );
                                ui.label(
                                    egui::RichText::new("成功上传后，这里会保留最近的链接记录。")
                                        .size(13.0)
                                        .color(palette.muted),
                                );
                            });
                        });
                    } else {
                        let mut to_delete: Option<i64> = None;
                        let mut copied_history_url: Option<String> = None;

                        for record in &self.history {
                            egui::Frame::none()
                                .fill(palette.row)
                                .stroke(egui::Stroke::new(1.0, palette.border))
                                .rounding(egui::Rounding::same(18.0))
                                .inner_margin(egui::Margin::same(14.0))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        render_pill(
                                            ui,
                                            "已上传",
                                            apple_green().gamma_multiply(
                                                if ui.visuals().dark_mode { 0.25 } else { 0.12 },
                                            ),
                                            apple_green(),
                                        );
                                        ui.label(
                                            egui::RichText::new(&record.uploaded_at)
                                                .size(12.5)
                                                .color(palette.muted),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui.small_button("删除").clicked() {
                                                    to_delete = Some(record.id);
                                                }
                                                if ui.small_button("复制").clicked() {
                                                    copied_history_url = Some(record.url.clone());
                                                }
                                                ui.hyperlink_to("打开", &record.url);
                                            },
                                        );
                                    });
                                    ui.label(
                                        egui::RichText::new(&record.url)
                                            .monospace()
                                            .color(palette.accent),
                                    );
                                    if !record.src.is_empty() {
                                        ui.label(
                                            egui::RichText::new(format!("src: {}", record.src))
                                                .size(12.5)
                                                .color(palette.muted),
                                        );
                                    }
                                });
                        }

                        if let Some(url) = copied_history_url {
                            if copy_text_to_clipboard(&url) {
                                self.set_status_auto_clear(false, "已复制历史链接".to_string());
                            }
                        }
                        if let Some(id) = to_delete {
                            if let Some(ref conn) = self.db {
                                db_delete(conn, id);
                            }
                            self.history.retain(|r| r.id != id);
                        }
                    }
                }
            }
        });
    }

    fn start_watch_thread(&self, ctx: egui::Context) {
        let tx = self.tx.clone();
        let shared = Arc::clone(&self.shared_config);
        let stop = Arc::clone(&self.watch_stop);
        thread::spawn(move || {
            let mut last_fp: u64 = 0;
            while !stop.load(Ordering::Relaxed) {
                let cfg = shared.lock().unwrap().clone();
                let auto_watch = cfg.auto_watch.unwrap_or(false);
                thread::sleep(Duration::from_millis(if auto_watch { 500 } else { 2000 }));
                if !auto_watch {
                    continue;
                }
                // 轻量指纹检查：只读原始 RGBA bytes，不做 PNG 编码
                if let Some(fp) = clipboard_raw_fingerprint() {
                    if fp != last_fp {
                        last_fp = fp;
                        // 指纹变了，才做完整 PNG 编码
                        if let Some(img) = read_clipboard_image() {
                            let _ = tx.send(AppEvent::WatchUpload(img));
                            ctx.request_repaint(); // 主动触发 UI 刷新
                        }
                    }
                }
            }
        });
    }
}

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 检查状态提示是否已过期
        if let Some(clear_at) = self.status_clear_at {
            if Instant::now() >= clear_at {
                self.status = None;
                self.status_clear_at = None;
            }
        }

        // 清理过期的失败任务图片数据（释放内存）
        for task in self.tasks.iter_mut() {
            if let Some(exp) = task.data_expires_at {
                if Instant::now() > exp && !task.image_data.is_empty() {
                    task.image_data = Vec::new();
                    task.data_expires_at = None;
                }
            }
        }

        // 接收后台事件
        while let Ok(event) = self.rx.try_recv() {
            match event {
                AppEvent::TaskProgress(task_id, status) => {
                    match &status {
                        TaskStatus::Success(url, src) => {
                            let url = url.clone();
                            let src = src.clone();
                            if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task_id) {
                                t.status = TaskStatus::Success(url.clone(), src.clone());
                                t.image_data = Vec::new(); // 释放内存
                                t.retry_count = 0;
                            }
                            self.last_url = Some(url.clone());
                            let auto_copy = self.config.copy_to_clipboard.unwrap_or(false);
                            if auto_copy {
                                copy_text_to_clipboard(&url);
                            }
                            if self.config.notify_on_success.unwrap_or(false) {
                                send_notification("上传成功", &url);
                            }
                            let msg = if auto_copy {
                                "上传成功，链接已复制".to_string()
                            } else {
                                "上传成功".to_string()
                            };
                            self.set_status_auto_clear(false, msg);
                            if let Some(ref conn) = self.db {
                                self.history = db_load(conn, 50);
                            }
                        }
                        TaskStatus::Failed { message, retryable } => {
                            let message = message.clone();
                            let retryable = *retryable;
                            if !retryable || !self.schedule_auto_retry(task_id, &message) {
                                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task_id) {
                                    t.status = TaskStatus::Failed {
                                        message: message.clone(),
                                        retryable,
                                    };
                                    // 保留 image_data 供重试
                                }
                                self.set_status_sticky(true, message);
                            }
                        }
                        TaskStatus::Uploading => {
                            if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task_id) {
                                t.status = TaskStatus::Uploading;
                            }
                        }
                        TaskStatus::Processing => {
                            if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task_id) {
                                t.status = TaskStatus::Processing;
                                t.image_data = Vec::new(); // 文件已传完，释放内存
                            }
                        }
                        TaskStatus::Retrying { .. } => {}
                    }
                }
                AppEvent::WatchUpload(img) => {
                    self.active_tab = AppTab::Activity;
                    self.bottom_tab = BottomTab::Tasks;
                    self.spawn_task(img);
                }
                AppEvent::RetryTask(task_id) => {
                    self.retry_task_auto(task_id);
                }
                AppEvent::TrayShowWindow => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                }
                AppEvent::TrayUpload => {
                    self.trigger_upload();
                }
                AppEvent::TrayToggleWatch => {
                    let cur = self.config.auto_watch.unwrap_or(false);
                    self.config.auto_watch = Some(!cur);
                    *self.shared_config.lock().unwrap() = self.config.clone();
                    self.watch_active = !cur;
                }
                AppEvent::TrayQuit => {
                    self.quit_requested = true;
                }
                AppEvent::TrayCopyUrl(url) => {
                    if copy_text_to_clipboard(&url) {
                        self.set_status_auto_clear(false, format!("已复制: {}", url));
                    }
                }
            }
            ctx.request_repaint();
        }

        // ── 同步托盘菜单状态 ──
        self.sync_tray_menu();

        // ── 关闭按钮拦截：最小化到托盘而非退出 ──
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.quit_requested {
                // 来自托盘菜单"退出"，允许真正关闭
            } else if self.config.close_to_tray.unwrap_or(true) {
                // 拦截关闭，隐藏窗口到托盘
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                return;
            }
        }

        if self.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let has_uploading = self.tasks.iter().any(|t| task_is_active(&t.status));

        // 智能降频：有任务时快速刷新，空闲时大幅降频（靠后台事件 ctx.request_repaint() 驱动）
        if has_uploading || self.status_clear_at.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        } else {
            ctx.request_repaint_after(Duration::from_secs(2));
        }

        let uploading_count = self
            .tasks
            .iter()
            .filter(|t| task_is_active(&t.status))
            .count();
        let failed_count = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Failed { .. }))
            .count();
        let success_count = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Success(_, _)))
            .count();
        let watch_enabled = self.config.auto_watch.unwrap_or(false);
        let close_to_tray = self.config.close_to_tray.unwrap_or(true);
        let auto_copy = self.config.copy_to_clipboard.unwrap_or(false);
        let notify_on_success = self.config.notify_on_success.unwrap_or(false);
        let theme_mode = config_theme_mode(&self.config);
        let accent_choice = config_accent_color(&self.config);
        let palette = apply_theme(ctx, theme_mode, accent_choice);

        paint_background(ctx, palette);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(palette.bg)
                    .inner_margin(egui::Margin::same(18.0)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("page_scroll")
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(18.0, 18.0);

                        panel_card_frame(palette).show(ui, |ui| {
                            let hero_wide = ui.available_width() > 680.0;
                            if hero_wide {
                                ui.columns(2, |columns| {
                                    columns[0].vertical(|ui| {
                                        render_window_controls(ui);
                                        ui.add_space(8.0);
                                        ui.label(
                                            egui::RichText::new("剪贴板上传")
                                                .size(31.0)
                                                .color(palette.text)
                                                .strong(),
                                        );
                                        ui.label(
                                            egui::RichText::new(
                                                "多页签工作台，支持浅色、深色和跟随系统。",
                                            )
                                            .size(14.5)
                                            .color(palette.muted),
                                        );
                                        ui.add_space(6.0);
                                        let upload_badge_text = if has_uploading {
                                            format!("{} 个任务处理中", uploading_count)
                                        } else {
                                            "准备就绪".to_string()
                                        };
                                        ui.horizontal_wrapped(|ui| {
                                            render_pill(
                                                ui,
                                                &upload_badge_text,
                                                if has_uploading {
                                                    accent_color_soft(
                                                        accent_choice,
                                                        ui.visuals().dark_mode,
                                                    )
                                                } else {
                                                    apple_green().gamma_multiply(
                                                        if ui.visuals().dark_mode {
                                                            0.25
                                                        } else {
                                                            0.12
                                                        },
                                                    )
                                                },
                                                if has_uploading {
                                                    palette.accent
                                                } else {
                                                    apple_green()
                                                },
                                            );
                                            render_pill(
                                                ui,
                                                if watch_enabled {
                                                    "自动监听开启"
                                                } else {
                                                    "自动监听关闭"
                                                },
                                                if watch_enabled {
                                                    apple_green().gamma_multiply(
                                                        if ui.visuals().dark_mode {
                                                            0.25
                                                        } else {
                                                            0.12
                                                        },
                                                    )
                                                } else {
                                                    egui::Color32::from_rgba_unmultiplied(
                                                        111,
                                                        118,
                                                        132,
                                                        if ui.visuals().dark_mode {
                                                            46
                                                        } else {
                                                            24
                                                        },
                                                    )
                                                },
                                                if watch_enabled {
                                                    apple_green()
                                                } else {
                                                    palette.muted
                                                },
                                            );
                                            render_pill(
                                                ui,
                                                if self.config_dirty {
                                                    "配置待保存"
                                                } else {
                                                    "配置已保存"
                                                },
                                                apple_orange().gamma_multiply(
                                                    if ui.visuals().dark_mode {
                                                        0.25
                                                    } else {
                                                        0.12
                                                    },
                                                ),
                                                if self.config_dirty {
                                                    apple_orange()
                                                } else {
                                                    palette.text
                                                },
                                            );
                                        });
                                    });
                                    columns[1].vertical(|ui| {
                                        render_panel_header(
                                            ui,
                                            "概览",
                                            "应用当前运行状态。",
                                            palette,
                                        );
                                        ui.add_space(4.0);
                                        ui.columns(3, |stats| {
                                            render_metric_tile(
                                                &mut stats[0],
                                                &self.tasks.len().to_string(),
                                                "任务",
                                                palette.accent,
                                                palette,
                                            );
                                            render_metric_tile(
                                                &mut stats[1],
                                                &self.history.len().to_string(),
                                                "历史",
                                                apple_green(),
                                                palette,
                                            );
                                            render_metric_tile(
                                                &mut stats[2],
                                                if watch_enabled { "ON" } else { "OFF" },
                                                "监听",
                                                apple_orange(),
                                                palette,
                                            );
                                        });
                                    });
                                });
                            } else {
                                ui.vertical(|ui| {
                                    render_window_controls(ui);
                                    ui.add_space(8.0);
                                    ui.label(
                                        egui::RichText::new("剪贴板上传")
                                            .size(31.0)
                                            .color(palette.text)
                                            .strong(),
                                    );
                                    ui.label(
                                        egui::RichText::new(
                                            "多页签工作台，支持浅色、深色和跟随系统。",
                                        )
                                        .size(14.5)
                                        .color(palette.muted),
                                    );
                                    ui.horizontal_wrapped(|ui| {
                                        render_pill(
                                            ui,
                                            if has_uploading {
                                                "正在处理任务"
                                            } else {
                                                "准备就绪"
                                            },
                                            if has_uploading {
                                                accent_color_soft(
                                                    accent_choice,
                                                    ui.visuals().dark_mode,
                                                )
                                            } else {
                                                apple_green().gamma_multiply(
                                                    if ui.visuals().dark_mode {
                                                        0.25
                                                    } else {
                                                        0.12
                                                    },
                                                )
                                            },
                                            if has_uploading {
                                                palette.accent
                                            } else {
                                                apple_green()
                                            },
                                        );
                                        render_pill(
                                            ui,
                                            if self.config_dirty {
                                                "配置待保存"
                                            } else {
                                                "配置已保存"
                                            },
                                            apple_orange().gamma_multiply(
                                                if ui.visuals().dark_mode { 0.25 } else { 0.12 },
                                            ),
                                            if self.config_dirty {
                                                apple_orange()
                                            } else {
                                                palette.text
                                            },
                                        );
                                    });
                                    ui.columns(3, |stats| {
                                        render_metric_tile(
                                            &mut stats[0],
                                            &self.tasks.len().to_string(),
                                            "任务",
                                            palette.accent,
                                            palette,
                                        );
                                        render_metric_tile(
                                            &mut stats[1],
                                            &self.history.len().to_string(),
                                            "历史",
                                            apple_green(),
                                            palette,
                                        );
                                        render_metric_tile(
                                            &mut stats[2],
                                            if watch_enabled { "ON" } else { "OFF" },
                                            "监听",
                                            apple_orange(),
                                            palette,
                                        );
                                    });
                                });
                            }
                        });

                        let _ = render_page_tab_bar(ui, &mut self.active_tab, palette);

                        match self.active_tab {
                            AppTab::Overview => {
                                let stacked_cards = ui.available_width() <= 760.0;
                                if stacked_cards {
                                    panel_card_frame(palette).show(ui, |ui| {
                                        self.render_control_panel(
                                            ui,
                                            palette,
                                            has_uploading,
                                            success_count,
                                            close_to_tray,
                                            auto_copy,
                                            notify_on_success,
                                        );
                                    });
                                    panel_card_frame(palette).show(ui, |ui| {
                                        render_panel_header(
                                            ui,
                                            "总览",
                                            "关键状态和快捷入口。",
                                            palette,
                                        );
                                        ui.add_space(8.0);
                                        soft_card_frame(palette).show(ui, |ui| {
                                            render_summary_row(
                                                ui,
                                                "当前主题",
                                                theme_mode.label(),
                                                palette.accent,
                                                palette,
                                            );
                                            render_summary_row(
                                                ui,
                                                "主题色",
                                                accent_choice.label(),
                                                palette.accent,
                                                palette,
                                            );
                                            render_summary_row(
                                                ui,
                                                "失败任务",
                                                &failed_count.to_string(),
                                                apple_red(),
                                                palette,
                                            );
                                        });
                                        ui.horizontal(|ui| {
                                            if ui
                                                .add(secondary_button("前往活动", palette))
                                                .clicked()
                                            {
                                                self.active_tab = AppTab::Activity;
                                            }
                                            if ui
                                                .add(secondary_button("前往设置", palette))
                                                .clicked()
                                            {
                                                self.active_tab = AppTab::Settings;
                                                self.settings_tab = SettingsTab::Config;
                                            }
                                        });
                                    });
                                } else {
                                    ui.columns(2, |columns| {
                                        panel_card_frame(palette).show(&mut columns[0], |ui| {
                                            self.render_control_panel(
                                                ui,
                                                palette,
                                                has_uploading,
                                                success_count,
                                                close_to_tray,
                                                auto_copy,
                                                notify_on_success,
                                            );
                                        });
                                        panel_card_frame(palette).show(&mut columns[1], |ui| {
                                            render_panel_header(
                                                ui,
                                                "总览",
                                                "关键状态和快捷入口。",
                                                palette,
                                            );
                                            ui.add_space(8.0);
                                            soft_card_frame(palette).show(ui, |ui| {
                                                render_summary_row(
                                                    ui,
                                                    "当前主题",
                                                    theme_mode.label(),
                                                    palette.accent,
                                                    palette,
                                                );
                                                render_summary_row(
                                                    ui,
                                                    "主题色",
                                                    accent_choice.label(),
                                                    palette.accent,
                                                    palette,
                                                );
                                                render_summary_row(
                                                    ui,
                                                    "失败任务",
                                                    &failed_count.to_string(),
                                                    apple_red(),
                                                    palette,
                                                );
                                                render_summary_row(
                                                    ui,
                                                    "系统主题",
                                                    match ctx.system_theme() {
                                                        Some(egui::Theme::Dark) => "深色",
                                                        Some(egui::Theme::Light) => "浅色",
                                                        None => "未知",
                                                    },
                                                    apple_orange(),
                                                    palette,
                                                );
                                            });
                                            ui.horizontal(|ui| {
                                                if ui
                                                    .add(secondary_button("前往活动", palette))
                                                    .clicked()
                                                {
                                                    self.active_tab = AppTab::Activity;
                                                }
                                                if ui
                                                    .add(secondary_button("前往设置", palette))
                                                    .clicked()
                                                {
                                                    self.active_tab = AppTab::Settings;
                                                    self.settings_tab = SettingsTab::Config;
                                                }
                                            });
                                        });
                                    });
                                }
                            }
                            AppTab::Upload => {
                                panel_card_frame(palette).show(ui, |ui| {
                                    self.render_control_panel(
                                        ui,
                                        palette,
                                        has_uploading,
                                        success_count,
                                        close_to_tray,
                                        auto_copy,
                                        notify_on_success,
                                    );
                                });
                            }
                            AppTab::Activity => {
                                self.render_activity_page(
                                    ui,
                                    palette,
                                    uploading_count,
                                    failed_count,
                                );
                            }
                            AppTab::Settings => {
                                panel_card_frame(palette).show(ui, |ui| {
                                    render_panel_header(
                                        ui,
                                        "设置",
                                        "在配置和外观之间切换，避免同屏相互挤压。",
                                        palette,
                                    );
                                    ui.add_space(10.0);
                                    let _ = render_settings_tab_bar(
                                        ui,
                                        &mut self.settings_tab,
                                        palette,
                                    );
                                });
                                panel_card_frame(palette).show(ui, |ui| match self.settings_tab {
                                    SettingsTab::Config => self.render_config_panel(ui, palette),
                                    SettingsTab::Appearance => {
                                        self.render_appearance_panel(ui, ctx, palette)
                                    }
                                });
                            }
                        }
                    });
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.watch_stop.store(true, Ordering::Relaxed);
        let _ = save_config(&self.config);
    }
}

fn main() {
    let cfg = load_config();
    let shared_config = Arc::new(Mutex::new(cfg.clone()));
    let (tx, rx) = mpsc::channel::<AppEvent>();

    // ── 全局快捷键 ────────────────────────────────────────
    let _hotkey_manager = GlobalHotKeyManager::new().ok();
    let mut _registered_hotkey: Option<HotKey> = None;
    if let Some(ref hk_str) = cfg.hotkey {
        if !hk_str.is_empty() {
            if let Some(hk) = parse_hotkey(hk_str) {
                if let Some(ref mgr) = _hotkey_manager {
                    if mgr.register(hk).is_ok() {
                        _registered_hotkey = Some(hk);
                        let tx2 = tx.clone();
                        thread::spawn(move || loop {
                            if let Ok(_event) = GlobalHotKeyEvent::receiver().recv() {
                                let _ = tx2.send(AppEvent::TrayUpload);
                            }
                        });
                    }
                }
            }
        }
    }

    // ── 系统托盘 ─────────────────────────────────────────
    let watch_active_init = cfg.auto_watch.unwrap_or(false);
    let (tray_menu, tray_handles, item_show, item_quit) = build_tray_menu(watch_active_init);

    // 克隆 MenuId 供事件线程使用（MenuId 可 Clone + Send）
    let show_id = item_show.id().clone();
    let upload_id = tray_handles.item_upload.id().clone();
    let watch_id = tray_handles.item_watch.id().clone();
    let quit_id = item_quit.id().clone();

    let shared_recent_urls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let shared_recent_urls_for_thread = Arc::clone(&shared_recent_urls);

    let tray_icon = create_tray_icon(tray_menu);
    if tray_icon.is_none() {
        eprintln!("系统托盘初始化失败，将继续以无托盘模式运行");
    }

    let has_tray = tray_icon.is_some();
    if has_tray {
        let tx3 = tx.clone();
        thread::spawn(move || loop {
            if let Ok(event) = tray_icon::menu::MenuEvent::receiver().recv() {
                let evt = if event.id == show_id {
                    AppEvent::TrayShowWindow
                } else if event.id == upload_id {
                    AppEvent::TrayUpload
                } else if event.id == watch_id {
                    AppEvent::TrayToggleWatch
                } else if event.id == quit_id {
                    AppEvent::TrayQuit
                } else if event.id.0.starts_with("recent_url_") {
                    if let Some(idx_str) = event.id.0.strip_prefix("recent_url_") {
                        if let Ok(idx) = idx_str.parse::<usize>() {
                            let urls = shared_recent_urls_for_thread.lock().unwrap();
                            if let Some(url) = urls.get(idx) {
                                AppEvent::TrayCopyUrl(url.clone())
                            } else {
                                continue;
                            }
                        } else {
                            continue;
                        }
                    } else {
                        continue;
                    }
                } else {
                    continue;
                };
                let _ = tx3.send(evt);
            }
        });
    }
    let tray_handles_opt = if has_tray { Some(tray_handles) } else { None };

    // ── 启动 eframe ──────────────────────────────────────
    let watch_active = cfg.auto_watch.unwrap_or(false);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 820.0])
            .with_min_inner_size([720.0, 620.0])
            .with_title("剪贴板上传工具"),
        ..Default::default()
    };

    let shared_for_app = Arc::clone(&shared_config);
    let tx_for_app = tx.clone();

    // 创建共享 HTTP 客户端（全局复用，避免每次上传都新建连接池）
    let http_client = Arc::new(
        Client::builder()
            .timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new()),
    );
    let watch_stop = Arc::new(AtomicBool::new(false));

    if let Err(e) = eframe::run_native(
        "剪贴板上传工具",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            let _ = apply_theme(
                &cc.egui_ctx,
                config_theme_mode(&cfg),
                config_accent_color(&cfg),
            );
            let db = open_db();
            let history = if let Some(ref conn) = db {
                db_load(conn, 50)
            } else {
                vec![]
            };
            let app = AppState {
                config: cfg,
                shared_config: Arc::clone(&shared_for_app),
                last_url: None,
                status: None,
                status_clear_at: None,
                rx,
                tx: tx_for_app.clone(),
                watch_active,
                quit_requested: false,
                history,
                tasks: Vec::new(),
                next_task_id: 0,
                active_tab: AppTab::Overview,
                settings_tab: SettingsTab::Config,
                bottom_tab: BottomTab::Tasks,
                config_dirty: false,
                db,
                http_client,
                watch_stop,
                tray_handles: tray_handles_opt,
                shared_recent_urls,
            };
            app.start_watch_thread(cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    ) {
        eprintln!("启动失败: {e}");
        std::process::exit(1);
    }

    drop(tray_icon);
}
