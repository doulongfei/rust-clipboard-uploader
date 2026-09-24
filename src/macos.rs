//! macOS application lifecycle and login items. Native UI APIs run on the main thread.
use anyhow::{Context, Result};
use fs2::FileExt;
use std::ffi::{c_char, CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

extern "C" {
    fn rcu_is_bundle() -> bool;
    fn rcu_install_events(callback: extern "C" fn());
    fn rcu_activate();
    fn rcu_prepare_window();
    fn rcu_finish_first_frame();
    fn rcu_use_regular_policy();
    fn rcu_login_status() -> i32;
    fn rcu_set_login(enabled: bool, message: *mut c_char, capacity: usize) -> i32;
    fn rcu_open_login_settings();
    fn rcu_show_error(message: *const c_char);
}

static CONTEXT: OnceLock<egui::Context> = OnceLock::new();
static SHOW_WINDOW: AtomicBool = AtomicBool::new(false);

extern "C" fn request_show() {
    SHOW_WINDOW.store(true, Ordering::Release);
    wake_ui();
}

pub fn wake_ui() {
    if let Some(ctx) = CONTEXT.get() {
        ctx.request_repaint();
    }
}

pub fn attach_context(ctx: &egui::Context) {
    let _ = CONTEXT.set(ctx.clone());
    if SHOW_WINDOW.load(Ordering::Acquire) {
        wake_ui();
    }
}

pub fn take_show_request() -> bool {
    SHOW_WINDOW.swap(false, Ordering::AcqRel)
}

pub fn is_bundle() -> bool {
    unsafe { rcu_is_bundle() }
}

pub fn install_events() {
    unsafe { rcu_install_events(request_show) }
}

pub fn activate() {
    unsafe { rcu_activate() }
}

pub fn prepare_window() {
    unsafe { rcu_prepare_window() }
}

pub fn finish_first_frame() {
    unsafe { rcu_finish_first_frame() }
}

pub fn use_regular_policy() {
    unsafe { rcu_use_regular_policy() }
}

pub fn login_status() -> i32 {
    unsafe { rcu_login_status() }
}

pub fn set_login(enabled: bool) -> Result<()> {
    let mut message = [0 as c_char; 1024];
    if unsafe { rcu_set_login(enabled, message.as_mut_ptr(), message.len()) } != 0 {
        anyhow::bail!(unsafe { CStr::from_ptr(message.as_ptr()) }
            .to_string_lossy()
            .into_owned());
    }
    Ok(())
}

pub fn open_login_settings() {
    unsafe { rcu_open_login_settings() }
}

pub fn show_error(message: &str) {
    if let Ok(message) = CString::new(message) {
        unsafe { rcu_show_error(message.as_ptr()) }
    }
}

pub fn init_logging() -> Result<()> {
    let dir = directories::BaseDirs::new()
        .context("无法确定用户目录")?
        .home_dir()
        .join("Library/Logs/RustClipboardUploader");
    fs::create_dir_all(&dir)?;
    let path = dir.join("app.log");
    if fs::metadata(&path)
        .map(|m| m.len() > 5 * 1024 * 1024)
        .unwrap_or(false)
    {
        fs::rename(&path, dir.join("app.previous.log"))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    if unsafe { libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub struct InstanceGuard {
    _lock: File,
    socket: PathBuf,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
    }
}

/// Lock ownership prevents duplicate clipboard watchers; IPC also handles direct binary launches.
pub fn acquire_instance(data_dir: PathBuf) -> Result<Option<InstanceGuard>> {
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(data_dir.join("app.lock"))?;
    let mut hash = DefaultHasher::new();
    data_dir.hash(&mut hash);
    let socket = std::env::temp_dir().join(format!("rcu-{:016x}.sock", hash.finish()));
    match lock.try_lock_exclusive() {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            // The first process may still be between taking the lock and binding its socket.
            for _ in 0..20 {
                if let Ok(mut stream) = UnixStream::connect(&socket) {
                    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                    stream.write_all(b"show")?;
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            anyhow::bail!("应用已经运行，但暂时无法恢复窗口。请从菜单栏打开应用。");
        }
        Err(error) => return Err(error.into()),
    }
    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let mut message = [0; 4];
            if stream.read_exact(&mut message).is_ok() && &message == b"show" {
                request_show();
            }
        }
    });
    Ok(Some(InstanceGuard {
        _lock: lock,
        socket,
    }))
}
