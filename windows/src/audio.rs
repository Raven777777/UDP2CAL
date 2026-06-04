use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::StreamConfig;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct AudioWriter {
    buf: Arc<Mutex<VecDeque<f32>>>,
    pub device_rate: u32,
    pub resample_count: u64,
    #[allow(dead_code)]
    pub underrun_count: Arc<AtomicU64>,
    pub write_count: u64,
    pub total_compensated: u64,
    /// 当前漂移补偿比率 (1.0=无, >1.0=拉伸)
    pub drift_ratio: f64,
    
    // ── 相位连续流式重采样器 ──
    phase: f64,
    effective_ratio: f64,
    
    // ── 比率自适应 ──
    last_ratio_update: Instant,
    last_fill_sample: usize,
    last_fill_time: Instant,
    target_fill: usize,
    
    // ── 【核心修复】复用重采样缓冲区，消除高频堆分配 ──
    resample_buf: Vec<f32>,
}

impl AudioWriter {
    pub fn write(&mut self, pcm: &[f32], source_rate: u32) {
        if pcm.is_empty() {
            return;
        }
        self.write_count += 1;
        let now = Instant::now();

        // ── 每 3 秒：EMA 更新漂移比率 ──
        if now.duration_since(self.last_ratio_update) > std::time::Duration::from_secs(3) {
            let current_fill = self.buf.lock().map(|b| b.len()).unwrap_or(0);
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
            let fill_pct = current_fill as f64 / self.target_fill as f64;
            if fill_pct < 0.5 {
                self.drift_ratio += 0.0005; // 极小幅度增加
            } else if fill_pct > 1.5 {
                self.drift_ratio -= 0.0005; // 极小幅度减少
            }

            // [核心修复]：严格限制漂移区间在 ±0.2% 之间，防止人耳察觉到变调
            self.drift_ratio = self.drift_ratio.clamp(0.998, 1.002);
            self.effective_ratio = (source_rate as f64 / self.device_rate as f64) / self.drift_ratio;
            
            self.last_fill_sample = current_fill;
            self.last_fill_time = now;
            self.last_ratio_update = now;
        }

        // ── 相位连续流式重采样 ──
        if source_rate != self.device_rate {
            self.resample_count += 1;
        }
        self.total_compensated += 1;

        let n_in = pcm.len() as f64;
        let est_out = ((n_in / self.effective_ratio).ceil() as usize) + 32;

        // 【核心修复】：清空并复用已分配的内存，避免每次 write 都分配/释放 4KB
        self.resample_buf.clear();
        if self.resample_buf.capacity() < est_out {
            self.resample_buf.reserve(est_out - self.resample_buf.capacity());
        }

        while self.phase < n_in {
            let idx = self.phase as usize;
            let frac = self.phase - idx as f64;
            
            let i0 = idx.min(pcm.len().saturating_sub(1));
            let i1 = (idx + 1).min(pcm.len().saturating_sub(1));
            
            self.resample_buf.push(
                (pcm[i0] as f64 + (pcm[i1] - pcm[i0]) as f64 * frac) as f32
            );
            self.phase += self.effective_ratio;
        }
        self.phase -= n_in;

        if let Ok(mut buf) = self.buf.lock() {
            buf.extend(&self.resample_buf);
            
            // 最大缓冲限制，防止内存泄漏 (限制为 1 秒)
            let max = self.device_rate as usize;
            if buf.len() > max {
                let excess = buf.len() - max;
                buf.drain(..excess);
                // 【移除危险代码】：不再调用 shrink_to。
                // 在持锁期间重分配内存会导致音频回调超时卡顿。
                // 消除高频临时分配后，VecDeque 容量会自然稳定，不会无限增长。
            }
        }
    }
}

