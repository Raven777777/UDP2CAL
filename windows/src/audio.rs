// 无锁 PCM 环 + cpal 输出回调
//
// v3 的时钟模型只有一条规则：**声卡是唯一音频时钟**。因此本模块只做四件事：
//   1. 提供一个无锁 SPSC 环，解码线程写、音频回调读；
//   2. 在回调里统计"真正交给声卡的帧数"（`played_frames`）与声卡自报的
//      "回调 → 实际发声"延迟（`device_latency_us`）——水位口径的一部分；
//   3. 把 48 kHz 媒体流按设备采样率做相位连续重采样，比率可被解码线程
//      在 ±0.5% 内微调（漂移校正的**唯一手段**，见 `set_ratio_adjust`）；
//   4. 把"还剩多少毫秒没播"如实报出来（`filled_frames`），供调度与控制器用。
//
// 重采样是多相 FIR（`RESAMPLE_PHASES` × `RESAMPLE_TAPS`）：两点线性插值的
// 幅度响应是 |cos(ω/2)|，相位扫到半个样本时 12 kHz 掉 3 dB、24 kHz 全陷；
// 而比率微调一启动相位就一直在扫。多相 FIR 的误差只取决于相位表密度，
// 与比率无关，也不存在"比率恰好为 1 就直通"的特例。
//
// 本模块**不做**任何离散丢样/插样：水位只由比率这一条连续量控制。
use crate::protocol::rtp;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, SampleFormat, SizedSample, StreamConfig};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::thread::Thread;
use std::time::{Duration, Instant};

/// 声卡时钟采样与本地 `Instant` 的公共基准。
///
/// cpal 的 WASAPI 后端在每次回调里调用 `IAudioClock::GetPosition`，并把
/// 返回的 QPC 位置放进 `OutputCallbackInfo::timestamp().playback`。这里把
/// 第一次回调的 playback 作为零点，之后用 `StreamInstant::duration_since`
/// 把设备播放位置换算到同一条 `Instant` 时间轴上。
static CLOCK_EPOCH: OnceLock<Instant> = OnceLock::new();

// ═══════════════════════════════════════════════════════════════════
// 无锁 SPSC 环形缓冲
// 音频回调线程绝不能等待互斥锁（优先级反转 → 欠载爆音）。
// 单生产者（解码线程）写 head，单消费者（音频回调）写 tail，
// 元素用 AtomicU32 承载 f32 位模式，全部走安全 API，无需 unsafe。
//
// ⚠ 所有权铁律：`head` 只由生产者写，`tail` **只由消费者写**。
// 生产者要丢弃未播数据时必须走 `request_flush()`（写 `flush_to`），
// 由消费者在下次回调时落地。历史实现让生产者直接写 `tail`，破坏了
// 这条不变量：`len()` 的 head/tail 两次读取之间若插入一次生产者 flush，
// 相减会回绕成 `usize::MAX` → `filled_frames()` 恒大于预解码深度 →
// 解码线程永久停产（静默死锁，无日志）。
// ═══════════════════════════════════════════════════════════════════

/// `flush_to` 的"无挂起请求"哨兵
const FLUSH_NONE: usize = usize::MAX;

struct Ring {
    data: Box<[AtomicU32]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
    /// 生产者请求的"丢弃到该位置"；由消费者落地，避免生产者写 `tail`
    flush_to: AtomicUsize,
}

impl Ring {
    /// frames 会自动向上取整到 2 的幂
    fn with_frames(frames: usize) -> Self {
        let cap = frames.max(1024).next_power_of_two();
        let data = (0..cap)
            .map(|_| AtomicU32::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            data,
            mask: cap - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            flush_to: AtomicUsize::new(FLUSH_NONE),
        }
    }

    #[inline]
    fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// 生产：缓冲满时返回 false（由调用方计为丢弃）
    #[inline]
    fn push(&self, sample: f32) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= self.capacity() {
            return false;
        }
        self.data[head & self.mask].store(sample.to_bits(), Ordering::Relaxed);
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// 消费：无数据返回 None
    #[inline]
    fn pop(&self) -> Option<f32> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let bits = self.data[tail & self.mask].load(Ordering::Relaxed);
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(f32::from_bits(bits))
    }

    /// 待播样本数。
    ///
    /// `head` 与 `tail` 是两次独立读取，必须做"读 tail → 读 head → 再读 tail"
    /// 的快照校验：只有两次 tail 一致，`head - tail` 才是有效快照。再做一次
    /// `saturating_sub` 兜底，绝不允许回绕成极大值。
    #[inline]
    fn len(&self) -> usize {
        loop {
            let tail = self.tail.load(Ordering::Acquire);
            let head = self.head.load(Ordering::Acquire);
            if self.tail.load(Ordering::Acquire) != tail {
                continue; // tail 在读取期间变化（flush 落地），重读
            }
            // 挂起的 flush 尚未落地时按落地后的水位报告，避免解码线程误判"已满"
            let pending = self.flush_to.load(Ordering::Acquire);
            let effective = if pending == FLUSH_NONE {
                tail
            } else {
                tail.max(pending.min(head))
            };
            return head.saturating_sub(effective);
        }
    }

    /// 仅由生产者调用：请求丢弃当前已入环、尚未播放的数据。
    /// 实际丢弃由消费者在下次回调时执行（≤1 个回调周期，约 10 ms）。
    #[inline]
    fn request_flush(&self) {
        let head = self.head.load(Ordering::Acquire);
        self.flush_to.store(head, Ordering::Release);
    }

    /// 仅由消费者（音频回调）调用：落地挂起的 flush。
    /// 消费者写自己的 `tail`，维持 `tail <= head` 不变量。
    #[inline]
    fn apply_pending_flush(&self) {
        let target = self.flush_to.swap(FLUSH_NONE, Ordering::Acquire);
        if target != FLUSH_NONE {
            let head = self.head.load(Ordering::Acquire);
            // `tail` 归消费者所有且**单调递增**：只允许前移。
            //
            // 生产者发出请求（`target = head`）到消费者落地之间，消费者可能
            // 还停留在上一次回调里继续 pop（生产者在期间又入环了几帧），
            // 从而已经越过 `target`。此时无条件写 `target` 会把 `tail` 拉回
            // 更早的位置：声卡会重放已经播过的样本（可听得到的一次 10 ms
            // 重播），`len()` 也会虚高，解码线程据此误判水位。
            //
            // `cur <= head` 是既有不变量，`target.min(head) <= head`，
            // 因此 `max` 之后仍满足 `tail <= head`。
            let cur = self.tail.load(Ordering::Relaxed);
            self.tail
                .store(cur.max(target.min(head)), Ordering::Release);
        }
    }
}

