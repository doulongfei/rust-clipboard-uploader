# 任务：修复 rust-clipboard-uploader，升级到 v0.3.6

项目路径：/home/wave001/.openclaw/workspace/rust-clipboard-uploader

## 修复清单

### 🔴 Bug 修复

**1. 去掉冗余事件，TaskProgress 驱动所有状态**
- 删除 `AppEvent::UploadSuccess` 和 `AppEvent::UploadError`
- `do_upload_task` 只发 `TaskProgress(id, Success(url, src))` 或 `TaskProgress(id, Failed(msg))`
- 主线程收到 `TaskProgress(id, Success)` 时：更新 `last_url`、`status`("上传成功")、刷新 history、如果配置了自动复制则复制、如果配置了通知则通知
- 主线程收到 `TaskProgress(id, Failed)` 时：更新 `status`(错误信息)
- `TaskStatus::Success` 改为携带 `(String, String)` 即 (url, src)

**2. uploading 标志统一**
- 删除 `AppState.uploading: bool` 字段
- 改为每帧计算：`let has_uploading = self.tasks.iter().any(|t| matches!(t.status, TaskStatus::Uploading));`
- 按钮禁用条件用 `has_uploading`
- `trigger_upload` 里去掉 `self.uploading = true/false`

**3. 任务完成后释放 image_data**
- `TaskStatus` 变为 `Success` 或 `Failed` 时，将对应 task 的 `image_data` 清空：`task.image_data = Vec::new()`
- 在主线程收到 TaskProgress 时执行这个清空操作
- 重试时：若 image_data 为空则重新读剪贴板；非空则用存储数据

**4. watch 线程未启用时减少 CPU 占用**
- auto_watch 为 false 时 sleep 2000ms，为 true 时 sleep 500ms

### 🟡 体验优化

**5. 状态提示自动消失**
- `AppState` 新增 `status_clear_at: Option<std::time::Instant>` 字段
- 设置以下状态时同时设 `status_clear_at = Some(Instant::now() + Duration::from_secs(3))`：
  - "配置已保存"、"已复制到剪贴板"、"上传成功"/"上传成功，链接已复制"
- 不自动消失的：上传失败、剪贴板无图片
- 在 `update()` 最开头加检查：若 `status_clear_at` 已过期则清除 status

**6. 任务卡片加时间戳**
- `UploadTask` 新增 `created_at: String` 字段（格式 `%H:%M:%S`）
- `spawn_task` 中赋值：`chrono::Local::now().format("%H:%M:%S").to_string()`
- 任务卡片左侧显示时间戳（小字灰色）

**7. 配置未保存提示**
- `AppState` 新增 `config_dirty: bool` 字段，初始 false
- 配置 Grid 每个控件加 `.changed()` 检测，变化时设 `config_dirty = true`
- 保存按钮标签：`if self.config_dirty { "💾 保存配置 ●" } else { "💾 保存配置" }`
- 保存成功后 `config_dirty = false`

### 🟢 架构优化

**8. SQLite 连接复用**
- `AppState` 新增 `db: Option<Connection>` 字段
- AppState 构造时 `db: open_db()`
- 主线程的 db 操作（db_load, db_insert, db_delete, db_clear）改为直接用 `self.db.as_ref()`
- 后台线程 `do_upload_task` 自己调 `open_db()` （Connection 不是 Send）
- 重构 db 函数签名接收 `&Connection`

## 完成步骤

1. 修改 src/main.rs 实现上述所有修复
2. `source ~/.cargo/env && cargo check` 确保零错误零警告
3. 更新 Cargo.toml version 为 "0.3.6"
4. `git add -A && git commit -m "fix: unify task state, auto-clear status, task timestamps, config dirty, db reuse (v0.3.6)"`
5. `git tag v0.3.6 && git push origin master --tags`
