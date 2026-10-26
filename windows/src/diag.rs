//! P0 诊断打点（ASRC 重构期专用，可在 P5 之后整体删除）
//!
//! 规范 §三 P0 要求"临时高频打点 water、highest_ts、played_frames 原始序列
//! 60 s"，用来在实机上量出网络抖动 σ_J 与晶振偏差 δ̂，从而决定速率估计
//! 窗口该取 10 s 还是 30 s、以及目标水位/死区该落在哪里。
//!
//! 打开方式：环境变量 `UDP2CAL_ASRC_DIAG` = 采样周期（毫秒，`0` 或缺省＝关闭）。
//! 输出：`%LOCALAPPDATA%\UDP2CAL\asrc_diag.csv`，首行列名，每次启动覆盖重写，
//! 最多记录 [`MAX_SECONDS`] 秒。
//!
//! 关闭时开销只有一个 `Option` 判空：写入路径既不在音频回调里，也不在锁内。

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 单次启动最多记录多久（≈15 000 行，远小于 1 MiB）
const MAX_SECONDS: f64 = 300.0;

/// 列名（与 `main.rs` 解码循环里 `row()` 的字段顺序一一对应）
const HEADER: &str = "t_s,ring_ms,pipe_ms,jb_ms,water_ms,target_ms,adjust_ppm,ff_ppm,sigma_ppm,\
buckets,played_s,highest_ts,next_ts,plc,underrun,trims\n";

/// 采样落盘器（单例由调用方持有，避免全局锁）
pub struct AsrcDiag {
    file: Option<std::fs::File>,
    path: Option<PathBuf>,
    period: Duration,
    next_at: Instant,
    start: Instant,
}

impl AsrcDiag {
    /// 从环境变量构造；未开启时返回一个空壳，后续调用全部是空操作。
    pub fn from_env() -> Self {
        let ms = std::env::var("UDP2CAL_ASRC_DIAG")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let start = Instant::now();
        let mut diag = Self {
            file: None,
            path: None,
            period: Duration::from_millis(ms.max(1)),
            next_at: start,
            start,
        };
        if ms == 0 {
            return diag;
        }

        let dir = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("UDP2CAL");
        if std::fs::create_dir_all(&dir).is_err() {
            return diag;
        }
        let path = dir.join("asrc_diag.csv");
        // 覆盖写：每次启动一份干净记录，避免与上一轮混在一起
        if let Ok(mut file) = std::fs::File::create(&path) {
            if file.write_all(HEADER.as_bytes()).is_ok() {
                diag.file = Some(file);
                diag.path = Some(path);
            }
        }
        diag
    }

    /// 是否已开启
    pub fn is_on(&self) -> bool {
        self.file.is_some()
    }

    /// 输出路径（用于启动日志）
    pub fn path_display(&self) -> String {
        match &self.path {
            Some(p) => p.display().to_string(),
            None => "(未开启)".to_string(),
        }
    }

    /// 采样周期到了吗？到了就把下一次采样点推到 `now + period`。
    pub fn due(&mut self) -> bool {
        if self.file.is_none() {
            return false;
        }
        let now = Instant::now();
        if now < self.next_at {
            return false;
        }
        if self.start.elapsed().as_secs_f64() > MAX_SECONDS {
            self.file = None;
            return false;
        }
        self.next_at = now + self.period;
        true
    }

    /// 写一行：`t_s` 自动加在行首，其余字段由调用方拼接
    pub fn row(&mut self, fields: std::fmt::Arguments<'_>) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let t = self.start.elapsed().as_secs_f64();
        let _ = writeln!(file, "{t:.3},{fields}");
    }
}