pub struct AudioWriter {
    ring: Arc<Ring>,
    pub device_rate: u32,
    /// 诊断计数器：由 UI 线程读取并展示/落盘（见 main.rs 的 AUDIO_STATS）
    pub stats: Arc<AudioStats>,
    /// 声卡已接手的帧数（含欠载时补的静音：它们同样占据媒体时间线）。
    ///
    /// ⚠ 这是"数据被交给驱动"的位置，**不是"已经听到"的位置**：
    /// 回调把 PCM 填进 WASAPI 缓冲区时计数就前移了，而这段数据要再经过
    /// 引擎缓冲 + DMA 才真正出声。二者之差由 `device_latency_frames()` 给出。
    played: Arc<AtomicU64>,
    /// 声卡自报延迟（微秒）：`timestamp().playback - timestamp().callback`。
    /// 这是本进程里唯一能拿到的"playhead 不含的那部分"实测值。
    device_latency_us: Arc<AtomicU64>,
    /// 设备时钟采样：`(设备帧, QPC 时刻相对于 CLOCK_EPOCH 的纳秒)`。
    /// 由音频回调在读取 `IAudioClock` 时间戳的同一刻写入，供前馈估计器
    /// 建立"媒体样本 ↔ 本地 Instant ↔ 设备进度"三者之间的可验证映射。
    device_clock_frames: Arc<AtomicU64>,
    device_clock_ns: Arc<AtomicU64>,
    /// 解码线程句柄：回调每次运行后 unpark 它（原子置位，无分配）
    waker: Arc<OnceLock<Thread>>,

    // ── 相位连续多相 FIR 重采样器 ──
    /// 当前输出点落在输入上的连续位置（源样本，可为负＝仍落在历史段）
    pos: f64,
    /// 固定比率 = 源采样率 / 设备采样率（不含微调）
    base_ratio: f64,
    /// 比率微调量（±0.005），由漂移控制器每 10 ms 写入一次
    adjust: f64,
    /// 当前输入采样率（0 = 尚未收到数据）
    source_rate: u32,
    /// 上一块的尾段（`RESAMPLE_HIST` 个源样本），供跨块插值
    hist: Vec<f32>,
    /// `hist ++ pcm` 拼接缓冲，复用避免高频分配
    work: Vec<f32>,
    /// 复用输出缓冲区，消除高频堆分配
    resample_buf: Vec<f32>,
}

// ═══════════════════════════════════════════════════════════════════
// 诊断计数器
// ═══════════════════════════════════════════════════════════════════

#[derive(Default)]
pub struct AudioStats {
    /// 写入环的块数
    pub writes: AtomicU64,
    /// 发生重采样的块数（源采样率 ≠ 设备采样率）
    pub resampled: AtomicU64,
    /// 因环形缓冲满被丢弃的样本数
    pub dropped: AtomicU64,
    /// 欠载填充的样本数（音频回调侧）
    pub underrun: AtomicU64,
}

impl AudioStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            writes: self.writes.load(Ordering::Relaxed),
            resampled: self.resampled.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            underrun: self.underrun.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub writes: u64,
    pub resampled: u64,
    pub dropped: u64,
    pub underrun: u64,
}

impl AudioWriter {
    /// 登记解码线程，供音频回调在每次运行后唤醒它
    pub fn set_waker(&self, thread: Thread) {
        let _ = self.waker.set(thread);
    }

    /// 声卡真实输出位置（设备帧，单调递增）。
    /// 欠载补的静音也计入——播放头必须等于"声卡播到哪里"，否则会永久落后。
    #[inline]
    pub fn played_frames(&self) -> u64 {
        self.played.load(Ordering::Acquire)
    }

    /// 声卡自报的"填缓冲 → 真正发声"延迟（设备帧）。
    ///
    /// 由音频回调从 cpal 的时间戳里读出来（`playback - callback`），
    /// 未测量到时为 0。用它把 `played_frames` 折算成**可听位置**：
    /// `audible = played_frames - device_latency_frames()`。
    #[inline]
    pub fn device_latency_frames(&self) -> u64 {
        let us = self.device_latency_us.load(Ordering::Acquire);
        let rate = self.device_rate.max(1) as u64;
        // 上限 1 秒：时间戳异常时宁可当作"没测到"，也不能让播放头倒退
        (us.saturating_mul(rate) / 1_000_000).min(rate)
    }

