//! RTP 媒体承载（RTP version 2 / PT=111 / 48 kHz / 固定 10 ms）
//!
//! 本模块是 v3 媒体面的唯一真源，按职责拆为五个子模块：
//!
//! 1. **线格式**（[`header`]）：[`RtpHeader`] / [`parse`] / [`write_packet`]，标准
//!    12 字节 RTP 固定头，无 CSRC、无扩展、无填充，负载就是 Opus 包本身；
//!    [`extend_seq`] / [`extend_ts`] 用有符号差值把 16/32 位回绕扩展成 64 位
//!    单调计数；
//! 2. **抖动缓冲**（[`jitter`]）：[`JitterBuffer`] 按时间戳存放，给出媒体跨度与
//!    真实占用/连续可解码量；播放决策只看 PCM 提交期限，不看声卡缓冲；
//! 3. **阶跃吸收**（[`step`]）：网络攒批造成的整帧级水位跳变，按整帧裁剪；
//! 4. **漂移校正**（[`drift`]）：目标水位与抗饱和 PI，把长期水位残差换算成
//!    重采样比率；
//! 5. **设备时钟速率**（[`device_rate`]）：从 `(设备累计帧, 单调时刻)` 估计声卡
//!    晶振频差。
//!
//! **接收侧时钟模型（ASRC）**：三个时间参考必须分开——发送端媒体样本时间
//! （RTP 时间戳）、接收端单调时钟（`Instant`）、接收端声卡实际消耗进度
//! （平台层上报的 played/playback-head 帧数）。输出重采样真正需要的比率是
//! "媒体 48 kHz / 声卡实际消耗率"：标称部分由 `source_rate / device_rate`
//! 给出，实际设备钟差由 [`DeviceRateEstimator`] 从 `(设备累计帧, QPC/单调时刻)`
//! 估计。RTP 到达时刻只用于抖动统计与延迟测量，**不进入 ASRC 比率**——
//! 否则 90 ms 级网络攒批会直接变成音高变化；[`DriftController`] 只用**长时间低通后**
//! 的水位残差修相位。
//!
//! **播放决策与设备延迟的关系**：设备里已经排队的音频不会给解码器"再等
//! 一会儿"的余地——下一帧必须现在提交，否则回调只能输出静音。因此 PLC 的
//! 判据是 `ring_ms`（已解码、尚未提交的 PCM）见底，而不是 `ring + device`。

pub mod device_rate;
pub mod drift;
pub mod header;
pub mod jitter;
pub mod step;

pub use device_rate::*;
pub use drift::*;
pub use header::*;
pub use jitter::*;
pub use step::*;

// ═══════════════════════ 媒体时钟 ═══════════════════════

pub const OPUS_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_FRAME_MS: u32 = 10;
pub const OPUS_FRAME_SAMPLES: u32 = 480;

// ── 抖动缓冲参数（规范 7.2）──
pub const FRAME_MS: u32 = OPUS_FRAME_MS;
pub const FRAME_SAMPLES: u32 = OPUS_FRAME_SAMPLES;

#[inline]
pub const fn ms_to_samples(ms: u32) -> u32 {
    ms * FRAME_SAMPLES / FRAME_MS
}

#[inline]
pub const fn samples_to_ms(samples: u64) -> u64 {
    samples * 1000 / OPUS_SAMPLE_RATE as u64
}

/// 媒体样本数 → 秒（速率估计器与日志用，f64 精度）
#[inline]
pub fn samples_to_secs(samples: u64) -> f64 {
    samples as f64 / OPUS_SAMPLE_RATE as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_samples_helpers() {
        assert_eq!(ms_to_samples(10), 480);
        assert_eq!(ms_to_samples(20), 960);
        assert_eq!(ms_to_samples(200), 9600);
        assert_eq!(samples_to_ms(9600), 200);
        assert_eq!(samples_to_ms(480), 10);
    }

    #[test]
    fn scheduler_thresholds_are_ordered() {
        // 目标水位必需覆盖整个流水线，节流点必需在 PLC 提前量之上，
        // 否则"该生产"与"该补 PLC"会互相打架。
        // 常量比较在编译期求值：改错常量直接编译失败，不必等跑测试。
        const { assert!(PLC_LEAD_MS < DECODE_LIMIT_MS) };
        const { assert!(DECODE_LIMIT_MS < WATER_TARGET_MS) };
        const { assert!(JITTER_START_MS < WATER_TARGET_MS) };
    }
}
