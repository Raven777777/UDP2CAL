//! 阶跃吸收：网络攒批造成的整帧级水位跳变，按整帧裁剪。

use std::time::{Duration, Instant};

use super::FRAME_MS;
use super::jitter::PLC_MAX_MS;

// ── 阶跃吸收参数（方案 C：整帧裁剪）──
//
// 阶跃来自网络：AP 攒批交付时十几帧同时到，`jb` 一次涨 100 ms 以上。比率排水
// 只有 5 ms/s，排 100 ms 要 20 s，期间 PI 一直贴 ±5000 ppm（±8.6 音分）——
// 这就是听感上的"变调"。既然阶跃本身是离散事件，就用**整帧级**硬重同步吃掉它：
// 丢掉 n 帧、时间线硬前移，不产生 PLC、不做样本级丢插（接缝比整帧跳更差）。

/// 单次 tick 内 `jb.water_ms()` 增长超过它 → 只作为诊断，不再单独触发裁剪。
///
/// 一次 90 ms 的网络攒批到货是正常现象：只要缓冲垫吸收了它，水位随后会被
/// 正常消费掉。把"单次增长"当成积压证据会误裁正常网络抖动。
pub const STEP_TRIM_GROWTH_MS: u32 = 80;
/// 总水位超过 `target + 它` 且**持续** [`STEP_TRIM_HOLD_MS`] 才判为积压
pub const STEP_TRIM_EXCESS_MS: u32 = 100;
/// 持续超额的确认时长：网络攒批通常 <200 ms，持续 1.5 s 才说明真的多压了音频
pub const STEP_TRIM_HOLD_MS: u32 = 1500;
/// 阶跃吸收的每分钟限额：超限后**只记录不裁剪**，避免参数过激时听成"跳音"
pub const STEP_TRIM_MAX_PER_MIN: u32 = 3;

/// [`StepAbsorber::decide`] 的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepDecision {
    /// 建议跳过的帧数（0 = 本次不动）
    pub frames: u32,
    /// 命中判据但被每分钟限额压制：**只记录、不裁剪**
    pub limited: bool,
}

impl StepDecision {
    pub const NONE: Self = Self {
        frames: 0,
        limited: false,
    };
}

/// 阶跃吸收的判据 + 限额（方案 C）。
///
/// 单独成形是为了让 Windows（`session/audio.rs` 解码循环）与 Android
/// （`ReverseLink.reversePlayLoop`）用**同一套**阈值与限额，两端不必各写一份。
/// 本结构只看数字、不碰缓冲：调用方拿到 `frames` 后自己调
/// [`JitterBuffer::pull_trim`](super::JitterBuffer::pull_trim)，
/// 因此可以在同一把锁里完成决定、在外面记录日志。
///
/// 判据：真实占槽水位 `occupancy + pipe` > `target + `[`STEP_TRIM_EXCESS_MS`]，并且
/// **连续保持** [`STEP_TRIM_HOLD_MS`]。单次水位增长只用于诊断，不再单独触发：
/// 一次 90 ms 的攒批到货如果被缓冲垫吸收，就不是积压。
///
/// 限额：每 60 s 最多裁剪 [`STEP_TRIM_MAX_PER_MIN`] 次；超限后仍返回建议帧数，
/// 但 `limited = true`，调用方必须**不裁剪只记录**。
///
/// 调用方还必须把“刚发生过 PLC/欠载”的缓冲排除在裁剪之外（`can_trim=false`）：
/// 欠载后的高水位多半是恢复回填，不是真的多压了音频。
pub struct StepAbsorber {
    /// 第一次 `decide` 只登记基准，不把启动爬升当成积压
    primed: bool,
    /// 连续超额开始时刻；跌破阈值/不可裁剪时清零
    over_since: Option<Instant>,
    window_start: Instant,
    trims_in_window: u32,
    /// 本窗口内的限额日志是否已经打过（节流用，见 [`StepAbsorber::take_limited_log`]）
    limited_logged: bool,
    /// 命中“持续超额”的次数（含被限额压制的，诊断）
    pub hits: u64,
}

