// 无锁 PCM 环 + cpal 输出回调
//
// v3 的时钟模型只有一条规则：**声卡播放头是唯一音频时钟**。
// 因此本模块只做三件事：
//   1. 提供一个无锁 SPSC 环，解码线程写、音频回调读；
//   2. 在回调里统计"真正被消费的帧数"（`played_frames`），供播放头换算；
//   3. 把 48 kHz 媒体流按设备采样率做**固定比率**重采样（不做 EMA、不读墙钟）。
//
// 漂移校正不在这里：它由解码线程根据抖动缓冲水位决定，并通过
// `PlayheadClock::note_pcm_correction` 回写媒体时间偏移。
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::thread::Thread;

// ═══════════════════════════════════════════════════════════════════
// 无锁 SPSC 环形缓冲
// 音频回调线程绝不能等待互斥锁（优先级反转 → 欠载爆音）。
// 单生产者（解码线程）写 head，单消费者（音频回调）写 tail，
// 元素用 AtomicU32 承载 f32 位模式，全部走安全 API，无需 unsafe。
// ═══════════════════════════════════════════════════════════════════

struct Ring {
    data: Box<[AtomicU32]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
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

    #[inline]
    fn len(&self) -> usize {
        self.head.load(Ordering::Acquire).wrapping_sub(self.tail.load(Ordering::Acquire))
    }

    /// 仅由生产者在"停止/重启"时调用：丢弃全部未播放数据
    #[inline]
    fn clear(&self) {
        let head = self.head.load(Ordering::Acquire);
        self.tail.store(head, Ordering::Release);
    }
}

pub struct AudioWriter {
    ring: Arc<Ring>,
    pub device_rate: u32,
    /// 诊断计数器：由 UI 线程读取并展示/落盘（见 main.rs 的 AUDIO_STATS）
    pub stats: Arc<AudioStats>,
    /// 声卡已真正消费的帧数（只统计真实 PCM，不统计回调补的静音）
    played: Arc<AtomicU64>,
    /// 解码线程句柄：回调每次运行后 unpark 它（原子置位，无分配）
    waker: Arc<OnceLock<Thread>>,

    // ── 相位连续流式重采样器（固定比率，不做漂移 EMA）──
    phase: f64,
    ratio: f64,
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

    /// 声卡已消费的真实帧数（单调递增，32 位回绕由调用方按 u64 处理）
    #[inline]
    pub fn played_frames(&self) -> u64 {
        self.played.load(Ordering::Acquire)
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
        self.ring.clear();
        self.phase = 0.0;
    }

