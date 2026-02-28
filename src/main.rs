use eframe::egui;
use std::fs;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};
use directories::ProjectDirs;
use anyhow::Result;
use arboard::Clipboard;
use image::{ExtendedColorType, ImageEncoder};
use reqwest::blocking::Client;
use reqwest::blocking::multipart;
use global_hotkey::{GlobalHotKeyManager, GlobalHotKeyEvent, hotkey::{HotKey, Modifiers, Code}};
use tray_icon::{TrayIconBuilder, Icon as TrayIconImg, menu::{Menu, MenuItem, PredefinedMenuItem}};
use rusqlite::{Connection, params};
use chrono::Local;

// ── 上传任务 ──────────────────────────────────────────────
#[derive(Debug, Clone)]
enum TaskStatus {
    Uploading,
    Processing,              // 已上传，服务端处理中（如大模型命名）
    Success(String, String), // (url, src)
    Failed(String),          // error msg
}

#[derive(Debug, Clone)]
struct UploadTask {
    id: usize,
    status: TaskStatus,
    image_data: Vec<u8>, // 用于失败重试；完成后清空
    created_at: String,  // 格式 %H:%M:%S
}

// ── 消息类型（后台线程 → 主线程）──────────────────────────
#[derive(Debug)]
enum AppEvent {
    TaskProgress(usize, TaskStatus), // (task_id, new_status)
    WatchUpload(Vec<u8>),            // 监听线程捕获到新图片，主线程分配任务
    TrayShowWindow,
    TrayUpload,
    TrayToggleWatch,
    TrayQuit,
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
    notify_on_success: Option<bool>,
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
            notify_on_success: Some(false),
            hotkey: None,
        }
    }
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
        );"
    ).ok()?;
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
    let mut stmt = match conn.prepare(
        "SELECT id, url, src, uploaded_at FROM history ORDER BY id DESC LIMIT ?1"
    ) {
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
        .write_image(&image.bytes, image.width as u32, image.height as u32, ExtendedColorType::Rgba8)
        .ok()?;
    Some(out)
}

/// 图片内容的简单指纹（长度 + 前64字节）
fn image_fingerprint(data: &[u8]) -> u64 {
    let len = data.len() as u64;
    let prefix: u64 = data.iter().take(64).enumerate().fold(0u64, |acc, (i, &b)| {
        acc ^ ((b as u64) << (i % 8 * 8))
    });
    len ^ (prefix.wrapping_mul(0x9e3779b97f4a7c15))
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
        if seg.is_empty() { continue; }
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
        if cur.is_array() { cur = cur.get(0)?; }
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
    let root = if j.is_array() { j.get(0).unwrap_or(&j) } else { &j };
    root.get("src")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

// ── 上传：构建请求（不发送）────────────────────────────
fn build_upload_request(cfg: &Config, img: Vec<u8>) -> Result<reqwest::blocking::RequestBuilder> {
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
    Ok(req.multipart(form))
}

// ── 上传：解析响应 ────────────────────────────────────
fn parse_upload_response(cfg: &Config, resp: reqwest::blocking::Response) -> Result<(String, String)> {
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if status.is_success() {
        let url = extract_url_from_response(cfg, &text).unwrap_or_else(|| text.clone());
        let src = extract_src_from_response(&text);
        return Ok((url, src));
    }
    Err(anyhow::anyhow!("请求失败: {} — {}", status, text.trim()))
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
            fonts.font_data.insert("cjk".to_owned(), egui::FontData::from_owned(data).into());
            fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap().insert(0, "cjk".to_owned());
            fonts.families.get_mut(&egui::FontFamily::Monospace).unwrap().push("cjk".to_owned());
            break;
        }
    }
    ctx.set_fonts(fonts);
}