impl StepAbsorber {
    pub fn new() -> Self {
        Self {
            primed: false,
            over_since: None,
            window_start: Instant::now(),
            trims_in_window: 0,
            limited_logged: false,
            hits: 0,
        }
    }

    /// 复位（重连 / 重同步 / 停止）：清掉判据基准与限额窗口
    pub fn reset(&mut self) {
        self.primed = false;
        self.over_since = None;
        self.window_start = Instant::now();
        self.trims_in_window = 0;
        self.limited_logged = false;
    }

    /// 当前 60 s 窗口内已用掉的裁剪次数（诊断，见规范 §7.13）
    pub fn trims_in_window(&self) -> u32 {
        self.trims_in_window
    }

    /// 限额压制日志的节流闸：**每个 60 s 窗口只放行一次**。
    ///
    /// 被压制时水位并不会自己降下来，所以判据每个 tick 都会再次命中；
    /// 不节流的话 Windows 编解码循环（≈250 次/s）与 Android 反向播放
    /// 线程（≈200 次/s）会把日志刷爆，把真正有用的 `[Latency]`/`[Reverse]`
    /// 挤出轮转窗口。返回 true 表示“本次可以打一条”。
    pub fn take_limited_log(&mut self) -> bool {
        if self.limited_logged {
            false
        } else {
            self.limited_logged = true;
            true
        }
    }

    /// 决定本 tick 要不要裁剪（使用当前时刻，供生产代码调用）。
    pub fn decide(
        &mut self,
        jb_water_ms: u32,
        pipe_ms: u32,
        can_trim: bool,
        target_ms: u32,
    ) -> StepDecision {
        self.decide_at(jb_water_ms, pipe_ms, can_trim, target_ms, Instant::now())
    }

    /// 可注入时钟的版本：单元测试据此验证“持续 1.5 s 才裁剪”，不必真的 sleep。
    pub fn decide_at(
        &mut self,
        jb_water_ms: u32,
        pipe_ms: u32,
        can_trim: bool,
        target_ms: u32,
        now: Instant,
    ) -> StepDecision {
        if !self.primed {
            self.primed = true;
            return StepDecision::NONE;
        }
        if !can_trim {
            self.over_since = None;
            return StepDecision::NONE;
        }
        let total = jb_water_ms.saturating_add(pipe_ms);
        let over = total > target_ms.saturating_add(STEP_TRIM_EXCESS_MS);
        if !over {
            self.over_since = None;
            return StepDecision::NONE;
        }
        let since = *self.over_since.get_or_insert(now);
        if now.saturating_duration_since(since) < Duration::from_millis(STEP_TRIM_HOLD_MS as u64) {
            return StepDecision::NONE;
        }
        self.hits += 1;
        if now.saturating_duration_since(self.window_start) >= Duration::from_secs(60) {
            self.window_start = now;
            self.trims_in_window = 0;
            self.limited_logged = false;
        }
        // 只跳到目标：向上取整到整帧，且不超过 PLC 上限（避免一次跳掉半秒）
        let excess = total.saturating_sub(target_ms);
        let frames = excess.div_ceil(FRAME_MS).min(PLC_MAX_MS / FRAME_MS);
        if frames == 0 {
            return StepDecision::NONE;
        }
        // 裁剪后必须重新积累一个完整的确认窗口，避免连续 tick 连跳
        self.over_since = None;
        if self.trims_in_window >= STEP_TRIM_MAX_PER_MIN {
            return StepDecision {
                frames,
                limited: true,
            };
        }
        self.trims_in_window += 1;
        StepDecision {
            frames,
            limited: false,
        }
    }
}

impl Default for StepAbsorber {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_absorber_ignores_normal_water_noise() {
        let mut a = StepAbsorber::new();
        let t0 = Instant::now();
        // 稳态：水位在目标 ±2 帧内跳，总量远低于 target+EXCESS
        for (i, w) in [150u32, 160, 140, 155, 145, 150].into_iter().enumerate() {
            let d = a.decide_at(w, 44, true, 150, t0 + Duration::from_secs(i as u64));
            assert_eq!(d, StepDecision::NONE);
        }
        assert_eq!(a.hits, 0);
    }

