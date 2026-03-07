# rust-clipboard-uploader

从系统剪贴板读取图片（如截图），一键上传到图床并获取链接的 GUI 工具。

## 特性

- 跨平台 GUI（Windows / macOS / Linux），基于 [egui](https://github.com/emilk/egui)
- 从系统剪贴板读取图片（使用 [arboard](https://github.com/1Password/arboard)）
- 支持 multipart/form-data 上传，兼容大多数图床
- 通过 `config.yaml` 配置上传地址、字段名、响应解析方式等
- 上传成功后可自动将链接复制到剪贴板
- GitHub Actions 自动构建三平台二进制并发布 Release

## 快速开始

### 从 Release 下载（推荐）

前往 [Releases](../../releases) 页面下载对应平台的二进制文件，直接运行即可。

### 从源码构建

1. 安装 [Rust 工具链](https://rustup.rs/)（stable）
2. Linux 需额外安装依赖：
   ```bash
   sudo apt-get install -y \
     libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
     libxcb-icccm4-dev libxcb-image0-dev libxcb-keysyms1-dev \
     libxkbcommon-dev libxkbcommon-x11-dev \
     libwayland-dev libegl1-mesa-dev \
     libssl-dev libgtk-3-dev
   ```

   > **X11 与 Wayland 均支持**，运行时自动选择当前桌面环境。
   > 强制使用某一后端可设置环境变量：
   > ```bash
   > WINIT_UNIX_BACKEND=wayland ./rust-clipboard-uploader  # 强制 Wayland
   > WINIT_UNIX_BACKEND=x11    ./rust-clipboard-uploader  # 强制 X11
   > ```
3. 构建并运行：
   ```bash
   git clone https://github.com/doulongfei/rust-clipboard-uploader.git
   cd rust-clipboard-uploader
   cargo run --release
   ```

## 配置

首次运行时自动生成默认配置文件，路径：

| 平台    | 路径 |
|---------|------|
| macOS   | `~/Library/Application Support/com.example.RustClipboardUploader/config.yaml` |
| Linux   | `~/.config/RustClipboardUploader/config.yaml` |
| Windows | `%APPDATA%\example\RustClipboardUploader\config\config.yaml` |

### 配置示例

```yaml
upload_url: "https://0x0.st"
method: POST           # POST 或 PUT
file_field: file       # 上传文件的表单字段名
response: text         # text 或 json.data.link（JSON 路径）
copy_to_clipboard: false
```

**`response` 字段说明：**
- `text` — 直接把响应正文当作链接（适用于 0x0.st 等）
- `json.data.link` — 从 JSON 响应中按路径提取链接（适用于返回 JSON 的图床）

### 自定义请求头示例（Token 鉴权）

```yaml
upload_url: "https://your-imghost.example.com/upload"
method: POST
file_field: image
response: "json.data.url"
copy_to_clipboard: true
headers:
  Authorization: "Bearer YOUR_TOKEN"
```

## 使用方法

1. 截图（系统截图快捷键，图片会进入剪贴板）
2. 打开工具，确认配置正确，点击 **📋 从剪贴板上传**
3. 上传成功后链接显示在界面下方，点击 **📋 复制** 或开启自动复制

## CI / Release

- 推送 tag（如 `v0.2.0`）自动触发构建
- 同时构建 Linux、macOS、Windows 三平台二进制
- 构建产物自动上传到 GitHub Release

## 许可

MIT
