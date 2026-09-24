# AGENTS.md — rust-clipboard-uploader

## 项目概述

一个跨平台（Windows / macOS / Linux）的剪贴板图片上传工具。用户截图后，通过 GUI 或全局热键一键将剪贴板中的图片上传到图床，并自动获取链接。

- **技术栈**：Rust + egui (eframe) + reqwest + rusqlite + arboard
- **当前版本**：`Cargo.toml` 中的 `version` 字段（目前 1.0.7）
- **代码结构**：业务和界面在 `src/main.rs`；macOS 登录项、单实例和生命周期在 `src/macos.rs` 与 `native/macos.m`。

---

## 构建与开发

```bash
# 检查编译（零错误零警告）
source ~/.cargo/env && cargo check

# 运行调试版
cargo run

# 构建 Release
cargo build --release
```

### Linux 系统依赖（首次构建前安装）
```bash
sudo apt-get install -y \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libxcb-icccm4-dev libxcb-image0-dev libxcb-keysyms1-dev \
  libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libegl1-mesa-dev \
  libssl-dev libgtk-3-dev
```

### 强制指定后端（Linux）
```bash
WINIT_UNIX_BACKEND=wayland ./rust-clipboard-uploader  # 强制 Wayland
WINIT_UNIX_BACKEND=x11    ./rust-clipboard-uploader  # 强制 X11
```

---

## 架构说明

### 核心数据结构

| 结构体 / 枚举 | 说明 |
|---|---|
| `AppState` | 主 App 状态，实现 `eframe::App` |
| `Config` | 配置文件结构，序列化为 `config.yaml` |
| `UploadTask` | 单次上传任务，包含状态、图片数据、时间戳 |
| `TaskStatus` | 任务状态枚举：`Uploading / Processing / Success(url, src) / Failed(msg)` |
| `AppEvent` | 后台线程 → 主线程的消息类型（mpsc channel） |
| `HistoryRecord` | SQLite 历史记录行 |

### 线程模型
- **主线程**：运行 egui UI，通过 `mpsc::Receiver<AppEvent>` 接收事件
- **上传后台线程**：每个任务独立 `thread::spawn`，完成后发 `TaskProgress`
- **剪贴板监听线程**：`auto_watch` 开启时轮询剪贴板变化（`watch_thread`），发 `WatchUpload`
- **托盘线程**：处理系统托盘菜单事件（`TrayShowWindow / TrayUpload / TrayToggleWatch / TrayQuit`）

### uploading 状态判断（关键模式）
不用独立的 `uploading: bool` 字段，每帧动态计算：
```rust
let has_uploading = self.tasks.iter().any(|t| matches!(t.status, TaskStatus::Uploading));
```

### 图片数据生命周期
- 任务创建时存入 `task.image_data`
- 任务变为 `Success` 或 `Failed` 后主线程将其清空（`task.image_data = Vec::new()`）
- 失败任务的 `data_expires_at` 到期后图片数据自动释放
- 重试时：若 `image_data` 为空则重新读剪贴板

---

## 配置文件

路径（由 `directories::ProjectDirs` 确定）：

| 平台 | 路径 |
|---|---|
| macOS | `~/Library/Application Support/com.example.RustClipboardUploader/config.yaml` |
| Linux | `~/.config/RustClipboardUploader/config.yaml` |
| Windows | `%APPDATA%\example\RustClipboardUploader\config\config.yaml` |

### 配置字段
```yaml
upload_url: "https://0x0.st"   # 必填
method: POST                    # POST 或 PUT
file_field: file                # 表单字段名
response: text                  # text 或 json 路径（如 json.data.url）
copy_to_clipboard: false
auto_watch: false
notify_on_success: false
hotkey: null                    # 全局热键，如 "ctrl+shift+u"
headers:                        # 可选自定义请求头
  Authorization: "Bearer TOKEN"
```

### response 字段解析规则（`extract_url_from_response`）
- `text` → 原始响应文本即为 URL
- `json.data.url` → 按点分路径从 JSON 中提取
- `[0].url` → JSON 数组第0项的字段

---

## 数据库（SQLite）

- 路径：`data_dir()/history.db`
- 表：`history(id, url, src, uploaded_at)`
- `AppState` 持有 `db: Option<Connection>` 复用连接
- 后台上传线程自行调用 `open_db()` 获取独立连接（`Connection` 非 `Send`）

主要函数：`open_db()` / `db_insert()` / `db_load()` / `db_delete()` / `db_clear()`

---

## CI / Release

- 触发条件：推送 master、`v*` tag 或手动触发 `workflow_dispatch`；master 上新版本号自动发布，已有 tag 的版本仅检查。
- 构建矩阵：Linux / macOS arm64 / macOS Intel / Windows。
- macOS 产物：`RustClipboardUploader-<版本>-macos-<arm64|x86_64>.dmg`；其他平台保持二进制。
- 提交 `Cargo.lock`，CI 使用 `--locked`；macOS 最低 13.0。
- 打包脚本：`scripts/package-macos.sh`；签名和公证 Secrets 见 README。
- Release 由 `softprops/action-gh-release@v2` 自动创建

发布流程：
```bash
# 更新 Cargo.toml 中的 version
cargo check
git add -A && git commit -m "chore: bump version to vX.Y.Z"
git push origin master
```

---

## 常见开发注意事项

1. **编译检查**：每次修改后必须 `cargo check` 确保零警告，再提交
2. **状态清除时机**：成功/已复制等提示状态通过 `status_clear_at: Option<Instant>` 在 3 秒后自动清除，在 `update()` 开头检查
3. **config_dirty**：配置控件 `.changed()` 时设为 `true`，保存后重置；保存按钮标签随之变化（`●` 标记）
4. **剪贴板指纹**：`clipboard_raw_fingerprint()` 用于 watch 线程去重，只读原始 RGBA bytes，不做 PNG 编码，避免高频内存分配
5. **watch 线程 CPU 占用**：`auto_watch=false` 时 sleep 2000ms，`true` 时 sleep 500ms
6. **托盘图标**：通过 `assets/NotoEmoji-Regular.ttf` 渲染 emoji 图标
