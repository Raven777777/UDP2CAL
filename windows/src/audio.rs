use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

// ═══════════════════════════════════════════════════════════════════
// 无锁 SPSC 环形缓冲
// 音频回调线程绝不能等待互斥锁（优先级反转 → 欠载爆音）。
// 单生产者（网络线程）写 head，单消费者（音频回调）写 tail，
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
}

pub struct AudioWriter {
    ring: Arc<Ring>,
    pub device_rate: u32,
    /// 诊断计数器：由 UI 线程读取并展示/落盘（见 main.rs 的 AUDIO_STATS）
    pub stats: Arc<AudioStats>,
    /// 当前漂移补偿比率 (1.0=无, >1.0=拉伸)
    drift_ratio: f64,

    // ── 相位连续流式重采样器 ──
    phase: f64,
    effective_ratio: f64,
    /// 当前输入采样率（0 = 尚未收到数据）
    source_rate: u32,

    // ── 比率自适应 ──
    last_ratio_update: Instant,
    last_fill_sample: usize,
    last_fill_time: Instant,
    target_fill: usize,

    // ── 复用重采样缓冲区，消除高频堆分配 ──
    resample_buf: Vec<f32>,
}

// ═══════════════════════════════════════════════════════════════════
// 诊断计数器
// 历史实现把 6 个计数器散在 AudioWriter 上，全部只写不读——诊断信息
// 采了却没人看。这里收敛成一个可共享的单例，UI 与日志共同消费。
// ═══════════════════════════════════════════════════════════════════

