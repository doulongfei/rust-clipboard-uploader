# rust-clipboard-uploader

一个面向日常截图工作流的跨平台 GUI 工具：从系统剪贴板读取图片，上传到图床或自定义接口，并立即拿到可复制的链接。

项目基于 Rust + `eframe/egui` 构建，支持 Windows、macOS、Linux，适合需要频繁截图、上传、回贴链接的场景。

## 功能概览

- 从系统剪贴板读取图片并上传，兼容常见截图工具产出的剪贴板图像
- 支持 `POST` / `PUT`、`multipart/form-data`、自定义上传字段名、自定义请求头
- 支持响应解析规则：
  - `text`
  - `json.url`
  - `json.data.link`
  - `[0].url`
- 上传成功后可自动复制链接到剪贴板
- 支持系统通知
- 支持自动监听剪贴板，新截图可直接触发上传
- 支持全局快捷键触发上传
- 支持失败自动重试：
  - 仅针对网络错误、`429`、`5xx`
  - 最多自动重试 2 次
- 内置任务列表和 SQLite 历史记录
- 多 Tab 界面：
  - `概览`
  - `上传`
  - `活动`
  - `设置`
- 支持 `跟随系统 / 浅色 / 深色` 主题模式和 5 组主题色
- 支持托盘/菜单栏常驻：
  - macOS 上有托盘时会以菜单栏后台应用方式运行
  - Windows / Linux 关闭窗口后可隐藏到托盘

## 快速开始

### 从 Release 下载

前往 [Releases](../../releases) 下载对应平台的二进制文件：

- `rust-clipboard-uploader-linux`
- `rust-clipboard-uploader-macos`
- `rust-clipboard-uploader-windows.exe`

下载后直接运行即可。

### 从源码运行

1. 安装稳定版 Rust 工具链

```bash
curl https://sh.rustup.rs -sSf | sh
source ~/.cargo/env
```

2. 克隆项目

```bash
git clone https://github.com/doulongfei/rust-clipboard-uploader.git
cd rust-clipboard-uploader
```

3. 检查编译

```bash
cargo check
```

4. 运行调试版

```bash
cargo run
```

5. 构建 Release

```bash
cargo build --release
```

### Linux 构建依赖

首次在 Linux 上构建前，请先安装系统依赖：

```bash
sudo apt-get install -y \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libxcb-icccm4-dev libxcb-image0-dev libxcb-keysyms1-dev \
  libxcb-randr0-dev libxcb-xtest0-dev libxcb-glx0-dev \
  libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libwayland-egl-backend-dev \
  libssl-dev libgtk-3-dev \
  libegl1-mesa-dev libgles2-mesa-dev \
  libxdo-dev
```

如果需要强制指定图形后端：

```bash
WINIT_UNIX_BACKEND=wayland cargo run
WINIT_UNIX_BACKEND=x11 cargo run
```

## 使用方式

### 手动上传

1. 使用系统截图工具完成截图，确保图片已经进入剪贴板
2. 打开应用，进入 `上传` 页
3. 点击上传按钮，或使用托盘菜单的上传动作
4. 上传成功后在 `活动` / `历史` 中查看结果

### 自动监听

开启 `自动监听剪贴板` 后，程序会轮询剪贴板变化。检测到新的截图图像时，会自动创建上传任务。

### 全局快捷键

在设置中填入快捷键字符串，例如：

- `ctrl+shift+u`
- `cmd+shift+u`

保存后重启应用生效。留空表示不注册全局快捷键。

### 托盘 / 菜单栏

- macOS：托盘可用时会以菜单栏应用方式运行，关闭窗口后继续在后台工作
- Windows / Linux：可关闭窗口隐藏到托盘，之后从托盘菜单恢复窗口

## 配置

首次运行时会自动创建配置文件 `config.yaml`。

### 配置文件位置

| 平台 | 路径 |
| --- | --- |
| macOS | `~/Library/Application Support/com.example.RustClipboardUploader/config.yaml` |
| Linux | `~/.config/RustClipboardUploader/config.yaml` |
| Windows | `%APPDATA%\example\RustClipboardUploader\config\config.yaml` |