    /// 写入 48 kHz 媒体 PCM（单声道），按设备采样率换算后入环。
    ///
    /// 比率固定为 `source_rate / device_rate`，**不含任何漂移补偿**：
    /// 采集/声卡时钟差由抖动缓冲水位 + `PlayheadClock` 偏移统一处理。
    pub fn write(&mut self, pcm: &[f32], source_rate: u32) {
        if pcm.is_empty() {
            return;
        }
        self.stats.writes.fetch_add(1, Ordering::Relaxed);

        if source_rate > 0 && source_rate != self.source_rate {
            self.source_rate = source_rate;
            let r = source_rate as f64 / self.device_rate as f64;
            self.ratio = if (0.1..10.0).contains(&r) { r } else { 1.0 };
            self.phase = 0.0;
        }
        let ratio = self.ratio.max(1e-6);

        if source_rate != self.device_rate {
            self.stats.resampled.fetch_add(1, Ordering::Relaxed);
        }

        if (ratio - 1.0).abs() < 1e-9 {
            for &s in pcm {
                if !self.ring.push(s) {
                    self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            return;
        }

        // ── 相位连续线性插值 ──
        let n_in = pcm.len() as f64;
        self.resample_buf.clear();
        let last = pcm.len() - 1;
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

        for &s in &self.resample_buf {
            if !self.ring.push(s) {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
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
    waker: &Arc<OnceLock<Thread>>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let ring = ring.clone();
    let stats = stats.clone();
    let played = played.clone();
    let waker = waker.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                // 回调约束：无锁、无分配、不调用 Opus、不写日志、不做复杂计算
                let need_frames = data.len() / channels;
                let mut filled = 0usize;
                let mut idx = 0usize;

                if let Some(ring) = ring.upgrade() {
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

                if let Some(p) = played.upgrade() {
                    if filled > 0 {
                        p.fetch_add(filled as u64, Ordering::Release);
                    }
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

    let stream_config: StreamConfig = if supported_config.min_sample_rate() <= cpal::SampleRate(48000)
        && supported_config.max_sample_rate() >= cpal::SampleRate(48000)
    {
        supported_config.with_sample_rate(cpal::SampleRate(48000)).into()
    } else {
        supported_config.with_max_sample_rate().into()
    };

    let actual_rate = stream_config.sample_rate.0;

    // 容量 341 ms（48 kHz 下 16384 帧），远大于 200 ms 下限；
    // **不预填静音**：预缓冲由抖动缓冲负责，回调在无数据时输出静音且不推进播放头。
    let ring = Arc::new(Ring::with_frames((actual_rate as usize * 341) / 1000));
    let played = Arc::new(AtomicU64::new(0));
    let waker: Arc<OnceLock<Thread>> = Arc::new(OnceLock::new());

    let (err_tx, err_rx) = std::sync::mpsc::sync_channel::<Option<String>>(1);

    let ring_weak = Arc::downgrade(&ring);
    let stats_weak = Arc::downgrade(&stats);
    let played_weak = Arc::downgrade(&played);
    let waker_thread = waker.clone();

    std::thread::Builder::new()
        .name("udp2cal-audio".into())
        .spawn(move || {
            let built = match sample_format {
                SampleFormat::F32 => build_stream::<f32>(&device, &stream_config, &ring_weak, &stats_weak, &played_weak, &waker_thread),
                SampleFormat::I16 => build_stream::<i16>(&device, &stream_config, &ring_weak, &stats_weak, &played_weak, &waker_thread),
                SampleFormat::U16 => build_stream::<u16>(&device, &stream_config, &ring_weak, &stats_weak, &played_weak, &waker_thread),
                SampleFormat::I32 => build_stream::<i32>(&device, &stream_config, &ring_weak, &stats_weak, &played_weak, &waker_thread),
                other => Err(format!("不支持的输出采样格式: {other}")),
            };
            let stream = match built {
                Ok(s) => s,
                Err(e) => {
                    let _ = err_tx.send(Some(e));
                    return;
                }
            };
            if let Err(e) = stream.play() {
                let _ = err_tx.send(Some(format!("播放失败: {e}")));
                return;
            }
            let _ = err_tx.send(None);
            loop {
                std::thread::park();
            }
        })
        .map_err(|e| format!("线程创建失败: {e}"))?;

    match err_rx.recv() {
        Ok(Some(e)) => Err(e),
        Ok(None) => Ok(AudioWriter {
            ring,
            device_rate: actual_rate,
            stats,
            played,
            waker,
            phase: 0.0,
            ratio: 1.0,
            source_rate: 0,
            resample_buf: Vec::with_capacity(4096),
        }),
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
    fn ring_clear_drops_pending() {
        let ring = Ring::with_frames(1024);
        for _ in 0..100 {
            ring.push(0.5);
        }
        ring.clear();
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.pop(), None);
        assert!(ring.push(0.25));
    }

    impl AudioWriter {
        fn for_test(device_rate: u32) -> Self {
            Self {
                ring: Arc::new(Ring::with_frames(device_rate as usize)),
                device_rate,
                stats: Arc::new(AudioStats::default()),
                played: Arc::new(AtomicU64::new(0)),
                waker: Arc::new(OnceLock::new()),
                phase: 0.0,
                ratio: 1.0,
                source_rate: 0,
                resample_buf: Vec::new(),
            }
        }
    }

    #[test]
    fn same_rate_write_is_one_to_one() {
        let mut aw = AudioWriter::for_test(48000);
        let pcm = vec![0.1f32; 480];
        aw.write(&pcm, 48000);
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
    fn flush_clears_ring_and_phase() {
        let mut aw = AudioWriter::for_test(48000);
        aw.write(&vec![0.1f32; 480], 48000);
        aw.flush();
        assert_eq!(aw.ring.len(), 0);
        assert_eq!(aw.space_frames(), aw.ring.capacity());
    }
}