#[derive(Default)]
pub struct AudioStats {
    /// 收到并处理的音频块数
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
    pub fn write(&mut self, pcm: &[f32], source_rate: u32) {
        if pcm.is_empty() {
            return;
        }
        self.stats.writes.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();

        // ── 采样率变化（含首次）立即生效 ──
        // 历史缺陷：effective_ratio 只在 3 秒 EMA 分支里更新，初值 1.0，
        // 源/设备采样率不一致时开头 3 秒会以错误速率播放（变调）。
        if source_rate > 0 && source_rate != self.source_rate {
            self.source_rate = source_rate;
            self.effective_ratio =
                (source_rate as f64 / self.device_rate as f64) / self.drift_ratio;
        }
        if !(0.1..10.0).contains(&self.effective_ratio) {
            // 防御：异常比率会让下面的相位循环失控
            self.effective_ratio = 1.0;
        }

        // ── 每 3 秒：EMA 更新漂移比率 ──
        if now.duration_since(self.last_ratio_update) > std::time::Duration::from_secs(3) {
            let current_fill = self.ring.len();
            let dt = now.duration_since(self.last_fill_time).as_secs_f64();

            if dt > 2.0 && self.last_fill_sample > 0 {
                let fill_delta = self.last_fill_sample as f64 - current_fill as f64;
                let consume_rate = fill_delta / dt; // + = buffer shrinking
                let produce_rate = self.device_rate as f64 - consume_rate;

                if produce_rate > 1000.0 && produce_rate.is_normal() {
                    let measured = self.device_rate as f64 / produce_rate;
                    if measured.is_finite() && measured > 0.5 && measured < 1.5 {
                        self.drift_ratio = self.drift_ratio * 0.7 + measured * 0.3; // EMA α=0.3
                    }
                }
            }

            // 缓冲区严重偏离时快速修正 (使用极小步长避免明显变调)
            let fill_pct = current_fill as f64 / self.target_fill.max(1) as f64;
            if fill_pct < 0.5 {
                self.drift_ratio += 0.0005;
            } else if fill_pct > 1.5 {
                self.drift_ratio -= 0.0005;
            }

            // 严格限制漂移区间在 ±0.2% 之间，防止人耳察觉到变调
            self.drift_ratio = self.drift_ratio.clamp(0.998, 1.002);
            self.effective_ratio =
                (self.source_rate.max(1) as f64 / self.device_rate as f64) / self.drift_ratio;

            self.last_fill_sample = current_fill;
            self.last_fill_time = now;
            self.last_ratio_update = now;
        }

        if source_rate != self.device_rate {
            self.stats.resampled.fetch_add(1, Ordering::Relaxed);
        }

        // ── 相位连续流式重采样 ──
        let n_in = pcm.len() as f64;
        self.resample_buf.clear();
        let last = pcm.len() - 1;
        let mut phase = self.phase;
        while phase < n_in {
            let idx = phase as usize;
            let frac = phase - idx as f64;
            let i0 = idx.min(last);
            let i1 = (idx + 1).min(last);
            self.resample_buf
                .push((pcm[i0] as f64 + (pcm[i1] - pcm[i0]) as f64 * frac) as f32);
            phase += self.effective_ratio;
        }
        self.phase = phase - n_in;

        // ── 无锁写入环形缓冲；满则丢弃（不再在锁内扩容/搬移）──
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
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let ring = ring.clone();
    let stats = stats.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
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

                if filled < need_frames {
                    data[idx..].fill(T::EQUILIBRIUM);
                    if let Some(st) = stats.upgrade() {
                        st.underrun
                            .fetch_add((need_frames - filled) as u64, Ordering::Relaxed);
                    }
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

    // 上限 500ms，预填充 40ms（低延迟）
    let ring = Arc::new(Ring::with_frames(actual_rate as usize / 2));
    let initial_fill = actual_rate as usize / 25;
    for _ in 0..initial_fill {
        ring.push(0.0);
    }

    let (err_tx, err_rx) = std::sync::mpsc::sync_channel::<Option<String>>(1);

    let ring_weak = Arc::downgrade(&ring);
    let stats_weak = Arc::downgrade(&stats);

    std::thread::Builder::new()
        .name("udp2cal-audio".into())
        .spawn(move || {
            let built = match sample_format {
                SampleFormat::F32 => build_stream::<f32>(&device, &stream_config, &ring_weak, &stats_weak),
                SampleFormat::I16 => build_stream::<i16>(&device, &stream_config, &ring_weak, &stats_weak),
                SampleFormat::U16 => build_stream::<u16>(&device, &stream_config, &ring_weak, &stats_weak),
                SampleFormat::I32 => build_stream::<i32>(&device, &stream_config, &ring_weak, &stats_weak),
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

    let now = Instant::now();
    match err_rx.recv() {
        Ok(Some(e)) => Err(e),
        Ok(None) => Ok(AudioWriter {
            ring,
            device_rate: actual_rate,
            stats,
            drift_ratio: 1.0,
            phase: 0.0,
            // 首帧 write() 会按实际 source_rate 立即校正，此处仅为占位
            effective_ratio: 1.0,
            source_rate: 0,
            last_ratio_update: now,
            last_fill_sample: initial_fill,
            last_fill_time: now,
            target_fill: initial_fill,
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
// 单元测试：环形缓冲与重采样比率
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

    impl AudioWriter {
        fn for_test(device_rate: u32) -> Self {
            let ring = Arc::new(Ring::with_frames(device_rate as usize));
            let now = Instant::now();
            Self {
                ring,
                device_rate,
                stats: Arc::new(AudioStats::default()),
                drift_ratio: 1.0,
                phase: 0.0,
                effective_ratio: 1.0,
                source_rate: 0,
                last_ratio_update: now,
                last_fill_sample: 0,
                last_fill_time: now,
                target_fill: device_rate as usize / 25,
                resample_buf: Vec::new(),
            }
        }
    }

    #[test]
    fn first_write_applies_source_rate_ratio() {
        // 回归：历史实现 effective_ratio 初值为 1.0 且只在 3 秒后才更新，
        // 导致 16k→48k 场景下开头 3 秒以 1:1 写入（三倍速）。
        let mut aw = AudioWriter::for_test(48000);
        let pcm = vec![0.1f32; 480];
        aw.write(&pcm, 16000);
        let produced = aw.ring.len();
        assert!(
            (produced as i64 - 1440).abs() <= 4,
            "16k→48k 应产出约 1440 个样本，实际 {produced}"
        );
    }

    #[test]
    fn same_rate_write_is_one_to_one() {
        let mut aw = AudioWriter::for_test(48000);
        let pcm = vec![0.1f32; 480];
        aw.write(&pcm, 48000);
        assert!((aw.ring.len() as i64 - 480).abs() <= 2, "len={}", aw.ring.len());
        assert_eq!(aw.stats.resampled.load(Ordering::Relaxed), 0);
    }
}