历史记录数据库会自动保存在应用数据目录下的 `history.db` 中。

### 完整配置示例

```yaml
upload_url: "https://0x0.st"
method: "POST"
file_field: "file"
headers:
  Authorization: "Bearer YOUR_TOKEN"
response: "text"
copy_to_clipboard: true
auto_watch: false
auto_retry: true
notify_on_success: true
close_to_tray: true
theme_mode: "system"
accent_color: "blue"
hotkey: "ctrl+shift+u"
```

### 配置字段说明

| 字段 | 说明 |
| --- | --- |
| `upload_url` | 上传接口地址 |
| `method` | 请求方法，支持 `POST` / `PUT` |
| `file_field` | multipart 表单中的文件字段名 |
| `headers` | 自定义请求头，适合 Bearer Token 或其他鉴权场景 |
| `response` | 响应解析规则，决定如何从服务端响应中取出链接 |
| `copy_to_clipboard` | 上传成功后是否自动复制链接 |
| `auto_watch` | 是否自动监听剪贴板中的新截图 |
| `auto_retry` | 是否在可重试错误时自动重试 |
| `notify_on_success` | 是否在上传成功后显示系统通知 |
| `close_to_tray` | 关闭窗口时是否继续后台运行 |
| `theme_mode` | `system` / `light` / `dark` |
| `accent_color` | `blue` / `green` / `orange` / `pink` / `purple` |
| `hotkey` | 全局快捷键字符串，留空则不注册 |

### 响应解析示例

| 配置值 | 说明 |
| --- | --- |
| `text` | 将整个响应正文当作链接 |
| `json.url` | 读取 JSON 对象中的 `url` 字段 |
| `json.data.url` | 读取嵌套字段 |
| `[0].url` | 读取 JSON 数组第一个元素的 `url` 字段 |

示例：

```yaml
response: "json.data.url"
```

如果服务端返回：

```json
{"data":{"url":"https://img.example.com/abc.png"}}
```

程序会提取出：

```text
https://img.example.com/abc.png
```

## 失败重试策略

自动重试只对临时性失败生效：

- 请求发送失败
- HTTP `429`
- HTTP `5xx`

当前策略：

- 最多自动重试 2 次
- 第 1 次等待 2 秒
- 第 2 次等待 5 秒

配置错误、解析错误、普通 `4xx` 响应不会自动重试，但仍然可以在任务列表里手动重试。

## 界面结构

### 顶部主导航

- `概览`：运行状态、任务统计、开关摘要
- `上传`：手动上传入口和最近状态
- `活动`：进行中的任务、失败任务、上传历史
- `设置`：配置与外观

### 设置页

设置页拆分为两个子页：

- `配置`
- `外观`

其中外观页支持系统主题联动和主题色切换。

## 开发说明

当前项目的主要业务逻辑集中在单文件 [`src/main.rs`](src/main.rs) 中，适合快速迭代，但也意味着修改前最好先通读相关状态流：

- UI 状态：`AppState`
- 配置结构：`Config`
- 任务状态：`TaskStatus`
- 任务记录：`UploadTask`
- 后台事件：`AppEvent`

常用开发命令：

```bash
source ~/.cargo/env
cargo fmt
cargo check
cargo run
```

## CI / Release

项目内置 GitHub Actions 发布流程，定义在 [`.github/workflows/release.yml`](.github/workflows/release.yml)。

触发方式：

- 推送 `v*` tag，例如 `v0.5.3`
- 手动触发 `workflow_dispatch`

发布流程会自动：

- 构建 `ubuntu-latest`
- 构建 `macos-latest`
- 构建 `windows-latest`
- 上传三平台构建产物
- 创建 GitHub Release

常用发布命令：

```bash
git add -A
git commit -m "chore: release vX.Y.Z"
git tag vX.Y.Z
git push origin master --tags
```

## 许可

MIT
