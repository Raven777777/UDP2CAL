//! 落盘日志
//!
//! 发布版是 GUI 子系统（`#![windows_subsystem = "windows"]`），进程没有控制台，
//! 所有 `eprintln!` 输出都会被系统丢弃——用户报"没声音/连不上"时没有任何现场证据。
//!
//! 本模块提供最小可用的文件日志：
//! - 路径：`%LOCALAPPDATA%\UDP2CAL\udp2cal.log`
//! - 单文件上限 2 MiB，超限后轮转为 `udp2cal.1.log`（只保留一轮，不会无限增长）
//! - 同时写 stderr：命令行启动（开发期）仍能直接看到
//! - 全局单例 + 互斥锁；调用频率为秒级到分钟级，不在音频回调路径上
//!
//! 用法：`crate::logf!("[Audio] 初始化失败: {e}");`

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use windows::Win32::System::SystemInformation::GetLocalTime;

/// 单文件上限，超过后轮转一次
const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// 轮转目标（只保留一轮）
const ROTATED_NAME: &str = "udp2cal.1.log";

struct LogFile {
    file: File,
    dir: PathBuf,
    path: PathBuf,
    /// 已写入字节数（含启动横幅），用于在**运行期**触发轮转
    bytes: u64,
}

impl LogFile {
    /// 追加一行；超限时先轮转。
    /// 轮转失败只影响日志本身，绝不 panic、绝不影响主流程；
    /// 同时把计数清零，避免每次写入都重试一次 rename。
    fn append(&mut self, line: &str) {
        let len = line.len() as u64;
        if self.bytes + len > MAX_BYTES && !self.rotate() {
            self.bytes = 0;
        }
        if self.file.write_all(line.as_bytes()).is_ok() {
            self.bytes += len;
        }
    }

    /// 覆盖上一轮备份后重建文件。返回是否成功。
    fn rotate(&mut self) -> bool {
        let _ = self.file.flush();
        // Rust 打开文件时带 FILE_SHARE_DELETE，运行中重命名是允许的
        if std::fs::rename(&self.path, self.dir.join(ROTATED_NAME)).is_err() {
            return false;
        }
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(f) => {
                self.file = f;
                self.bytes = 0;
                true
            }
            Err(_) => false,
        }
    }
}

static LOG: OnceLock<Mutex<LogFile>> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// 初始化日志文件。失败时静默降级为"仅 stderr"，不影响主流程。
pub fn init() {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("UDP2CAL");
    if std::fs::create_dir_all(&base).is_err() {
        return;
    }
    let path = base.join("udp2cal.log");

    // 启动时文件已超限：直接轮转，避免把上一轮日志和新会话混在一个文件里
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_BYTES {
            let _ = std::fs::rename(&path, base.join(ROTATED_NAME));
        }
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let banner = format!(
            "===== UDP2CAL {} 启动 (pid {}) =====\n",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        );
        let _ = file.write_all(banner.as_bytes());
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if LOG
            .set(Mutex::new(LogFile {
                file,
                dir: base,
                path: path.clone(),
                bytes,
            }))
            .is_ok()
        {
            let _ = LOG_PATH.set(path);
        }
    }
}

/// 日志文件路径（用于在 UI/关于页展示，未初始化时返回 None）
pub fn path() -> Option<&'static PathBuf> {
    LOG_PATH.get()
}

/// 写入一行日志。任意线程可调用。
pub fn write_line(args: std::fmt::Arguments<'_>) {
    let line = format!("[{}] {}\n", timestamp(), args);
    // 开发期（从命令行启动）仍输出到 stderr；release 下句柄无效，write 会被忽略
    eprint!("{line}");

    let Some(lock) = LOG.get() else { return };
    // 锁中毒也要继续写：日志丢了比锁丢了更糟
    let mut log = match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    log.append(&line);
}

/// 安装 panic hook：把 panic 的线程名/位置/信息（及可用时的 backtrace）落盘。
///
/// 未安装时的后果：发布版是 GUI 子系统，`eprintln!` 被系统丢弃；解码线程或
/// UDP 接收任务 panic 后进程仍活着、`APP_RUNNING` 仍为 1、DISCOVER_REQ 照旧
/// 回 ACK，手机看到“幽灵设备”，而日志里一个字都没有。
///
/// 用 `try_lock` 而不是 `lock`：panic 可能就发生在持日志锁的路径上，
/// `std::sync::Mutex` 不可重入，重入会死锁（或 panic-in-panic 直接 abort）。
/// 抢不到锁就只靠 stderr，绝不能让 hook 本身再出问题。
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let msg = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>").to_string();
        // `capture()` 只在 RUST_BACKTRACE 开启时真正抓栈，其余情况 Display 为空。
        let bt = std::backtrace::Backtrace::capture();
        let line = format!(
            "[{}] [Panic] thread '{}' at {}: {}\n{}",
            timestamp(),
            name,
            loc,
            msg,
            bt
        );
        eprint!("{line}");
        let Some(lock) = LOG.get() else { return };
        if let Ok(mut log) = lock.try_lock() {
            log.append(&line);
        }
    }));
}

/// 本地时间戳，格式 `2026-10-23 20:55:24.123`
fn timestamp() -> String {
    // SAFETY: `GetLocalTime` 无参数、返回值语义（无指针输出），调用总是成功。
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

/// 写一行日志（见模块文档）
#[macro_export]
macro_rules! logf {
    ($($arg:tt)*) => {
        $crate::logger::write_line(format_args!($($arg)*))
    };
}