// ── 解析快捷键字符串 ──────────────────────────────────────
fn parse_hotkey(s: &str) -> Option<HotKey> {
    let parts: Vec<&str> = s.split('+').collect();
    if parts.is_empty() { return None; }
    let key_str = parts.last()?;
    let code = match key_str.to_lowercase().as_str() {
        "a" => Code::KeyA, "b" => Code::KeyB, "c" => Code::KeyC,
        "d" => Code::KeyD, "e" => Code::KeyE, "f" => Code::KeyF,
        "g" => Code::KeyG, "h" => Code::KeyH, "i" => Code::KeyI,
        "j" => Code::KeyJ, "k" => Code::KeyK, "l" => Code::KeyL,
        "m" => Code::KeyM, "n" => Code::KeyN, "o" => Code::KeyO,
        "p" => Code::KeyP, "q" => Code::KeyQ, "r" => Code::KeyR,
        "s" => Code::KeyS, "t" => Code::KeyT, "u" => Code::KeyU,
        "v" => Code::KeyV, "w" => Code::KeyW, "x" => Code::KeyX,
        "y" => Code::KeyY, "z" => Code::KeyZ,
        "f1" => Code::F1, "f2" => Code::F2, "f3" => Code::F3,
        "f4" => Code::F4, "f5" => Code::F5, "f6" => Code::F6,
        _ => return None,
    };
    let mut mods = Modifiers::empty();
    for part in &parts[..parts.len()-1] {
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
fn do_upload_task(cfg: &Config, tx: &mpsc::Sender<AppEvent>, task_id: usize, img: Vec<u8>) {
    // 1. 构建并发送请求（文件传输阶段）
    let req = match build_upload_request(cfg, img) {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(AppEvent::TaskProgress(task_id, TaskStatus::Failed(format!("构建请求失败: {}", e))));
            return;
        }
    };
    let resp = match req.send() {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(AppEvent::TaskProgress(task_id, TaskStatus::Failed(format!("发送请求失败: {}", e))));
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
            let _ = tx.send(AppEvent::TaskProgress(task_id, TaskStatus::Success(url, src)));
        }
        Err(e) => {
            let msg = format!("上传失败: {}", e);
            let _ = tx.send(AppEvent::TaskProgress(task_id, TaskStatus::Failed(msg)));
        }
    }
}

