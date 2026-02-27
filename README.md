# Rust 剪贴板上传工具

一个在 macOS 上运行的 GUI 小工具，用于把剪贴板中的图片（例如截图）上传到指定图床并复制返回链接（可选）。

特性

- 使用 egui/eframe 构建跨平台 GUI
- 支持从系统剪贴板读取图片（使用 arboard）
- 通过配置文件 config.yaml 设置上传地址、字段名、解析方式等
- 可选：上传成功后自动复制图片链接到剪贴板
- GitHub Actions 自动构建并发布二进制到 Release

快速开始

1. 克隆仓库：

   git clone https://github.com/doulongfei/rust-clipboard-uploader.git

2. 安装 Rust 工具链（stable）：

   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

3. 构建并运行：

   cd rust-clipboard-uploader
   cargo run --release

配置

配置文件存放在平台配置目录，例如 macOS: ~/Library/Application Support/RustClipboardUploader/config.yaml
示例配置（已在程序首次运行时生成默认配置）：

upload_url: "https://0x0.st"
method: "POST"
file_field: "file"
response: "text"
copy_to_clipboard: false

使用说明

- 打开应用，编辑上传地址和选项，点击“保存配置”。
- 将截图复制到剪贴板后，点击“从剪贴板上传”。
- 上传成功后会在界面显示返回的链接（并根据配置复制到剪贴板）。

CI / Release

- 已包含 GitHub Actions 工作流：.github/workflows/release.yml
- Push 一个 tag（例如 v0.1.0）将触发构建并在 Release 中上传二进制文件

许可

MIT