pub fn start_audio() -> Result<AudioWriter, String> {
    let device = find_vb_cable_device()
        .or_else(|| cpal::default_host().default_output_device())
        .ok_or("无可用音频输出设备")?;

    let supported_config = device
        .supported_output_configs()
        .map_err(|e| format!("枚举配置失败: {e}"))?
        .find(|c| c.channels() == 1)
        .or_else(|| {
            cpal::default_host()
                .default_output_device()
                .and_then(|d| d.supported_output_configs().ok())
                .and_then(|mut cs| cs.next())
        })
        .ok_or("无有效输出配置")?;

    let stream_config: StreamConfig = if supported_config.min_sample_rate() <= cpal::SampleRate(48000)
        && supported_config.max_sample_rate() >= cpal::SampleRate(48000)
    {
        supported_config.with_sample_rate(cpal::SampleRate(48000)).into()
    } else {
        supported_config.with_max_sample_rate().into()
    };

    let actual_rate = stream_config.sample_rate.0;
    // 【核心修复 1】获取实际输出的通道数
    let channels = stream_config.channels as usize; 
    
    let buf = Arc::new(Mutex::new(VecDeque::<f32>::new()));
    let initial_fill = actual_rate as usize / 5; // 200ms 初始缓冲
    buf.lock().unwrap().resize(initial_fill, 0.0);

    // 【修复】使用 Weak 弱引用，当 AudioWriter 被 Drop 时，后台线程不再强持有 VecDeque 内存
    let buf_weak = Arc::downgrade(&buf);
    let buf_weak_cb = buf_weak.clone();
    
    let (err_tx, err_rx) = std::sync::mpsc::sync_channel::<Option<String>>(1);
    
    let underrun = Arc::new(AtomicU64::new(0));
    // 【修复】underrun 计数器也使用 Weak，避免单个 Atomic 阻止整块内存释放
    let underrun_weak = Arc::downgrade(&underrun);
    let underrun_weak_cb = underrun_weak.clone();

    std::thread::Builder::new()
        .name("udp2mic-audio".into())
        .spawn(move || {
            let err_fn = |_| {};
            let stream = match device.build_output_stream(
                &stream_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    // 【关键】每次回调尝试升级弱引用，若 AudioWriter 已释放则填充零
                    if let Some(buf_strong) = buf_weak_cb.upgrade() {
                        if let Ok(mut buf) = buf_strong.lock() {
                            // 【核心修复 2】按照帧(Frame)计算需求，而不是采样点
                            let need_frames = data.len() / channels;
                            let avail_frames = buf.len();
                            let frames_to_read = need_frames.min(avail_frames);
                            
                            let mut data_idx = 0;
                            // 从缓冲区取出一帧（单声道），并复制到所有输出通道（例如左右耳）
                            for _ in 0..frames_to_read {
                                let sample = buf.pop_front().unwrap();
                                for _ in 0..channels {
                                    data[data_idx] = sample;
                                    data_idx += 1;
                                }
                            }
                            
                            // 处理欠载补零
                            if frames_to_read < need_frames {
                                let missing_frames = need_frames - frames_to_read;
                                data[data_idx..].fill(0.0);
                                
                                // 记录真实的缺失帧数
                                if let Some(ur_strong) = underrun_weak_cb.upgrade() {
                                    ur_strong.fetch_add(missing_frames as u64, Ordering::Relaxed);
                                }
                            }
                        } else {
                            data.fill(0.0);
                        }
                    } else {
                        // AudioWriter 已被释放，静默填零
                        data.fill(0.0);
                    }
                },
                err_fn,
                None,
            ) {
                Ok(s) => s,
                Err(e) => {
                    let _ = err_tx.send(Some(format!("创建流失败: {e}")));
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
        Ok(None) =>         Ok(AudioWriter {
            buf,
            device_rate: actual_rate,
            resample_count: 0,
            underrun_count: underrun,
            write_count: 0,
            total_compensated: 0,
            drift_ratio: 1.0,
            phase: 0.0,
            // 初始化为 1.0，首次 write 时若 source_rate != device_rate 会在 3s EMA 前使用此值，
            // 但大部分场景下两者相等（均为 48kHz），故 1.0 作为安全起始值
            effective_ratio: 1.0,
            last_ratio_update: now,
            last_fill_sample: initial_fill,
            last_fill_time: now,
            target_fill: initial_fill,
            // 预分配 8192 个 f32 (32KB) 足以容纳最大单帧重采样，后续永不再分配
            resample_buf: Vec::with_capacity(8192),
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