    #[test]
    fn step_absorber_waits_for_sustained_excess() {
        let mut a = StepAbsorber::new();
        let t0 = Instant::now();
        assert_eq!(a.decide_at(150, 44, true, 150, t0), StepDecision::NONE);
        // 一次 120 ms 攒批到货：不能立刻裁，先确认它是否被消费掉
        assert_eq!(a.decide_at(270, 44, true, 150, t0), StepDecision::NONE);
        assert_eq!(
            a.decide_at(270, 44, true, 150, t0 + Duration::from_millis(1000)),
            StepDecision::NONE
        );
        // 持续 1.5 s 仍是 high → 说明真的多压了音频
        let d = a.decide_at(270, 44, true, 150, t0 + Duration::from_millis(1600));
        assert!(!d.limited);
        assert_eq!(d.frames, PLC_MAX_MS / FRAME_MS);
        assert_eq!(a.hits, 1);
    }

    #[test]
    fn step_absorber_clears_timer_when_water_falls_back() {
        let mut a = StepAbsorber::new();
        let t0 = Instant::now();
        a.decide_at(150, 44, true, 150, t0);
        a.decide_at(270, 44, true, 150, t0);
        // 攒批被缓冲吸收，总水位回到阈值内：计时清零
        a.decide_at(160, 44, true, 150, t0 + Duration::from_millis(1000));
        let d = a.decide_at(270, 44, true, 150, t0 + Duration::from_millis(1600));
        assert_eq!(d, StepDecision::NONE, "必须重新积累完整确认窗口");
    }

    #[test]
    fn step_absorber_trims_when_water_exceeds_target_window() {
        let mut a = StepAbsorber::new();
        let t0 = Instant::now();
        a.decide_at(150, 44, true, 150, t0);
        assert_eq!(
            a.decide_at(190, 60, true, 150, t0 + Duration::from_millis(1600)),
            StepDecision::NONE
        );
        a.decide_at(210, 60, true, 150, t0 + Duration::from_millis(1700));
        let d = a.decide_at(210, 60, true, 150, t0 + Duration::from_millis(3300));
        assert_eq!(d.frames, 12);
    }

    #[test]
    fn step_absorber_respects_the_per_minute_limit() {
        let mut a = StepAbsorber::new();
        let t0 = Instant::now();
        let mut trimmed = 0;
        let mut limited = 0;
        for i in 0..(STEP_TRIM_MAX_PER_MIN + 2) {
            let base = t0 + Duration::from_millis(2000 + i as u64 * 2000);
            a.decide_at(150, 44, true, 150, base);
            a.decide_at(270, 44, true, 150, base);
            let d = a.decide_at(270, 44, true, 150, base + Duration::from_millis(1600));
            if d.frames == 0 {
                continue;
            }
            if d.limited {
                limited += 1;
            } else {
                trimmed += 1;
            }
        }
        assert_eq!(trimmed, STEP_TRIM_MAX_PER_MIN as i32);
        assert!(limited >= 1, "超限必须只记录不裁剪");
    }

    #[test]
    fn step_absorber_only_acts_once_playing() {
        let mut a = StepAbsorber::new();
        let t0 = Instant::now();
        // 预缓冲期水位从 0 爬升：不得当成积压
        assert_eq!(a.decide_at(210, 44, false, 150, t0), StepDecision::NONE);
        assert_eq!(a.hits, 0);
        // 进入 PLAYING 后也必须连续超额 1.5 s
        assert_eq!(
            a.decide_at(210, 44, true, 150, t0 + Duration::from_millis(1600)),
            StepDecision::NONE
        );
        let d = a.decide_at(210, 44, true, 150, t0 + Duration::from_millis(3200));
        assert_eq!(d.frames, 11);
    }
}
