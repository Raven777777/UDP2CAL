//! 诊断计数器与周期统计日志。
//!
//! 纯遥测原子量（电平、延迟、收包计数）集中在这里；会话级计数器
//! （音频 / 抖动缓冲 / 反向链路）随 `Session` 走，日志函数显式接收它。

use crate::logf;
use crate::protocol;
use crate::session::Session;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

/// 音频电平（dBFS），解码线程每 100 ms 更新一次
pub(crate) static AUDIO_LEVEL_DB: AtomicU32 = AtomicU32::new((-60.0f32).to_bits());

/// 总延迟估算（ms）：`now − 播放头那一帧到达本机的时刻 + 声卡自报延迟`。
///
/// 口径说明：这是接收端**唯一**能在本地测到的延迟——发送端采集到发包、
/// 以及单程网络时延，在没有跨设备时钟的情况下无法测出，不计入。
/// 它等于"此刻正在从音箱出来的那个样本，是多久以前到达网卡的"，
/// 也就是缓冲水位 + 声卡缓冲的真实合并值（比水位更接近听感延迟）。
pub(crate) static LATENCY_MS: AtomicU32 = AtomicU32::new(0);
/// 声卡自报的缓冲延迟（ms），供 UI/日志解释"总延迟"的构成
pub(crate) static DEVICE_LATENCY_MS: AtomicU32 = AtomicU32::new(0);
/// 当前重采样比率微调量（f32 位模式，±0.005）
pub(crate) static DRIFT_ADJUST_BITS: AtomicU32 = AtomicU32::new(0);

/// 用 Opus 带内 FEC 重建成功的帧数（每个统计窗口清零，诊断用）
pub(crate) static FEC_FRAMES: AtomicU64 = AtomicU64::new(0);

// ── 解码线程水位/比率快照 ──
//
// 解码线程只写原子量，日志统一由 UI 线程在 `log_periodic_stats` 里输出：
// 实时线程每 10 s 同步写盘，在慢盘/日志轮转时会阻塞超过 PCM 环的 40 ms，
// 造成周期性的可闻欠载（capture.rs 同样禁止发送线程写日志）。
pub(crate) static DECODE_JB_WATER_MS: AtomicU32 = AtomicU32::new(0);
pub(crate) static DECODE_PIPE_MS: AtomicU32 = AtomicU32::new(0);
pub(crate) static DECODE_WATER_TARGET_MS: AtomicU32 = AtomicU32::new(0);
pub(crate) static DECODE_DEVICE_MS: AtomicU32 = AtomicU32::new(0);
/// 反馈 Δu / 前馈 r̂ / σ（ppm，f64 位模式）
pub(crate) static DECODE_DU_PPM_BITS: AtomicU64 = AtomicU64::new(0);
pub(crate) static DECODE_FF_PPM_BITS: AtomicU64 = AtomicU64::new(0);
pub(crate) static DECODE_SIGMA_PPM_BITS: AtomicU64 = AtomicU64::new(0);
pub(crate) static DECODE_BUCKETS: AtomicU32 = AtomicU32::new(0);

/// 阶跃吸收事件快照：`STEP_EVENT` 是类型，其余字段是一次事件的内容。
pub(crate) const STEP_EVENT_NONE: u8 = 0;
pub(crate) const STEP_EVENT_TRIMMED: u8 = 1;
pub(crate) const STEP_EVENT_LIMITED: u8 = 2;
pub(crate) static STEP_EVENT: AtomicU8 = AtomicU8::new(STEP_EVENT_NONE);
pub(crate) static STEP_BEFORE_MS: AtomicU32 = AtomicU32::new(0);
pub(crate) static STEP_WANT: AtomicU32 = AtomicU32::new(0);
pub(crate) static STEP_SKIPPED: AtomicU32 = AtomicU32::new(0);
pub(crate) static STEP_WATER_MS: AtomicU32 = AtomicU32::new(0);
pub(crate) static STEP_TRIMS: AtomicU64 = AtomicU64::new(0);
pub(crate) static STEP_WINDOW_TRIMS: AtomicU32 = AtomicU32::new(0);