    /// 取一次设备时钟采样：`(已消费设备帧, 对应的 QPC/Instant 时刻)`。
    /// 时间来自 cpal 的 `IAudioClock::GetPosition` 时间戳，而不是解码线程
    /// 被调度到的 `Instant::now()`——后者会把线程唤醒抖动误当成设备速率差。
    pub fn device_clock_sample(&self) -> Option<(u64, Instant)> {
        let ns = self.device_clock_ns.load(Ordering::Acquire);
        if ns == 0 {
            return None;
        }
        let frames = self.device_clock_frames.load(Ordering::Acquire);
        let epoch = *CLOCK_EPOCH.get()?;
        Some((frames, epoch + Duration::from_nanos(ns)))
    }

    /// 同上，毫秒（诊断展示用）
    #[inline]
    pub fn device_latency_ms(&self) -> u32 {
        ((self.device_latency_frames() * 1000) / self.device_rate.max(1) as u64) as u32
    }

    /// 环里已排队、尚未被声卡消费的帧数（设备帧）。
    /// 解码线程用它把"预解码深度"钉在几十毫秒内——这正是把 PLC
    /// 与水位绑定的前提：生产速度必须被消费速度约束。
    #[inline]
    pub fn filled_frames(&self) -> usize {
        self.ring.len()
    }

    /// 丢弃未播放数据（重连 / 重同步 / 停止）
    pub fn flush(&mut self) {
        // tail 归音频回调所有：这里只能投递请求，由回调落地
        self.ring.request_flush();
        self.reset_filter();
    }

    /// 清空重采样器状态：相位归零、历史段置零（新数据从静音淡入）
    fn reset_filter(&mut self) {
        self.pos = 0.0;
        for s in self.hist.iter_mut() {
            *s = 0.0;
        }
        self.work.clear();
    }

    /// 设置重采样比率微调量（±0.5%）。
    ///
    /// 这是漂移校正的**主力手段**：比率是连续量，改变它不会在 PCM 里留下
    /// 任何接缝，听感上只是极其缓慢的变调（0.5% ≈ 8.7 音分），
    /// 远低于"丢/插 1 ms"这种离散编辑的可闻度。
    ///
    /// 语义：`实际比率 = 源/设备 × (1 + adjust)`；adjust > 0 → 每秒产出的
    /// 设备帧变少 → 缓冲水位回落。
    pub fn set_ratio_adjust(&mut self, adjust: f64) {
        let a = if adjust.is_finite() {
            adjust.clamp(-rtp::RESAMPLE_MAX_ADJUST, rtp::RESAMPLE_MAX_ADJUST)
        } else {
            0.0
        };
        self.adjust = a;
    }

    /// 当前生效的重采样比率（含微调）
    #[inline]
    pub fn current_ratio(&self) -> f64 {
        (self.base_ratio * (1.0 + self.adjust)).clamp(1e-6, 1e6)
    }

