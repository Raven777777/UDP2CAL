// 无锁 PCM 环 + cpal 输出回调
//
// v3 的时钟模型只有一条规则：**声卡播放头是唯一音频时钟**。
// 因此本模块只做四件事：
//   1. 提供一个无锁 SPSC 环，解码线程写、音频回调读；
//   2. 在回调里统计"真正交给声卡播放的帧数"（`played_frames`），供播放头换算；
//   3. 把 48 kHz 媒体流按设备采样率做相位连续重采样，比率可被解码线程
//      在 ±0.5% 内微调（漂移校正的**主力手段**，见 `set_ratio_adjust`）；
//   4. 上报声卡自报的"回调 → 实际发声"延迟（`device_latency_us`），
//      用于回答"playhead_ts 到底含不含 DMA 缓冲"。
//
// 离散的丢/插不在这里决策：由解码线程根据抖动缓冲水位决定，
// 并通过 `PlayheadClock::note_pcm_correction` 回写媒体时间偏移。
use crate::protocol::rtp;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, SampleFormat, SizedSample, StreamConfig};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::thread::Thread;

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
        let data = (0..cap).map(|_| AtomicU32::new(0)).collect::<Vec<_>>().into_boxed_slice();
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
            self.tail.store(target.min(head), Ordering::Release);
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
    /// 解码线程句柄：回调每次运行后 unpark 它（原子置位，无分配）
    waker: Arc<OnceLock<Thread>>,

    // ── 相位连续流式重采样器 ──
    phase: f64,
    /// 固定比率 = 源采样率 / 设备采样率（不含微调）
    base_ratio: f64,
    /// 比率微调量（±0.005），由漂移控制器每 10 ms 写入一次
    adjust: f64,
    /// 当前输入采样率（0 = 尚未收到数据）
    source_rate: u32,

    // ── 复用重采样缓冲区，消除高频堆分配 ──
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

    /// 同上，毫秒（诊断展示用）
    #[inline]
    pub fn device_latency_ms(&self) -> u32 {
        ((self.device_latency_frames() * 1000) / self.device_rate.max(1) as u64) as u32
    }

    /// 环里还能放下多少帧（设备帧）
    #[inline]
    #[allow(dead_code)]
    pub fn space_frames(&self) -> usize {
        self.ring.capacity().saturating_sub(self.ring.len())
    }

    /// 环里已排队、尚未被声卡消费的帧数（设备帧）。
    /// 解码线程用它把"预解码深度"钉在几十毫秒内——这正是把 PLC
    /// 与播放头绑定的前提：生产速度必须被消费速度约束。
    #[inline]
    pub fn filled_frames(&self) -> usize {
        self.ring.len()
    }

    /// 丢弃未播放数据（重连 / 重同步 / 停止）
    pub fn flush(&mut self) {
        // tail 归音频回调所有：这里只能投递请求，由回调落地
        self.ring.request_flush();
        self.phase = 0.0;
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
    fn current_ratio(&self) -> f64 {
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
            self.phase = 0.0;
        }
        let ratio = self.current_ratio();

        if source_rate != self.device_rate || self.adjust.abs() > 1e-12 {
            self.stats.resampled.fetch_add(1, Ordering::Relaxed);
        }

        if (ratio - 1.0).abs() < 1e-9 {
            let mut written = 0usize;
            for &s in pcm {
                if self.ring.push(s) {
                    written += 1;
                } else {
                    self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            return written;
        }

        // ── 相位连续线性插值 ──
        //
        // 相位恒在 [0, ratio) 内（循环出口条件保证 `phase >= n_in`，故
        // `phase - n_in >= 0`），因此不存在"负相位外插"问题。
        // 块尾若落在 [n_in-1, n_in)，`i1` 会钳到 `last`，等效于把末样本多保持
        // 一个输出点——这是 ≤1 个样本的保持误差，不可闻，且不影响比率精度。
        let n_in = pcm.len() as f64;
        let last = pcm.len() - 1;
        self.resample_buf.clear();
        let mut phase = self.phase;
        while phase < n_in {
            let idx = phase as usize;
            let frac = phase - idx as f64;
            let i0 = idx.min(last);
            let i1 = (idx + 1).min(last);
            self.resample_buf.push((pcm[i0] as f64 + (pcm[i1] - pcm[i0]) as f64 * frac) as f32);
            phase += ratio;
        }
        self.phase = phase - n_in;

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
// 有界回落的 PCM 编辑（P0 安全网）
//
// 两条铁律：
//   1. 交叉淡化长度 ≥ 1 ms（48 样本），丢和插都只能在淡化区内完成；
//   2. 接缝处必须一阶连续 —— 权重用升余弦，两端导数为零，不会留下折角。
//
// 几何（以 n=480、k=48 为例）：
//   丢：把 x[p..p+k] 与 x[p+k..p+2k] 在 k 个样本内交叉，输出 n-k 个样本；
//   插：把 x[p+k..] 整体后移 k，并在 k 个样本内从 x[p..] 交叉到 x[p-k..]，
//       输出 n+k 个样本（信号在此"停顿" 1 ms 再继续）。
// 两种编辑都从上一帧末样本平滑接上、并平滑交接下一帧首样本。
// ═══════════════════════════════════════════════════════════════════

/// 升余弦淡入权重：`i` 从 0 到 k-1 → 权重从 ≈0 到 ≈1
#[inline]
fn fade_weight(i: usize, k: usize) -> f32 {
    0.5 - 0.5 * (std::f32::consts::PI * (i as f32 + 1.0) / (k as f32 + 1.0)).cos()
}

/// 丢弃 `k` 个样本（每声道），在帧尾 `k` 个样本内交叉淡化。
///
/// 就地压缩 `pcm`，返回新长度 `n - k`。样本数不足 `3k` 时不动，
/// 原样返回 `n`（调用方据此判断这次回落是否真的生效）。
pub fn slip_drop(pcm: &mut [f32], k: usize) -> usize {
    let n = pcm.len();
    if k == 0 || n < 3 * k {
        return n;
    }
    let p = n - 2 * k;
    for i in 0..k {
        let w = fade_weight(i, k);
        pcm[p + i] = pcm[p + i] * (1.0 - w) + pcm[p + i + k] * w;
    }
    pcm.copy_within(p + 2 * k..n, p + k);
    n - k
}

/// 插入 `k` 个样本（每声道）：把 `pcm[..n]` 重写进 `out`，在帧中间
/// 做 `k` 个样本的交叉淡化，返回新长度 `n + k`。
///
/// `out` 至少需要 `n + k` 个位置；条件不满足时退化为原样拷贝并返回 `n`。
pub fn slip_insert(pcm: &[f32], out: &mut [f32], k: usize) -> usize {
    let n = pcm.len();
    if k == 0 || n < 3 * k || out.len() < n + k {
        let take = n.min(out.len());
        out[..take].copy_from_slice(&pcm[..take]);
        return take;
    }
    let p = n / 2;
    out[..p].copy_from_slice(&pcm[..p]);
    for i in 0..k {
        let w = fade_weight(i, k);
        out[p + i] = pcm[p + i] * (1.0 - w) + pcm[p + i - k] * w;
    }
    out[p + k..n + k].copy_from_slice(&pcm[p..n]);
    n + k
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
        .min_by_key(|c| (rank(c.sample_format()), if c.channels() == 1 { 0 } else { 1 }, c.channels()))
        .ok_or_else(|| "无有效输出配置".to_string())
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    ring: &Weak<Ring>,
    stats: &Weak<AudioStats>,
    played: &Weak<AtomicU64>,
    latency: &Arc<AtomicU64>,
    waker: &Arc<OnceLock<Thread>>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let ring = ring.clone();
    let stats = stats.clone();
    let played = played.clone();
    let latency = latency.clone();
    let waker = waker.clone();
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
                    p.fetch_add(need_frames as u64, Ordering::Release);
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

    let mut stream_config: StreamConfig = if supported_config.min_sample_rate() <= cpal::SampleRate(48000)
        && supported_config.max_sample_rate() >= cpal::SampleRate(48000)
    {
        supported_config.with_sample_rate(cpal::SampleRate(48000)).into()
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
    let waker: Arc<OnceLock<Thread>> = Arc::new(OnceLock::new());

    let (err_tx, err_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

    let ring_weak = Arc::downgrade(&ring);
    let stats_weak = Arc::downgrade(&stats);
    let played_weak = Arc::downgrade(&played);
    let waker_thread = waker.clone();
    let latency_shared = device_latency_us.clone();

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
                    SampleFormat::F32 => build_stream::<f32>(&device, &cfg, &ring_weak, &stats_weak, &played_weak, &latency_shared, &waker_thread),
                    SampleFormat::I16 => build_stream::<i16>(&device, &cfg, &ring_weak, &stats_weak, &played_weak, &latency_shared, &waker_thread),
                    SampleFormat::U16 => build_stream::<u16>(&device, &cfg, &ring_weak, &stats_weak, &played_weak, &latency_shared, &waker_thread),
                    SampleFormat::I32 => build_stream::<i32>(&device, &cfg, &ring_weak, &stats_weak, &played_weak, &latency_shared, &waker_thread),
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
            waker,
            phase: 0.0,
            base_ratio: 1.0,
            adjust: 0.0,
            source_rate: 0,
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
                waker: Arc::new(OnceLock::new()),
                phase: 0.0,
                base_ratio: 1.0,
                adjust: 0.0,
                source_rate: 0,
                resample_buf: Vec::new(),
            }
        }
    }

    #[test]
    fn same_rate_write_is_one_to_one() {
        let mut aw = AudioWriter::for_test(48000);
        let pcm = vec![0.1f32; 480];
        assert_eq!(aw.write(&pcm, 48000), 480);
        assert_eq!(aw.ring.len(), 480);
        assert_eq!(aw.stats.resampled.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn forty_four_one_device_upsamples_48k_input() {
        // 48 kHz 源 → 44.1 kHz 设备：480 样本应产出约 441 帧
        let mut aw = AudioWriter::for_test(44100);
        let pcm = vec![0.1f32; 480];
        aw.write(&pcm, 48000);
        let produced = aw.ring.len();
        assert!((produced as i64 - 441).abs() <= 2, "produced={produced}");
        assert_eq!(aw.stats.resampled.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn resampler_keeps_phase_across_blocks() {
        let mut aw = AudioWriter::for_test(44100);
        let pcm = vec![0.1f32; 480];
        for _ in 0..10 {
            aw.write(&pcm, 48000);
        }
        let produced = aw.ring.len();
        assert!((produced as i64 - 4_410).abs() <= 4, "produced={produced}");
    }

    #[test]
    fn ratio_adjust_changes_produced_frames() {
        // adjust > 0 → 比率变大 → 每秒**少**产出约 240 帧（水位回落）
        for (adjust, expect) in [(0.005f64, 47761i64), (-0.005, 48241)] {
            let mut aw = AudioWriter::for_test(48000);
            aw.set_ratio_adjust(adjust);
            let pcm = vec![0.1f32; 480];
            let mut total = 0i64;
            for _ in 0..100 {
                total += aw.write(&pcm, 48000) as i64;
            }
            assert!(
                (total - expect).abs() <= 3,
                "adjust={adjust} 1 秒内产出 {total}，期望约 {expect}"
            );
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
        assert_eq!(aw.device_latency_frames(), 48000, "异常时间戳必须被钳到 1 s");
    }

    // ── 有界回落的交叉淡化 ──

    /// 用一条 480 样本的正弦测"接缝处是否有台阶"：
    /// 回落前后都必须保持一阶连续（相邻样本差不超过正弦自身的最大斜率 + 容差）。
    fn assert_smooth(sig: &[f32], from: usize, to: usize, max_step: f32) {
        for i in from..to {
            let d = (sig[i + 1] - sig[i]).abs();
            assert!(d <= max_step, "样本 {i} 处有台阶: Δ={d} > {max_step}");
        }
    }

    #[test]
    fn slip_drop_is_crossfaded_not_cut() {
        // 1 kHz 正弦，480 样本/帧
        let n = 480usize;
        let k = rtp::SLIP_FADE_SAMPLES;
        let mut pcm: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin() * 0.5)
            .collect();
        let tail = pcm[n - 1];
        let new_len = slip_drop(&mut pcm, k);
        assert_eq!(new_len, n - k);
        // 1 kHz 在 48 kHz 下的最大相邻差 ≈ 0.5 × 2π×1000/48000 ≈ 0.0655
        assert_smooth(&pcm, 0, new_len - 1, 0.09);
        // 末样本必须等于原帧末样本：不丢音频尾巴，也不与下一帧错位
        assert!((pcm[new_len - 1] - tail).abs() < 1e-4);
    }

    #[test]
    fn slip_drop_removes_exactly_k_samples() {
        let mut pcm = vec![0.0f32; 480];
        for (i, s) in pcm.iter_mut().enumerate() {
            *s = i as f32;
        }
        let k = 48usize;
        let n = slip_drop(&mut pcm, k);
        assert_eq!(n, 432);
        // 末样本保留（淡入权重约 0.999，误差 < 0.05 个 LSB 的量化级）
        assert!((pcm[431] - 479.0).abs() < 0.1, "末样本 {}", pcm[431]);
        // 前段原样
        assert_eq!(pcm[0], 0.0);
        assert_eq!(pcm[383], 383.0);
    }

    #[test]
    fn slip_insert_is_crossfaded_not_cut() {
        let n = 480usize;
        let k = rtp::SLIP_FADE_SAMPLES;
        let pcm: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin() * 0.5)
            .collect();
        let mut out = vec![0.0f32; n + k];
        let new_len = slip_insert(&pcm, &mut out, k);
        assert_eq!(new_len, n + k);
        assert_smooth(&out, 0, new_len - 1, 0.09);
        assert_eq!(out[new_len - 1], pcm[n - 1], "末样本必须与原帧一致");
        assert_eq!(out[0], pcm[0]);
    }

    #[test]
    fn slip_ignores_frames_too_short_for_fade() {
        // 帧长 < 3k 时不得动手（宁可少一次校正，也不能把帧切坏）
        let mut tiny = vec![1.0f32; 100];
        assert_eq!(slip_drop(&mut tiny, rtp::SLIP_FADE_SAMPLES), 100);
        let out_len = {
            let mut out = vec![0.0f32; 100 + rtp::SLIP_FADE_SAMPLES];
            slip_insert(&tiny, &mut out, rtp::SLIP_FADE_SAMPLES)
        };
        assert_eq!(out_len, 100);
    }

    #[test]
    fn slip_fade_weights_are_monotone() {
        let k = rtp::SLIP_FADE_SAMPLES;
        let mut prev = fade_weight(0, k);
        assert!(prev < 0.02, "淡入起点必须接近 0");
        for i in 1..k {
            let w = fade_weight(i, k);
            assert!(w > prev, "权重必须单调递增");
            prev = w;
        }
        assert!(prev > 0.98, "淡入终点必须接近 1");
    }

    #[test]
    fn flush_clears_ring_and_phase() {
        let mut aw = AudioWriter::for_test(48000);
        aw.write(&vec![0.1f32; 480], 48000);
        aw.flush();
        assert_eq!(aw.ring.len(), 0);
        assert_eq!(aw.space_frames(), aw.ring.capacity());
    }
}