/// RTP 接收计数（诊断用）
pub(crate) static RTP_RX_PACKETS: AtomicU64 = AtomicU64::new(0);
pub(crate) static RTP_RX_DROPPED: AtomicU64 = AtomicU64::new(0);

/// 每 30 s 落一次诊断日志：音频计数器 + RTP/抖动缓冲计数器 + 反向链路。
/// 调用方负责计时（UI 线程，避免在实时线程里做文件 I/O）。
pub(crate) fn log_periodic_stats(session: &Session) {
    let s = session.audio_stats().snapshot();
    let (j, water, lost, expected) = {
        let jb = session.jitter();
        (
            jb.stats,
            jb.water_ms(),
            jb.lost_estimate(),
            jb.expected_packets(),
        )
    };
    let peer = session.p2p();
    logf!(
        "[Audio] 统计 块={} 重采样={} 丢样={} 满丢旧数据={} 欠载={}",
        s.writes,
        s.resampled,
        s.dropped,
        s.flush_on_full,
        s.underrun
    );
    logf!(
        "[Audio] 延迟≈{}ms（含声卡 {}ms）比率微调 {:+.3}%",
        LATENCY_MS.load(Ordering::Relaxed),
        DEVICE_LATENCY_MS.load(Ordering::Relaxed),
        f32::from_bits(DRIFT_ADJUST_BITS.load(Ordering::Relaxed)) * 100.0
    );
    // 解码线程快照（原每 10 s 的 [Latency] 行，搬到 UI 线程输出）
    let jb_water = DECODE_JB_WATER_MS.load(Ordering::Relaxed);
    let pipe_ms = DECODE_PIPE_MS.load(Ordering::Relaxed);
    let total_water = jb_water.saturating_add(pipe_ms);
    logf!(
        "[Latency] 总延迟≈{}ms（水位 {}ms = 抖动 {} + 解码 {}，目标 {}）声卡 {}ms 比率 {:+.3}%（反馈 {:+.0}ppm / 前馈 {:+.1}ppm，σ {:.1}ppm，{} 桶，阶跃裁剪 {} 帧）",
        LATENCY_MS.load(Ordering::Relaxed),
        total_water,
        jb_water,
        pipe_ms,
        DECODE_WATER_TARGET_MS.load(Ordering::Relaxed),
        DECODE_DEVICE_MS.load(Ordering::Relaxed),
        f32::from_bits(DRIFT_ADJUST_BITS.load(Ordering::Relaxed)) * 100.0,
        f64::from_bits(DECODE_DU_PPM_BITS.load(Ordering::Relaxed)) * 1e6,
        f64::from_bits(DECODE_FF_PPM_BITS.load(Ordering::Relaxed)) * 1e6,
        f64::from_bits(DECODE_SIGMA_PPM_BITS.load(Ordering::Relaxed)),
        DECODE_BUCKETS.load(Ordering::Relaxed),
        j.step_trims,
    );
    // 阶跃吸收事件（原解码线程内联日志，同样搬到 UI 线程）
    match STEP_EVENT.swap(STEP_EVENT_NONE, Ordering::Acquire) {
        STEP_EVENT_TRIMMED => {
            let skipped = STEP_SKIPPED.load(Ordering::Relaxed);
            logf!(
                "[Step] 阶跃吸收：整帧跳 {} / {} 帧（{}ms），水位 {}→{}ms，累计 {} 帧",
                skipped,
                STEP_WANT.load(Ordering::Relaxed),
                skipped * protocol::rtp::FRAME_MS,
                STEP_BEFORE_MS.load(Ordering::Relaxed),
                STEP_WATER_MS.load(Ordering::Relaxed),
                STEP_TRIMS.load(Ordering::Relaxed)
            );
        }
        STEP_EVENT_LIMITED => {
            logf!(
                "[Step] 水位 {}ms（目标 {}）命中阶跃吸收，但每分钟 {} 次限额已满（本窗口已用 {} 次）：本次只记录不裁剪",
                STEP_BEFORE_MS.load(Ordering::Relaxed),
                DECODE_WATER_TARGET_MS.load(Ordering::Relaxed),
                protocol::rtp::STEP_TRIM_MAX_PER_MIN,
                STEP_WINDOW_TRIMS.load(Ordering::Relaxed)
            );
        }
        _ => {}
    }
    logf!(
        "[RTP] 收={} 字节={} 迟到={} 重复={} 溢出={} 重同步={} PLC={}(hold {}) 峰值={} FEC={} 水位={}ms SSRC={:#010x}",
        j.received,
        j.bytes,
        j.late,
        j.duplicate,
        j.overflow,
        j.resyncs,
        j.plc_frames,
        j.hold_frames,
        j.max_plc_run,
        FEC_FRAMES.swap(0, Ordering::Relaxed),
        water,
        peer.forward_ssrc
    );
    // 丢包估计 = 序号跨度 − 成功入槽数（自上次重同步起）：把“真丢”
    // 与本地丢弃（迟到/溢出的计数在旁边）分开看，才谈得上调水位/开 FEC。
    let loss_pct = if expected > 0 {
        lost as f64 * 100.0 / expected as f64
    } else {
        0.0
    };
    logf!(
        "[RTP] 丢包≈{}/{}（{:.1}%）：分子含未到达与到达但未入槽（迟到/溢出/重复）",
        lost,
        expected,
        loss_pct
    );
    logf!(
        "[RTP] 丢包/非法包={} 累计接收={}",
        RTP_RX_DROPPED.load(Ordering::Relaxed),
        RTP_RX_PACKETS.load(Ordering::Relaxed)
    );
    // 反向链路统计由 UI 线程汇总：发送线程内**不得**做文件 I/O，
    // 否则一次慢写盘就会打断 RTP（实测每 30 s 一次破音）
    let r = session.take_reverse_stats();
    logf!(
        "[Reverse] 30s: 包={} 字节={} 编码错误={} 超限={} 丢样={} 发送错误={} 发送中断={} 帧={} 均值us(wait/read/encode/send)={:.0}/{:.0}/{:.0}/{:.0} 积压峰值={}ms",
        r.packets,
        r.bytes,
        r.encode_errors,
        r.oversize,
        r.dropped_samples,
        r.send_errors,
        r.gaps,
        r.frames,
        r.avg_us(r.wait_us),
        r.avg_us(r.read_us),
        r.avg_us(r.encode_us),
        r.avg_us(r.send_us),
        r.max_backlog / 2 * 10 / 480
    );
    // 本地丢样与编码超限是“本机造成的丢帧”（CPU 被抢占导致
    // 装配缓冲写满、或码率超过 1275 字节/帧上限），不是网络丢包：
    // 只在实际发生时单独告警，否则埋在 30 s 汇总行末尾等于看不见。
    if r.dropped_samples > 0 || r.oversize > 0 {
        logf!(
            "[Reverse] 警告: 本地丢样={}ms 编码超限={}帧 —— 丢样看 CPU 抢占/装配缓冲，超限看码率是否超过 1275 字节/帧",
            r.dropped_samples / 2 * 10 / 480,
            r.oversize
        );
    }
    // 节拍迟到量（借鉴 Sunshine 的 "Sleep overshoot"）：
    // 只报 min/max/avg，不做 EWMA——偶发的百毫秒级卡顿才是最该看见的。
    if r.late_n > 0 || r.reanchors > 0 {
        logf!(
            "[Pacer] 30s: 节拍迟到 min/avg/max={}/{}/{}us 帧={} 重锚={}",
            r.late_min_us,
            r.late_avg_us(),
            r.late_max_us,
            r.late_n,
            r.reanchors
        );
    }
}