    /// 写入 48 kHz 媒体 PCM（单声道），按设备采样率换算后入环。
    /// 返回**实际写入环的设备帧数**（调用方用它折算比率校正的媒体时间偏移）。
    ///
    /// 比率 = `源/设备 × (1 + adjust)`，`adjust` 由漂移控制器每 10 ms 更新。
    /// 采集/声卡时钟差就是这样被吸收掉的——不需要丢样或插样。
    pub fn write(&mut self, pcm: &[f32], source_rate: u32) -> usize {
        if pcm.is_empty() {
            return 0;
        }
        self.stats.writes.fetch_add(1, Ordering::Relaxed);

        if source_rate > 0 && source_rate != self.source_rate {
            self.source_rate = source_rate;
            let r = source_rate as f64 / self.device_rate as f64;
            self.base_ratio = if (0.1..10.0).contains(&r) { r } else { 1.0 };
            // 换源采样率＝换了一条输入时间线：清空历史段与相位
            self.pos = 0.0;
            self.hist.iter_mut().for_each(|v| *v = 0.0);
        }
        let ratio = self.current_ratio();

        if source_rate != self.device_rate || self.adjust.abs() > 1e-12 {
            self.stats.resampled.fetch_add(1, Ordering::Relaxed);
        }

        // ── 多相 FIR 插值 ──
        //
        // 输入先与上一块的尾段拼成 `work`；`pos` 是当前输出点落在输入上的连续
        // 位置（可为负＝仍落在历史段）。只有当抽头窗口完全落在已知数据内时才
        // 发出样本，因此不存在外插，也不需要"比率恰好为 1 就直通"的分支。
        let n = pcm.len();
        self.work.clear();
        self.work.extend_from_slice(&self.hist);
        self.work.extend_from_slice(pcm);

        self.resample_buf.clear();
        let mut pos = self.pos;
        if n > RESAMPLE_HALF + 1 {
            // 窗口右端不得越出本块：base <= n - HALF - 1
            let limit = n as f64 - RESAMPLE_HALF as f64 - 1.0;
            let table = coef_table();
            while pos < limit {
                let base = pos.floor();
                // pcm[i] 在 work 中的下标是 i + RESAMPLE_HIST，
                // 而抽头窗口从下标 base + 1 - HALF 开始
                let w0 = base as isize + (RESAMPLE_HIST as isize + 1 - RESAMPLE_HALF as isize);
                let frac = pos - base;
                let phase = ((frac * RESAMPLE_PHASES as f64) as usize).min(RESAMPLE_PHASES - 1);
                let coefs = &table[phase];
                let w0 = w0 as usize;
                let mut acc = 0.0f32;
                for (j, &c) in coefs.iter().enumerate() {
                    acc += c * self.work[w0 + j];
                }
                self.resample_buf.push(acc);
                pos += ratio;
            }
        }
        self.pos = pos - n as f64;
        // 防御：连续收到极小输入块时 `pos` 会一路后退，钳在历史段范围内
        let floor_pos = -((RESAMPLE_HALF + 1) as f64);
        if self.pos < floor_pos {
            self.pos = floor_pos;
        }
        // 保存尾段作为下一块的历史
        let len = self.work.len();
        self.hist.copy_from_slice(&self.work[len - RESAMPLE_HIST..]);

        let mut written = 0usize;
        for &s in &self.resample_buf {
            if self.ring.push(s) {
                written += 1;
            } else {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        written
    }
}

// ═══════════════════════════════════════════════════════════════════
// 多相 FIR 插值表
//
// 为什么不用两点线性插值：它的幅度响应是 |cos(ω/2)|，相位差半个样本时
// 12 kHz 掉 3 dB、24 kHz 全陷。而比率微调让相位以 u×Fs 持续扫掠，于是
// 那 3 dB 变成常年高频颤动（u=0.5% 时约 240 Hz）。多相 FIR 把插值误差
// 变成纯相位表密度问题（1/(2×32) = 0.016 样本），与比率无关，
// 12 kHz 的幅度误差约 0.003 dB。
// ═══════════════════════════════════════════════════════════════════

/// 相位分辨率
const RESAMPLE_PHASES: usize = 32;
/// 每相抽头数（偶数，对称）
const RESAMPLE_TAPS: usize = 48;
/// 半窗长
const RESAMPLE_HALF: usize = RESAMPLE_TAPS / 2;
/// 归一化截止频率（cycles/sample）：0.42 × 48 kHz = 20.2 kHz，
/// 加上过渡带后阻带起点约 0.49，折返分量落在带外
const RESAMPLE_CUTOFF: f64 = 0.42;
/// Kaiser 窗 β（≈100 dB 阻带）
const RESAMPLE_BETA: f64 = 10.0;
/// 历史样本数：≥ HALF+1，保证相位滑到 −(HALF+1) 时仍有数据
const RESAMPLE_HIST: usize = RESAMPLE_TAPS + 1;

type CoefTable = [[f32; RESAMPLE_TAPS]; RESAMPLE_PHASES];

/// 零阶修正贝塞尔函数（Kaiser 窗用）。级数展开即可，β ≤ 10 时收敛极快。
fn bessel_i0(x: f64) -> f64 {
    let y = x * x / 4.0;
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    for k in 1..40 {
        term *= y / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// 生成/取用相位表（首次调用时计算，之后只读）
fn coef_table() -> &'static CoefTable {
    static TABLE: OnceLock<CoefTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        let win_norm = bessel_i0(RESAMPLE_BETA);
        let mut table = [[0.0f32; RESAMPLE_TAPS]; RESAMPLE_PHASES];
        let w = 2.0 * RESAMPLE_CUTOFF;
        for (p, coefs) in table.iter_mut().enumerate() {
            let frac = p as f64 / RESAMPLE_PHASES as f64;
            let mut sum = 0.0f64;
            for (j, c) in coefs.iter_mut().enumerate() {
                // 抽头 j 对应的输入样本与插值中心的距离（单位：样本）
                let x = j as f64 - (RESAMPLE_HALF as f64 - 1.0) - frac;
                let sinc = if x.abs() < 1e-9 {
                    1.0
                } else {
                    (std::f64::consts::PI * w * x).sin() / (std::f64::consts::PI * w * x)
                };
                let u = x / RESAMPLE_HALF as f64;
                let win = if u.abs() >= 1.0 {
                    0.0
                } else {
                    bessel_i0(RESAMPLE_BETA * (1.0 - u * u).sqrt()) / win_norm
                };
                let h = w * sinc * win;
                *c = h as f32;
                sum += h;
            }
            // 每相单独归一：保证 DC 增益精确为 1，否则会留下低频起伏
            let inv = 1.0 / sum;
            for c in coefs.iter_mut() {
                *c = (*c as f64 * inv) as f32;
            }
        }
        table
    })
}

/// 优先选择 f32，其次 i16/u16/i32；同等格式下优先单声道（回调会复制到所有声道）
fn pick_output_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfigRange, String> {
    fn rank(f: SampleFormat) -> u8 {
        match f {
            SampleFormat::F32 => 0,
            SampleFormat::I16 => 1,
            SampleFormat::U16 => 2,
            SampleFormat::I32 => 3,
            _ => 9,
        }
    }
    let configs: Vec<_> = device
        .supported_output_configs()
        .map_err(|e| format!("枚举配置失败: {e}"))?
        .filter(|c| rank(c.sample_format()) < 9)
        .collect();
    if configs.is_empty() {
        return Err("无可用输出格式（需要 f32/i16/u16/i32）".into());
    }
    configs
        .into_iter()
        .min_by_key(|c| {
            (
                rank(c.sample_format()),
                if c.channels() == 1 { 0 } else { 1 },
                c.channels(),
            )
        })
        .ok_or_else(|| "无有效输出配置".to_string())
}

/// `build_stream` 的共享观测点集合。
///
/// 七个句柄打包成一个结构，而不是在参数表里排成七行：(1) 回调闭包只捕获一份
/// 数据；(2) 调用点不再有"同为 `&Arc<AtomicU64>` 的两个参数传反"的空间。
#[derive(Clone)]
struct StreamShared {
    ring: Weak<Ring>,
    stats: Weak<AudioStats>,
    played: Weak<AtomicU64>,
    latency: Arc<AtomicU64>,
    clock_frames: Arc<AtomicU64>,
    clock_ns: Arc<AtomicU64>,
    waker: Arc<OnceLock<Thread>>,
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    shared: &StreamShared,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    // 拆包时把 Weak/Arc 按名字绑定到与原签名同名的局部量，
    // 回调体因此保持一字不改。
    let StreamShared {
        ring,
        stats,
        played,
        latency,
        clock_frames,
        clock_ns,
        waker,
    } = shared.clone();
    // 第一次回调的 playback 位置作为设备时钟零点；回调闭包私有，无锁。
    let mut playback_base: Option<cpal::StreamInstant> = None;
    device
        .build_output_stream(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                // 回调约束：无锁、无分配、不调用 Opus、不写日志、不做复杂计算
                let need_frames = data.len() / channels;
                let mut filled = 0usize;
                let mut idx = 0usize;

                // 声卡自报延迟：数据"填进缓冲"到"真正出声"的间隔。
                // WASAPI 侧由 IAudioClock 给出，已包含引擎缓冲与 DMA 排队。
                // 这是回答"playhead_ts 含不含 DMA 缓冲"的唯一实测口径。
                let ts = info.timestamp();
                if let Some(d) = ts.playback.duration_since(&ts.callback) {
                    let us = d.as_micros().min(u64::MAX as u128) as u64;
                    latency.store(us, Ordering::Release);
                }
                // IAudioClock 的播放位置 → 同一条 Instant 时间轴。
                let _ = CLOCK_EPOCH.get_or_init(Instant::now);
                let base = match playback_base {
                    Some(base) => base,
                    None => {
                        playback_base = Some(ts.playback);
                        ts.playback
                    }
                };
                if let Some(d) = ts.playback.duration_since(&base) {
                    clock_ns.store(d.as_nanos().min(u64::MAX as u128) as u64, Ordering::Release);
                }

                // 生产者只能"请求"丢弃（flush），由消费者在这里落地
                if let Some(ring) = ring.upgrade() {
                    ring.apply_pending_flush();
                    for _ in 0..need_frames {
                        match ring.pop() {
                            Some(sample) => {
                                let value = T::from_sample(sample);
                                for _ in 0..channels {
                                    data[idx] = value;
                                    idx += 1;
                                }
                                filled += 1;
                            }
                            None => break,
                        }
                    }
                }

                // 播放头 = 声卡**真实输出位置**。欠载时声卡仍在按实时输出静音，
                // 这些帧同样占据媒体时间线，必须计入：只统计真实 PCM 会让播放头
                // 永久落后欠载量，抖动缓冲的有效水位被被动抬高，PLC 触发点随之
                // 推迟，形成"欠载 → 延迟增大 → 更易欠载"的正反馈。
                if let Some(p) = played.upgrade() {
                    let total =
                        p.fetch_add(need_frames as u64, Ordering::Release) + need_frames as u64;
                    // 与上面的 clock_ns 同属一次回调；前馈只在 1 s 桶里取点，
                    // 两个原子量之间的亚毫秒撕裂不影响斜率。
                    clock_frames.store(total, Ordering::Release);
                }

                if filled < need_frames {
                    data[idx..].fill(T::EQUILIBRIUM);
                    if let Some(st) = stats.upgrade() {
                        st.underrun
                            .fetch_add((need_frames - filled) as u64, Ordering::Relaxed);
                    }
                }

                // 唤醒解码线程：播放头已经前移，该继续拉帧了。
                // `unpark()` 在解码线程未休眠时只是一次原子置位（无锁、无分配）；
                // 只有当它确实处于 park 时才触发一次内核唤醒。
                if let Some(thread) = waker.get() {
                    thread.unpark();
                }
            },
            |_| {},
            None,
        )
        .map_err(|e| format!("创建流失败: {e}"))
}

