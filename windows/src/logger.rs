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
//!
//! ## 只在 debug 构建落盘
//!
//! Release 构建**完全不写盘**（不建目录、不 `metadata`、不 open/rename），
//! 由 [`ENABLED`] 决定：`cfg!(debug_assertions)` 为假时 [`init`] / [`write_line`] /
//! [`install_panic_hook`] 一律首行早退，且 `logf!` 宏本身编译期短路（`cfg!` 是
//! 字面 `false`，LLVM 直接删掉整个分支，格式化开销也一起消失）。
//! 目的：发布版不给用户机器制造任何日志写入/Disk I/O，也不用担心日志文件被
//! 杀软或同步盘扫描。需要现场排障证据时用 `cargo build`（debug）出的二进制。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use windows::Win32::System::SystemInformation::GetLocalTime;

/// 落盘日志总开关：**仅 debug 构建**开启。
///
/// Release 下为 `false`，`init()` 不碰磁盘（连目录都不创建），`logf!` 宏在编译期
/// 就被优化成空语句——保证发布版的日志相关 Disk I/O 恒为 0。
pub const ENABLED: bool = cfg!(debug_assertions);

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
    /// 轮转失败只影响日志本身，绝不 panic、绝不影响主流程；失败时改为
    /// 原地截断（见 [`Self::truncate_in_place`]），以保证 2 MiB 上限仍然成立。
    fn append(&mut self, line: &str) {
        let len = line.len() as u64;
        if self.bytes + len > MAX_BYTES && !self.rotate() {
            // 轮转失败（备份文件被杀软/其他进程占住）时不能假装成功：
            // 把 `bytes` 清零会让计数与文件真实大小脱钩，之后的写入再也不
            // 触发轮转，文件便无上限地长下去——与模块承诺的 2 MiB 上限相惖。
            // 退一步：原地截断（内容本来也转不出去，保留一半旧日志没有意义）。
            // 连截断都失败才保留计数，让下一次写入再试一次。
            eprintln!("[{}] [Logger] 轮转失败，原地截断日志", timestamp());
            self.truncate_in_place();
        }
        if self.file.write_all(line.as_bytes()).is_ok() {
            self.bytes += len;
        }
    }

    /// 轮转失败时的兜底：清空主文件并重新打开追加句柄。
    ///
    /// 不能直接 `self.file.set_len(0)`：Windows 下以 `FILE_APPEND_DATA` 打开的
    /// 句柄不允许 `SetEndOfFile`（会 ERROR_ACCESS_DENIED），必须借一个新句柄
    /// 把文件截成 0。旧内容本来就轮转不出去，保留一半旧日志没有意义——目标
    /// 是让 2 MiB 上限继续成立。
    fn truncate_in_place(&mut self) -> bool {
        let _ = self.file.flush();
        // 这个句柄只为截断；drop 之后才能把追加句柄换回去
        let truncated = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
            .is_ok();
        if !truncated {
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
///
/// Release 构建（[`ENABLED`] == false）直接返回：不创建目录、不读 `metadata`、
/// 不轮转、不打开文件，`LOG`/`LOG_PATH` 保持空，[`path`] 因此返回 `None`。
pub fn init() {
    if !ENABLED {
        return;
    }
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
///
/// Release 构建下为空操作（连时间戳与字符串格式化都跳过）；正常情况下
/// `logf!` 宏已在编译期把调用点整段删掉，这里的早退只是兜底。
pub fn write_line(args: std::fmt::Arguments<'_>) {
    if !ENABLED {
        return;
    }
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
    // Release 下没有日志文件、stderr 也被 GUI 子系统丢弃，装 hook 无收益
    if !ENABLED {
        return;
    }
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
        // `cfg!` 是编译期常量：release 下整个分支被删除，连 `format_args!` 的
        // 格式化成本都不存在。实参在语法上仍被"使用"，因此各调用点不会因为
        // 日志被编译掉而报 unused variable / unused_mut。
        if $crate::logger::ENABLED {
            $crate::logger::write_line(format_args!($($arg)*))
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 每个用例独占一个临时目录（并发跑不会互相踩）
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "udp2cal-logtest-{}-{}-{:?}",
            std::process::id(),
            tag,
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// `bytes` 预置成超限，使下一次 `append` 立即触发轮转分支
    fn over_limit_log(dir: &Path, content: &[u8]) -> LogFile {
        let path = dir.join("udp2cal.log");
        std::fs::write(&path, content).expect("写入初始日志");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("打开日志");
        LogFile {
            file,
            dir: dir.to_path_buf(),
            path,
            bytes: MAX_BYTES,
        }
    }

    #[test]
    fn rotate_success_moves_old_file_and_resets_counter() {
        let dir = temp_dir("ok");
        let mut log = over_limit_log(&dir, b"old-content\n");
        log.append("new\n");

        // 旧内容整体搬到备份文件，主文件只剩本次写入
        let rotated = std::fs::read(dir.join(ROTATED_NAME)).expect("备份文件应存在");
        assert_eq!(rotated, b"old-content\n");
        assert_eq!(log.bytes, 4, "计数应只包含新写入的字节数");
        assert_eq!(
            std::fs::metadata(&log.path).map(|m| m.len()).unwrap_or(0),
            4,
            "主文件应只包含新写入的内容"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 轮转失败（备份目标被目录占住 → rename 必然失败）时必须原地截断，
    /// 否则计数与文件大小脱钩，日志文件会突破 2 MiB 上限无限增长。
    #[test]
    fn rotate_failure_truncates_in_place_instead_of_losing_the_cap() {
        let dir = temp_dir("fail");
        // 用目录占住备份名，使 rename 确定性失败
        std::fs::create_dir(dir.join(ROTATED_NAME)).expect("创建占位目录");

        let mut log = over_limit_log(&dir, b"0123456789");
        log.append("new\n");

        let size = std::fs::metadata(&log.path)
            .map(|m| m.len())
            .unwrap_or(u64::MAX);
        assert_eq!(
            size, 4,
            "轮转失败后应原地截断再写入，而不是继续在超限文件尾部追加"
        );
        assert_eq!(log.bytes, 4, "计数必须与文件真实大小一致");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