// ── 底部 Tab ──────────────────────────────────────────────
#[derive(Debug, Clone, PartialEq)]
enum BottomTab {
    Tasks,
    History,
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
    bottom_tab: BottomTab,
    config_dirty: bool,
    db: Option<Connection>,
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
        });
        let cfg = self.config.clone();
        let tx = self.tx.clone();
        thread::spawn(move || do_upload_task(&cfg, &tx, task_id, img));
        task_id
    }

    fn trigger_upload(&mut self) {
        let has_uploading = self.tasks.iter().any(|t| matches!(t.status, TaskStatus::Uploading | TaskStatus::Processing));
        if has_uploading { return; }
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
        self.bottom_tab = BottomTab::Tasks;
        self.spawn_task(img);
    }

    fn retry_task(&mut self, task_id: usize) {
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
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task_id) {
            t.status = TaskStatus::Uploading;
            t.image_data = img.clone();
        }
        let cfg = self.config.clone();
        let tx = self.tx.clone();
        thread::spawn(move || do_upload_task(&cfg, &tx, task_id, img));
    }

    fn start_watch_thread(&self) {
        let tx = self.tx.clone();
        let shared = Arc::clone(&self.shared_config);
        thread::spawn(move || {
            let mut last_fp: u64 = 0;
            loop {
                let cfg = shared.lock().unwrap().clone();
                let auto_watch = cfg.auto_watch.unwrap_or(false);
                thread::sleep(Duration::from_millis(if auto_watch { 500 } else { 2000 }));
                if !auto_watch { continue; }
                if let Some(img) = read_clipboard_image() {
                    let fp = image_fingerprint(&img);
                    if fp != last_fp {
                        last_fp = fp;
                        let _ = tx.send(AppEvent::WatchUpload(img));
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
                        TaskStatus::Failed(msg) => {
                            let msg = msg.clone();
                            if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task_id) {
                                t.status = TaskStatus::Failed(msg.clone());
                                // 保留 image_data 供重试
                            }
                            self.set_status_sticky(true, msg);
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
                    }
                }
                AppEvent::WatchUpload(img) => {
                    self.bottom_tab = BottomTab::Tasks;
                    self.spawn_task(img);
                }
                AppEvent::TrayShowWindow => { ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true)); }
                AppEvent::TrayUpload => { self.trigger_upload(); }
                AppEvent::TrayToggleWatch => {
                    let cur = self.config.auto_watch.unwrap_or(false);
                    self.config.auto_watch = Some(!cur);
                    *self.shared_config.lock().unwrap() = self.config.clone();
                    self.watch_active = !cur;
                }
                AppEvent::TrayQuit => { self.quit_requested = true; }
            }
            ctx.request_repaint();
        }

        if self.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // 持续刷新以响应后台事件
        ctx.request_repaint_after(Duration::from_millis(200));

        let has_uploading = self.tasks.iter().any(|t| matches!(t.status, TaskStatus::Uploading | TaskStatus::Processing));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("剪贴板上传工具");
            ui.add_space(8.0);

            // ── 配置区 ──────────────────────────────────────
            egui::Grid::new("config_grid")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.label("上传地址:");
                    if ui.add(egui::TextEdit::singleline(&mut self.config.upload_url).desired_width(f32::INFINITY)).changed() {
                        self.config_dirty = true;
                    }
                    ui.end_row();

                    ui.label("请求方法:");
                    ui.horizontal(|ui| {
                        let method = self.config.method.get_or_insert_with(|| "POST".to_string());
                        if ui.selectable_value(method, "POST".to_string(), "POST").changed() {
                            self.config_dirty = true;
                        }
                        if ui.selectable_value(method, "PUT".to_string(), "PUT").changed() {
                            self.config_dirty = true;
                        }
                    });
                    ui.end_row();

                    ui.label("文件字段:");
                    let ff = self.config.file_field.get_or_insert_with(|| "file".to_string());
                    if ui.add(egui::TextEdit::singleline(ff).desired_width(f32::INFINITY)).changed() {
                        self.config_dirty = true;
                    }
                    ui.end_row();

                    ui.label("响应解析:");
                    ui.vertical(|ui| {
                        let resp = self.config.response.get_or_insert_with(|| "text".to_string());
                        if ui.add(egui::TextEdit::singleline(resp).desired_width(f32::INFINITY)).changed() {
                            self.config_dirty = true;
                        }
                        ui.label(egui::RichText::new("text | json.url | json.data.link").small().weak());
                    });
                    ui.end_row();

                    ui.label("全局快捷键:");
                    ui.vertical(|ui| {
                        let hk = self.config.hotkey.get_or_insert_with(String::new);
                        if ui.add(egui::TextEdit::singleline(hk)
                            .hint_text("ctrl+shift+u")
                            .desired_width(f32::INFINITY)).changed() {
                            self.config_dirty = true;
                        }
                        ui.label(egui::RichText::new("留空则不设置，修改后保存配置并重启生效").small().weak());
                    });
                    ui.end_row();

                    ui.label("上传选项:");
                    ui.vertical(|ui| {
                        let copy = self.config.copy_to_clipboard.get_or_insert(false);
                        if ui.checkbox(copy, "上传成功后自动复制链接").changed() {
                            self.config_dirty = true;
                        }
                        let notify = self.config.notify_on_success.get_or_insert(false);
                        if ui.checkbox(notify, "上传成功后发送系统通知").changed() {
                            self.config_dirty = true;
                        }
                    });
                    ui.end_row();

                    ui.label("自动监听:");
                    ui.horizontal(|ui| {
                        let watch = self.config.auto_watch.get_or_insert(false);
                        if ui.checkbox(watch, "监听剪贴板变化自动上传").changed() {
                            self.watch_active = *watch;
                            self.config_dirty = true;
                            *self.shared_config.lock().unwrap() = self.config.clone();
                        }
                        if self.watch_active {
                            ui.label(egui::RichText::new("👁 监听中").color(egui::Color32::from_rgb(80, 200, 120)));
                        } else {
                            ui.label(egui::RichText::new("未监听").weak());
                        }
                    });
                    ui.end_row();
                });

            ui.add_space(10.0);

            // ── 操作按钮 ────────────────────────────────────
            ui.horizontal(|ui| {
                let save_label = if self.config_dirty { "💾 保存配置 ●" } else { "💾 保存配置" };
                if ui.button(save_label).clicked() {
                    *self.shared_config.lock().unwrap() = self.config.clone();
                    match save_config(&self.config) {
                        Ok(_) => {
                            self.config_dirty = false;
                            self.set_status_auto_clear(false, "配置已保存".to_string());
                        }
                        Err(e) => self.set_status_sticky(true, format!("保存失败: {}", e)),
                    }
                }

                let btn_label = if has_uploading { "⏳ 上传中..." } else { "📤 从剪贴板上传" };
                if ui.add_enabled(!has_uploading, egui::Button::new(btn_label)).clicked() {
                    self.trigger_upload();
                }
            });

            ui.add_space(6.0);

            // ── 状态栏 ──────────────────────────────────────
            if let Some((is_err, msg)) = &self.status {
                let color = if *is_err { egui::Color32::from_rgb(220, 80, 80) } else { egui::Color32::from_rgb(80, 200, 120) };
                let icon = if *is_err { "❌" } else { "✅" };
                ui.label(egui::RichText::new(format!("{} {}", icon, msg)).color(color));
                ui.add_space(4.0);
            }

            // ── 最近上传链接（最后一次成功）────────────────
            if let Some(url) = self.last_url.clone() {
                egui::Frame::default()
                    .fill(egui::Color32::from_gray(30))
                    .rounding(egui::Rounding::same(4.0))
                    .inner_margin(egui::Margin::same(8.0))
                    .show(ui, |ui: &mut egui::Ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.add(egui::Label::new(
                                egui::RichText::new(&url).monospace().small().color(egui::Color32::from_rgb(100, 200, 255))
                            ).wrap());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.hyperlink_to("🔗", &url).on_hover_text("在浏览器打开");
                                if ui.small_button("📋").on_hover_text("复制链接").clicked() {
                                    if copy_text_to_clipboard(&url) {
                                        self.set_status_auto_clear(false, "已复制到剪贴板".to_string());
                                    }
                                }
                            });
                        });
                    });
                ui.add_space(6.0);
            }

            ui.separator();
            ui.add_space(4.0);

            // ── 底部 Tab 栏：任务队列 | 上传历史 ───────────
            let uploading_count = self.tasks.iter().filter(|t| matches!(t.status, TaskStatus::Uploading | TaskStatus::Processing)).count();
            let failed_count = self.tasks.iter().filter(|t| matches!(t.status, TaskStatus::Failed(_))).count();

            ui.horizontal(|ui| {
                let task_label = if uploading_count > 0 {
                    format!("⏳ 传输任务 ({})", uploading_count)
                } else if failed_count > 0 {
                    format!("❌ 传输任务 ({}失败)", failed_count)
                } else {
                    format!("📋 传输任务 ({})", self.tasks.len())
                };
                let task_selected = self.bottom_tab == BottomTab::Tasks;
                if ui.selectable_label(task_selected, task_label).clicked() {
                    self.bottom_tab = BottomTab::Tasks;
                }

                ui.separator();

                let hist_label = format!("🕓 上传历史 ({})", self.history.len());
                let hist_selected = self.bottom_tab == BottomTab::History;
                if ui.selectable_label(hist_selected, hist_label).clicked() {
                    self.bottom_tab = BottomTab::History;
                    if let Some(ref conn) = self.db {
                        self.history = db_load(conn, 50);
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match self.bottom_tab {
                        BottomTab::Tasks => {
                            if !self.tasks.is_empty() {
                                if ui.small_button("🗑 清空").on_hover_text("移除所有已完成任务").clicked() {
                                    self.tasks.retain(|t| matches!(t.status, TaskStatus::Uploading | TaskStatus::Processing));
                                }
                            }
                        }
                        BottomTab::History => {
                            if !self.history.is_empty() {
                                if ui.small_button("🗑 清空").clicked() {
                                    if let Some(ref conn) = self.db {
                                        db_clear(conn);
                                    }
                                    self.history.clear();
                                }
                            }
                        }
                    }
                });
            });

            ui.add_space(4.0);

            // ── Tab 内容 ─────────────────────────────────────
            match self.bottom_tab {
                BottomTab::Tasks => {
                    if self.tasks.is_empty() {
                        ui.label(egui::RichText::new("暂无上传任务").weak().italics());
                    } else {
                        let mut retry_id: Option<usize> = None;
                        let mut remove_id: Option<usize> = None;
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .id_salt("task_scroll")
                            .show(ui, |ui| {
                                for task in self.tasks.iter().rev() {
                                    egui::Frame::default()
                                        .fill(egui::Color32::from_gray(32))
                                        .rounding(egui::Rounding::same(4.0))
                                        .inner_margin(egui::Margin::same(6.0))
                                        .show(ui, |ui| {
                                            ui.set_width(ui.available_width());
                                            match &task.status {
                                                TaskStatus::Uploading => {
                                                    ui.horizontal(|ui| {
                                                        ui.label(egui::RichText::new(&task.created_at).small().weak());
                                                        ui.spinner();
                                                        ui.label(egui::RichText::new("上传中…")
                                                            .color(egui::Color32::from_rgb(200, 180, 80)));
                                                    });
                                                }
                                                TaskStatus::Processing => {
                                                    ui.horizontal(|ui| {
                                                        ui.label(egui::RichText::new(&task.created_at).small().weak());
                                                        ui.spinner();
                                                        ui.label(egui::RichText::new("处理中…")
                                                            .color(egui::Color32::from_rgb(100, 180, 255)));
                                                        ui.label(egui::RichText::new("(已上传，AI 命名中)")
                                                            .small().weak());
                                                    });
                                                }
                                                TaskStatus::Success(url, _src) => {
                                                    ui.horizontal(|ui| {
                                                        // 时间戳
                                                        ui.label(egui::RichText::new(&task.created_at).small().weak());
                                                        ui.label(egui::RichText::new("✅")
                                                            .color(egui::Color32::from_rgb(80, 200, 120)));
                                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                            if ui.small_button("✕").on_hover_text("移除").clicked() {
                                                                remove_id = Some(task.id);
                                                            }
                                                            if ui.small_button("📋").on_hover_text("复制链接").clicked() {
                                                                copy_text_to_clipboard(url);
                                                                // Note: can't call self methods in closure; set via remove_id pattern workaround
                                                            }
                                                        });
                                                    });
                                                    ui.add(egui::Label::new(
                                                        egui::RichText::new(url).monospace().small()
                                                            .color(egui::Color32::from_rgb(100, 200, 255))
                                                    ).wrap());
                                                }
                                                TaskStatus::Failed(err) => {
                                                    ui.horizontal(|ui| {
                                                        // 时间戳
                                                        ui.label(egui::RichText::new(&task.created_at).small().weak());
                                                        ui.label(egui::RichText::new("❌")
                                                            .color(egui::Color32::from_rgb(220, 80, 80)));
                                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                            if ui.small_button("✕").on_hover_text("移除").clicked() {
                                                                remove_id = Some(task.id);
                                                            }
                                                            if ui.small_button("🔄 重试").clicked() {
                                                                retry_id = Some(task.id);
                                                            }
                                                        });
                                                    });
                                                    ui.add(egui::Label::new(
                                                        egui::RichText::new(err).small()
                                                            .color(egui::Color32::from_rgb(220, 120, 120))
                                                    ).wrap());
                                                }
                                            }
                                        });
                                    ui.add_space(2.0);
                                }
                            });
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
                        ui.label(egui::RichText::new("暂无历史记录").weak().italics());
                    } else {
                        let mut to_delete: Option<i64> = None;
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .id_salt("history_scroll")
                            .show(ui, |ui| {
                                for record in &self.history {
                                    ui.add_space(2.0);
                                    egui::Frame::default()
                                        .fill(egui::Color32::from_gray(28))
                                        .rounding(egui::Rounding::same(4.0))
                                        .inner_margin(egui::Margin::same(6.0))
                                        .show(ui, |ui| {
                                            ui.set_width(ui.available_width());
                                            ui.horizontal(|ui| {
                                                ui.label(egui::RichText::new(&record.uploaded_at).small().weak());
                                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                    if ui.small_button("✕").clicked() {
                                                        to_delete = Some(record.id);
                                                    }
                                                    if ui.small_button("📋").on_hover_text("复制链接").clicked() {
                                                        copy_text_to_clipboard(&record.url);
                                                        // status update handled after borrow ends
                                                    }
                                                });
                                            });
                                            ui.add(egui::Label::new(
                                                egui::RichText::new(&record.url).monospace().small()
                                                    .color(egui::Color32::from_rgb(100, 200, 255))
                                            ).wrap());
                                            if !record.src.is_empty() {
                                                ui.label(egui::RichText::new(format!("src: {}", record.src)).small().weak());
                                            }
                                        });
                                }
                            });
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

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
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
                        thread::spawn(move || {
                            loop {
                                if let Ok(_event) = GlobalHotKeyEvent::receiver().recv() {
                                    let _ = tx2.send(AppEvent::TrayUpload);
                                }
                            }
                        });
                    }
                }
            }
        }
    }

    // ── 系统托盘 ─────────────────────────────────────────
    let tray_menu = Menu::new();
    let item_show = MenuItem::new("打开窗口", true, None);
    let item_upload = MenuItem::new("从剪贴板上传", true, None);
    let item_watch = MenuItem::new("自动监听: 开/关", true, None);
    let item_quit = MenuItem::new("退出", true, None);

    let _ = tray_menu.append(&item_show);
    let _ = tray_menu.append(&item_upload);
    let _ = tray_menu.append(&item_watch);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());
    let _ = tray_menu.append(&item_quit);

    let icon_data = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
        0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x10,
        0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x91, 0x68, 0x36,
        0x00, 0x00, 0x00, 0x34, 0x49, 0x44, 0x41, 0x54,
        0x78, 0x9C, 0x62, 0xF8, 0xCF, 0xC0, 0xC0, 0xC0,
        0xF0, 0x9F, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81,
        0x81, 0xFF, 0x18, 0x18, 0x18, 0x18, 0x18, 0x98,
        0xFF, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81,
        0x81, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0x03, 0x00,
        0x0E, 0xF0, 0x03, 0x01, 0xEF, 0x2F, 0x2A, 0x0B,
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
        0xAE, 0x42, 0x60, 0x82,
    ];

    let tray_icon = if let Ok(img) = image::load_from_memory(&icon_data) {
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        TrayIconImg::from_rgba(rgba.into_raw(), w, h).ok().and_then(|icon| {
            TrayIconBuilder::new()
                .with_menu(Box::new(tray_menu))
                .with_icon(icon)
                .with_tooltip("剪贴板上传工具")
                .build()
                .ok()
        })
    } else {
        None
    };

    if tray_icon.is_some() {
        let tx3 = tx.clone();
        let show_id = item_show.id().clone();
        let upload_id = item_upload.id().clone();
        let watch_id = item_watch.id().clone();
        let quit_id = item_quit.id().clone();
        thread::spawn(move || {
            loop {
                if let Ok(event) = tray_icon::menu::MenuEvent::receiver().recv() {
                    let evt = if event.id == show_id {
                        AppEvent::TrayShowWindow
                    } else if event.id == upload_id {
                        AppEvent::TrayUpload
                    } else if event.id == watch_id {
                        AppEvent::TrayToggleWatch
                    } else if event.id == quit_id {
                        AppEvent::TrayQuit
                    } else {
                        continue;
                    };
                    let _ = tx3.send(evt);
                }
            }
        });
    }

    // ── 启动 eframe ──────────────────────────────────────
    let watch_active = cfg.auto_watch.unwrap_or(false);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 720.0])
            .with_title("剪贴板上传工具"),
        ..Default::default()
    };

    let shared_for_app = Arc::clone(&shared_config);
    let tx_for_app = tx.clone();

    if let Err(e) = eframe::run_native(
        "剪贴板上传工具",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
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
                bottom_tab: BottomTab::Tasks,
                config_dirty: false,
                db,
            };
            app.start_watch_thread();
            Ok(Box::new(app))
        }),
    ) {
        eprintln!("启动失败: {e}");
        std::process::exit(1);
    }

    drop(tray_icon);
}