/// 打开输出设备并启动播放线程。
/// `stats` 由调用方持有（进程级单例），音频回调通过 `Weak` 上报欠载。
pub fn start_audio(stats: Arc<AudioStats>) -> Result<AudioWriter, String> {
    let device = find_vb_cable_device()
        .or_else(|| cpal::default_host().default_output_device())
        .ok_or("无可用音频输出设备")?;

    let supported_config = pick_output_config(&device)?;
    let sample_format = supported_config.sample_format();
    let supported_buffer = *supported_config.buffer_size();

    let mut stream_config: StreamConfig = if supported_config.min_sample_rate()
        <= cpal::SampleRate(48000)
        && supported_config.max_sample_rate() >= cpal::SampleRate(48000)
    {
        supported_config
            .with_sample_rate(cpal::SampleRate(48000))
            .into()
    } else {
        supported_config.with_max_sample_rate().into()
    };

    let actual_rate = stream_config.sample_rate.0;

    // ── 显式 10 ms 输出缓冲 ──
    //
    // 不显式指定时 WASAPI 会给默认引擎周期（Windows 上常见 10–20 ms，
    // 某些驱动 30 ms），播放头的量化粒度随之变粗。这里钉成 10 ms：
    // 缓冲延迟低一档，欠载风险由上游 341 ms 的 PCM 环吸收。
    //
    // ⚠ cpal 0.15.3 的 `buffer_size_to_duration()` 把帧数换算成 hnsBufferDuration
    // 时少乘了 100（480 帧被算成 0.1 ms 而不是 10 ms）。共享模式下 WASAPI 会把
    // 过小的周期向上取整到引擎最小周期，因此实际值不等于这里的请求值——
    // 所以我们**不信任请求值**，而是从 `OutputCallbackInfo` 里实测
    // `playback - callback`（见 `device_latency_us`）。
    let want_frames = (actual_rate / 100).max(1); // 10 ms
    let min_frames = match supported_buffer {
        cpal::SupportedBufferSize::Range { min, .. } => min,
        cpal::SupportedBufferSize::Unknown => 1,
    };
    let buffer_frames = want_frames.max(min_frames);
    stream_config.buffer_size = BufferSize::Fixed(buffer_frames);

    // 容量 341 ms（48 kHz 下 16384 帧），远大于 200 ms 下限；
    // **不预填静音**：预缓冲由抖动缓冲负责，回调在无数据时输出静音且不推进播放头。
    let ring = Arc::new(Ring::with_frames((actual_rate as usize * 341) / 1000));
    let played = Arc::new(AtomicU64::new(0));
    let device_latency_us = Arc::new(AtomicU64::new(0));
    let device_clock_frames = Arc::new(AtomicU64::new(0));
    let device_clock_ns = Arc::new(AtomicU64::new(0));
    let waker: Arc<OnceLock<Thread>> = Arc::new(OnceLock::new());

    let (err_tx, err_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

    let ring_weak = Arc::downgrade(&ring);
    let stats_weak = Arc::downgrade(&stats);
    let played_weak = Arc::downgrade(&played);
    let shared = StreamShared {
        ring: ring_weak,
        stats: stats_weak,
        played: played_weak,
        latency: device_latency_us.clone(),
        clock_frames: device_clock_frames.clone(),
        clock_ns: device_clock_ns.clone(),
        waker: waker.clone(),
    };

    std::thread::Builder::new()
        .name("udp2cal-audio".into())
        .spawn(move || {
            // 固定 10 ms 失败（驱动拒绝该周期）时退回驱动默认值，
            // 绝不让"低延迟优化"变成"打不开设备"
            let mut last_err = String::new();
            let mut chosen = None;
            for bs in [BufferSize::Fixed(buffer_frames), BufferSize::Default] {
                let mut cfg = stream_config.clone();
                cfg.buffer_size = bs;
                let built = match sample_format {
                    SampleFormat::F32 => build_stream::<f32>(&device, &cfg, &shared),
                    SampleFormat::I16 => build_stream::<i16>(&device, &cfg, &shared),
                    SampleFormat::U16 => build_stream::<u16>(&device, &cfg, &shared),
                    SampleFormat::I32 => build_stream::<i32>(&device, &cfg, &shared),
                    other => Err(format!("不支持的输出采样格式: {other}")),
                };
                match built {
                    Ok(s) => {
                        chosen = Some(s);
                        break;
                    }
                    Err(e) => last_err = e,
                }
            }
            let Some(stream) = chosen else {
                let _ = err_tx.send(Err(last_err));
                return;
            };
            if let Err(e) = stream.play() {
                let _ = err_tx.send(Err(format!("播放失败: {e}")));
                return;
            }
            let _ = err_tx.send(Ok(()));
            loop {
                std::thread::park();
            }
        })
        .map_err(|e| format!("线程创建失败: {e}"))?;

    match err_rx.recv() {
        Ok(Ok(())) => Ok(AudioWriter {
            ring,
            device_rate: actual_rate,
            stats,
            played,
            device_latency_us,
            device_clock_frames,
            device_clock_ns,
            waker,
            pos: 0.0,
            base_ratio: 1.0,
            adjust: 0.0,
            source_rate: 0,
            hist: vec![0.0; RESAMPLE_HIST],
            work: Vec::with_capacity(RESAMPLE_HIST + 4096),
            resample_buf: Vec::with_capacity(4096),
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("音频线程异常退出".into()),
    }
}

pub fn detect_vb_cable() -> Option<String> {
    let host = cpal::default_host();
    host.output_devices()
        .ok()?
        .filter_map(|d| d.name().ok())
        .find(|n| n.contains("VB-Audio Virtual Cable"))
}

fn find_vb_cable_device() -> Option<cpal::Device> {
    let host = cpal::default_host();
    for d in host.output_devices().ok()? {
        if let Ok(name) = d.name() {
            if name.contains("VB-Audio Virtual Cable") {
                return Some(d);
            }
        }
    }
    None
}

// ═══════════════════════════════════════════════════════════════════
// 单元测试：环形缓冲与固定比率重采样
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_preserves_order_and_bounds() {
        let ring = Ring::with_frames(1024);
        for i in 0..1024 {
            assert!(ring.push(i as f32));
        }
        assert_eq!(ring.len(), 1024);
        // 满后再写入必须被拒绝，且不覆盖已有数据
        assert!(!ring.push(9999.0));
        for i in 0..1024 {
            assert_eq!(ring.pop(), Some(i as f32));
        }
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn ring_accepts_after_consume() {
        let ring = Ring::with_frames(1024);
        for _ in 0..1024 {
            assert!(ring.push(0.5));
        }
        assert!(!ring.push(0.5));
        assert_eq!(ring.pop(), Some(0.5));
        // 腾出一个位置后应能继续写入
        assert!(ring.push(0.25));
        assert_eq!(ring.len(), 1024);
    }

    #[test]
    fn ring_flush_is_applied_by_consumer() {
        let ring = Ring::with_frames(1024);
        for _ in 0..100 {
            ring.push(0.5);
        }
        // 生产者只投递请求：tail 不动（tail 归消费者所有）
        ring.request_flush();
        assert_eq!(ring.len(), 0, "挂起 flush 后水位应立即按落地后计算");
        // 消费者在回调里落地
        ring.apply_pending_flush();
        assert_eq!(ring.pop(), None);
        assert!(ring.push(0.25));
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn flush_never_rewinds_consumer_position() {
        // 回归：消费者在 flush 落地前已经越过 target 时（同一个回调里继续 pop，
        // 而生产者在期间又入环了几帧），tail 不得被拉回更早的位置。
        let ring = Ring::with_frames(1024);
        for _ in 0..8 {
            ring.push(1.0);
        }
        assert_eq!(ring.pop(), Some(1.0));
        assert_eq!(ring.pop(), Some(1.0));

        ring.request_flush(); // target = head = 8

        // 消费者仍在同一个回调里：生产者又入环 4 帧，消费者把它们全消费掉
        for _ in 0..4 {
            ring.push(2.0);
        }
        while ring.pop().is_some() {}
        let before = ring.tail.load(Ordering::Acquire);
        assert_eq!(before, 12);

        // 下一次回调开头落地 flush：tail 不得回退
        ring.apply_pending_flush();
        let after = ring.tail.load(Ordering::Acquire);
        assert!(
            after >= before,
            "flush 使 tail 回退：{before} → {after}（会重播已播样本）"
        );
        assert_eq!(ring.len(), 0);
    }

    #[test]
    fn ring_flush_keeps_data_pushed_after_request() {
        let ring = Ring::with_frames(1024);
        for _ in 0..100 {
            ring.push(0.5);
        }
        ring.request_flush();
        ring.push(0.25); // flush 之后入环的新数据不得被丢弃
        ring.apply_pending_flush();
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.pop(), Some(0.25));
    }

    #[test]
    fn ring_len_never_wraps_under_flush_race() {
        // 回归：生产者 request_flush 与消费者 pop 并发时，len() 不得回绕。
        // 历史实现由生产者直接写 tail，len() 会得到 usize::MAX，
        // 使 filled_frames() 恒大于预解码深度 → 解码线程永久停产。
        use std::sync::atomic::AtomicBool;
        let ring = std::sync::Arc::new(Ring::with_frames(4096));
        let stop = std::sync::Arc::new(AtomicBool::new(false));

        let r = ring.clone();
        let s = stop.clone();
        let producer = std::thread::spawn(move || {
            let mut v = 0u32;
            while !s.load(Ordering::Relaxed) {
                // 只由生产者写 head 与 flush_to，绝不碰 tail
                if !r.push(v as f32) || v.is_multiple_of(97) {
                    r.request_flush();
                }
                v = v.wrapping_add(1);
            }
        });
        let r = ring.clone();
        let s = stop.clone();
        let consumer = std::thread::spawn(move || {
            let mut max_len = 0usize;
            while !s.load(Ordering::Relaxed) {
                r.apply_pending_flush();
                let _ = r.pop();
                max_len = max_len.max(r.len());
            }
            max_len
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        stop.store(true, Ordering::Relaxed);
        producer.join().unwrap();
        let max_len = consumer.join().unwrap();
        assert!(
            max_len <= ring.capacity(),
            "len() 不得超过容量（实际 {max_len}，容量 {}）",
            ring.capacity()
        );
    }

    impl AudioWriter {
        fn for_test(device_rate: u32) -> Self {
            Self {
                ring: Arc::new(Ring::with_frames(device_rate as usize)),
                device_rate,
                stats: Arc::new(AudioStats::default()),
                played: Arc::new(AtomicU64::new(0)),
                device_latency_us: Arc::new(AtomicU64::new(0)),
                device_clock_frames: Arc::new(AtomicU64::new(0)),
                device_clock_ns: Arc::new(AtomicU64::new(0)),
                waker: Arc::new(OnceLock::new()),
                pos: 0.0,
                base_ratio: 1.0,
                adjust: 0.0,
                source_rate: 0,
                hist: vec![0.0; RESAMPLE_HIST],
                work: Vec::new(),
                resample_buf: Vec::new(),
            }
        }
    }

    // ── 多相 FIR 重采样 ──

    const RATE_12K: f32 = 12_000.0;
    const FS: f32 = 48_000.0;

    /// 12 kHz 正弦（每样本 0.25 周，采样点恰好落在 0/±1 上：
    /// 两点线性插值在相位 0.5 时对它正好 −3 dB，是最能暴露问题的信号）
    fn tone_12k(len: usize, start: usize) -> Vec<f32> {
        (0..len)
            .map(|i| (2.0 * std::f32::consts::PI * RATE_12K * (start + i) as f32 / FS).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        if x.is_empty() {
            return 0.0;
        }
        (x.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn same_rate_write_passes_dc_exactly() {
        // DC 增益必须精确为 1（每相单独归一），否则水位标定会带系统偏差
        let mut aw = AudioWriter::for_test(48000);
        let pcm = vec![0.1f32; 480];
        let mut produced = 0usize;
        for _ in 0..10 {
            produced += aw.write(&pcm, 48000);
        }
        assert!(produced > 0);
        let mut ring = Vec::new();
        while let Some(v) = aw.ring.pop() {
            ring.push(v);
        }
        // 前 25 个样本落在"历史静音 → 信号"的过渡里，其余必须是 0.1
        for (i, v) in ring.iter().enumerate().skip(200) {
            assert!((v - 0.1).abs() < 1e-6, "样本 {i} = {v}");
        }
    }

    #[test]
    fn resampler_outputs_expected_frame_rate() {
        // 48000 源 → 48000 设备：比率 1，输出只慢一个固定群延迟（HALF+1 样本）
        let mut aw = AudioWriter::for_test(48000);
        let pcm = vec![0.1f32; 480];
        let mut total = 0i64;
        for _ in 0..100 {
            total += aw.write(&pcm, 48000) as i64;
        }
        assert!((total - 47_975).abs() <= 3, "1 秒内产出 {total}");
    }

    #[test]
    fn forty_four_one_device_upsamples_48k_input() {
        // 48 kHz 源 → 44.1 kHz 设备：1 秒 48000 样本应产出约 44100 帧
        let mut aw = AudioWriter::for_test(44100);
        let pcm = vec![0.1f32; 480];
        let mut total = 0i64;
        for _ in 0..100 {
            total += aw.write(&pcm, 48000) as i64;
        }
        assert!((total - 44_077).abs() <= 4, "1 秒内产出 {total}");
        assert_eq!(aw.stats.resampled.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn resampler_keeps_phase_across_blocks() {
        // 块间相位必须连续：连续 10 块的总产出不得因取整而漏帧
        let mut aw = AudioWriter::for_test(44100);
        let pcm = vec![0.1f32; 480];
        let mut total = 0i64;
        for _ in 0..10 {
            total += aw.write(&pcm, 48000) as i64;
        }
        assert!((total - 4_386).abs() <= 4, "10 帧产出 {total}");
    }

    #[test]
    fn ratio_adjust_changes_produced_frames() {
        // adjust > 0 → 比率变大 → 每秒**少**产出约 240 帧（水位回落）
        fn one_second(adjust: f64) -> i64 {
            let mut aw = AudioWriter::for_test(48000);
            aw.set_ratio_adjust(adjust);
            let pcm = vec![0.1f32; 480];
            let mut total = 0i64;
            for _ in 0..100 {
                total += aw.write(&pcm, 48000) as i64;
            }
            total
        }
        let fast = one_second(0.005);
        let slow = one_second(-0.005);
        // 两个方向的差值必须等于 2×0.5%×48000 ≈ 480 帧（固定群延迟自动抵消）
        assert!((slow - fast - 480).abs() <= 3, "fast={fast} slow={slow}");
        assert!((fast - 47_737).abs() <= 4, "fast={fast}");
        assert!((slow - 48_217).abs() <= 4, "slow={slow}");
    }

    #[test]
    fn resampler_12k_amplitude_is_flat() {
        // P2 验收门：12 kHz 处幅度波动 < 0.1 dB（≈1.15%）。
        // 两点线性插值在这里会掉 3 dB，且随比率扫掠变成常年颤动。
        for adjust in [0.0f64, 0.001, -0.001, 0.005, -0.005] {
            let mut aw = AudioWriter::for_test(48000);
            aw.set_ratio_adjust(adjust);
            let mut total = 0usize;
            for block in 0..50 {
                let pcm = tone_12k(480, block * 480);
                total += aw.write(&pcm, 48000);
            }
            let mut ring = Vec::new();
            while let Some(v) = aw.ring.pop() {
                ring.push(v);
            }
            assert_eq!(ring.len(), total);
            // 跳过前 480 个样本（历史静音淡入段），其余按 RMS 折算幅度
            let body = &ring[480..];
            let amp = rms(body) * std::f64::consts::SQRT_2;
            let err_db = 20.0 * (amp / 1.0).abs().log10().abs().max(1e-12);
            assert!(
                err_db < 0.1,
                "adjust={adjust}: 12 kHz 幅度 {amp:.6}（{err_db:.3} dB）"
            );
        }
    }

    #[test]
    fn resampler_has_no_click_at_block_boundaries() {
        // 相位连续 + 多相插值 ⇒ 块与块之间不得出现台阶
        let mut aw = AudioWriter::for_test(48000);
        aw.set_ratio_adjust(0.003);
        for block in 0..20 {
            let pcm = tone_12k(480, block * 480);
            aw.write(&pcm, 48000);
        }
        let mut ring = Vec::new();
        while let Some(v) = aw.ring.pop() {
            ring.push(v);
        }
        // 12 kHz 在 48 kHz 下的最大相邻差 ≈ 2π×12k/48k ≈ 1.57
        for i in 200..ring.len() - 1 {
            let d = (ring[i + 1] - ring[i]).abs();
            assert!(d <= 1.75, "样本 {i} 处有台阶: Δ={d}");
        }
    }

    #[test]
    fn ratio_adjust_is_clamped() {
        let mut aw = AudioWriter::for_test(48000);
        aw.set_ratio_adjust(9.0);
        assert!((aw.current_ratio() - (1.0 + rtp::RESAMPLE_MAX_ADJUST)).abs() < 1e-12);
        aw.set_ratio_adjust(f64::NAN);
        assert_eq!(aw.adjust, 0.0);
    }

    #[test]
    fn device_latency_never_exceeds_one_second() {
        let aw = AudioWriter::for_test(48000);
        aw.device_latency_us.store(10_000, Ordering::Release); // 10 ms
        assert_eq!(aw.device_latency_frames(), 480);
        assert_eq!(aw.device_latency_ms(), 10);
        aw.device_latency_us.store(u64::MAX, Ordering::Release);
        assert_eq!(
            aw.device_latency_frames(),
            48000,
            "异常时间戳必须被钳到 1 s"
        );
    }

    #[test]
    fn flush_clears_ring_and_resampler() {
        let mut aw = AudioWriter::for_test(48000);
        aw.write(&vec![0.1f32; 480], 48000);
        aw.flush();
        assert_eq!(aw.ring.len(), 0);
        assert_eq!(aw.pos, 0.0);
        assert!(aw.hist.iter().all(|&v| v == 0.0));
    }
}
