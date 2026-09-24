## macOS 安装与自启动

- 新增 Apple Silicon（arm64）和 Intel（x86_64）的 DMG 安装包，要求 macOS 13 或更新版本。
- 打开 DMG，将 RustClipboardUploader.app 拖到“应用程序”，然后从“应用程序”打开。
- 设置 → 配置 → “登录时自动启动”：通过系统登录项启用，登录后常驻菜单栏。
- 关闭窗口继续后台运行，重新打开应用恢复已有窗口；阻止重复实例和重复监听。
- 保留原有配置与 SQLite 历史记录；后台日志位于 `~/Library/Logs/RustClipboardUploader/`。
- Windows / Linux 保留原有二进制发行方式。

### 签名说明

以下是本次构建的实际签名状态。Ad-hoc 表示没有 Developer ID 公证：首次打开时 macOS 可能阻止运行。确认下载来自本仓库后，可到“系统设置 → 隐私与安全性”选择“仍要打开”。不要关闭系统 Gatekeeper。

