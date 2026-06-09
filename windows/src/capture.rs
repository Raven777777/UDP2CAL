// WASAPI 环回捕获 + Opus 编码 + 反向 UDP 发送
// 捕获系统音频输出（VB-Cable/扬声器）→ Opus 编码 → 发送到 Android 手机
// 支持运行时从 CONNECT 保活包同步手机端 Opus 设置

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use windows::Win32::Media::Audio::{
    eConsole, eRender, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CLSCTX_ALL, COINIT_MULTITHREADED,
};

const FRAME_SIZE: usize = 480;      // 48000 * 10ms 帧（单声道，低延迟优化）
const FRAME_SIZE_16K: usize = 160;  // 16000 * 10ms 帧（单声道，低性能）
const CODEC_MONO: u8 = 1;      // 协议 codec=1: Opus单声道（低性能模式）
const CODEC_STEREO: u8 = 2;    // 协议 codec=2: Opus立体声（高品质模式）

// ═══════════════════════════════════════════════════════════════════
// Opus 反向编码运行时配置（由主线程 CONNECT 保活包更新，运行中热生效）
// ═══════════════════════════════════════════════════════════════════

/// Opus 编码器运行时参数（与 Android 端编码设置一一对应）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OpusReverseConfig {
    pub bitrate_kbps: u16,     // 0=max/auto
    pub bandwidth: u8,         // 0=NB, 1=MB, 2=WB, 3=SWB, 4=FB
    pub complexity: u8,        // 1-10
    pub signal: u8,            // 0=auto, 1=voice, 2=music
    pub vbr: bool,
    pub dtx: bool,
    pub fec: bool,
    pub vbr_constraint: bool,
    pub packet_loss: u8,       // 0-100
}

impl Default for OpusReverseConfig {
    fn default() -> Self {
        Self {
            bitrate_kbps: 0,     // auto/max
            bandwidth: 4,        // FB
            complexity: 3,
            signal: 0,           // auto
            vbr: true,
            dtx: false,
            fec: false,
            vbr_constraint: false,
            packet_loss: 0,
        }
    }
}

static REVERSE_CFG: OnceLock<Mutex<OpusReverseConfig>> = OnceLock::new();

fn reverse_config() -> &'static Mutex<OpusReverseConfig> {
    REVERSE_CFG.get_or_init(|| Mutex::new(OpusReverseConfig::default()))
}

/// 由 main.rs 的 handle_control 调用，从 CONNECT 包更新反向编码参数
pub fn update_reverse_config(cfg: OpusReverseConfig) {
    if let Ok(mut c) = reverse_config().lock() {
        if *c != cfg {
            eprintln!("[Reverse] 更新 Opus 配置: br={} bw={} cplx={} sig={} vbr={} dtx={} fec={} pl={}",
                cfg.bitrate_kbps, cfg.bandwidth, cfg.complexity, cfg.signal,
                cfg.vbr, cfg.dtx, cfg.fec, cfg.packet_loss);
            *c = cfg;
        }
    }
}

/// 将 Opus 配置应用到编码器（在 run_loop 线程中调用）
fn apply_config(encoder: &mut audiopus::coder::Encoder, cfg: &OpusReverseConfig) {
    // 码率：0=max, 其他按 kbps 设置
    let bitrate = if cfg.bitrate_kbps == 0 {
        audiopus::Bitrate::Max
    } else {
        audiopus::Bitrate::BitsPerSecond(cfg.bitrate_kbps as i32 * 1000)
    };
    let _ = encoder.set_bitrate(bitrate);

    // 带宽
    let bw = match cfg.bandwidth {
        0 => audiopus::Bandwidth::Narrowband,
        1 => audiopus::Bandwidth::Mediumband,
        2 => audiopus::Bandwidth::Wideband,
        3 => audiopus::Bandwidth::Superwideband,
        _ => audiopus::Bandwidth::Fullband,
    };
    let _ = encoder.set_bandwidth(bw);

    // 复杂度
    let _ = encoder.set_complexity(cfg.complexity);

    // VBR
    let _ = encoder.set_vbr(cfg.vbr);

    // DTX
    let _ = encoder.set_dtx(cfg.dtx);

    // FEC
    let _ = encoder.set_inband_fec(cfg.fec);

    // 丢包率
    let _ = encoder.set_packet_loss_perc(cfg.packet_loss);

    // VBR 约束
    let _ = encoder.set_vbr_constraint(cfg.vbr_constraint);

    // 信号类型（影响 SILK/CELT 模式选择）
    match cfg.signal {
        1 => { let _ = encoder.set_signal(audiopus::Signal::Voice); }
        2 => { let _ = encoder.set_signal(audiopus::Signal::Music); }
        _ => { let _ = encoder.set_signal(audiopus::Signal::Auto); }
    }
}

// ═══════════════════════════════════════════════════════════════════
// 反向音频发送器
// ═══════════════════════════════════════════════════════════════════

/// 反向音频发送器
pub struct ReverseSender {
    running: Arc<AtomicBool>,
    #[allow(dead_code)]
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl ReverseSender {
    /// low_perf=true: 单声道64kbps窄带（适配 android_old 低端设备）
    /// low_perf=false: 立体声最大码率全频带（默认高品质）
    pub fn start(android_addr: SocketAddr, device_id: [u8; 8], low_perf: bool) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        let handle = std::thread::Builder::new()
            .name("reverse-capture".into())
            .spawn(move || {
                Self::run_loop(android_addr, device_id, &running_clone, low_perf);
            })
            .ok();

        ReverseSender { running, thread_handle: handle }
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    fn run_loop(android_addr: SocketAddr, device_id: [u8; 8], running: &AtomicBool, low_perf: bool) {
        eprintln!("[Reverse] 启动反向音频发送到 {}", android_addr);

        unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }

        // 低性能模式: 单声道 64kbps 宽带 (16kHz); 高性能: 立体声最大码率全频带
        let (channels, codec, frame_samps, bitrate, bandwidth, sample_rate_id) = if low_perf {
            eprintln!("[Reverse] 低性能模式: 单声道 64kbps 16kHz 宽带");
            (audiopus::Channels::Mono, CODEC_MONO, FRAME_SIZE_16K,
             audiopus::Bitrate::BitsPerSecond(64000), audiopus::Bandwidth::Wideband, 2u8)
        } else {
            eprintln!("[Reverse] 高品质模式: 立体声 最大码率 全频带");
            (audiopus::Channels::Stereo, CODEC_STEREO, FRAME_SIZE * 2,
             audiopus::Bitrate::Max, audiopus::Bandwidth::Fullband, 4u8)
        };

        let sr = if low_perf { audiopus::SampleRate::Hz16000 } else { audiopus::SampleRate::Hz48000 };
        let mut encoder = match audiopus::coder::Encoder::new(sr, channels, audiopus::Application::Audio) {
            Ok(e) => e,
            Err(err) => { eprintln!("[Reverse] Opus 编码器失败: {:?}", err); return; }
        };
        let _ = encoder.set_bitrate(bitrate);
        let _ = encoder.set_bandwidth(bandwidth);
        let _ = encoder.set_vbr(true);
        if low_perf { let _ = encoder.set_complexity(1); }

        // 应用可能已接收到的手机端配置
        if let Ok(cfg) = reverse_config().lock() {
            if *cfg != OpusReverseConfig::default() {
                apply_config(&mut encoder, &cfg);
            }
        }

        let send_sock = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => { eprintln!("[Reverse] UDP socket 失败: {}", e); return; }
        };

        let capture_client = match init_loopback() {
            Some(c) => c,
            None => { eprintln!("[Reverse] 环回捕获初始化失败"); return; }
        };

        let mut pcm_buf: Vec<i16> = Vec::with_capacity(frame_samps);
        let mut accum_buf = [0i16; 960];  // 10ms stereo: 480 * 2chs
        let mut mono_buf: Vec<i16> = Vec::with_capacity(FRAME_SIZE * 2);
        let mut encode_out = [0u8; 1500];
        let mut seq_num: u8 = 0;
        let mut hdr = [0u8; 15];
        let mut last_cfg_check = std::time::Instant::now();

        while running.load(Ordering::Relaxed) {
            // ── 每 1 秒检查配置变化并热应用 ──
            if last_cfg_check.elapsed() > Duration::from_secs(1) {
                if let Ok(cfg) = reverse_config().lock() {
                    apply_config(&mut encoder, &cfg);
                }
                last_cfg_check = std::time::Instant::now();
            }

            let samples = match read_loopback_stereo(&capture_client) {
                Some(s) => s,
                None => { std::thread::sleep(Duration::from_millis(5)); continue; }
            };

            if low_perf {
                // 低性能模式不需要运行时调参（android_old 不支持同步）
                for sample_f32 in samples {
                    let s = (sample_f32.clamp(-1.0, 1.0) * 32767.0) as i16;
                    pcm_buf.push(s);
                }
                while pcm_buf.len() >= 2 {
                    let l = pcm_buf[0] as i32;
                    let r = pcm_buf[1] as i32;
                    mono_buf.push(((l + r) / 2) as i16);
                    pcm_buf.drain(0..2);
                }
                while mono_buf.len() >= FRAME_SIZE {
                    let mut decimated = [0i16; FRAME_SIZE_16K];
                    for i in 0..FRAME_SIZE_16K {
                        decimated[i] = mono_buf[i * 3];
                    }
                    let n = encoder.encode(&decimated[..FRAME_SIZE_16K], &mut encode_out).unwrap_or(0);
                    let plen = n.min(1472);
                    if plen > 0 {
                        hdr[0] = 2;
                        hdr[1] = (1u8 << 7) | (codec << 4) | sample_rate_id;
                        hdr[2] = 0;
                        hdr[3] = (plen >> 8) as u8;
                        hdr[4] = plen as u8;
                        hdr[5] = 0;
                        hdr[6] = seq_num;
                        seq_num = seq_num.wrapping_add(1);
                        hdr[7..15].copy_from_slice(&device_id);
                        let mut packet = Vec::with_capacity(15 + plen);
                        packet.extend_from_slice(&hdr[..15]);
                        packet.extend_from_slice(&encode_out[..plen]);
                        let _ = send_sock.send_to(&packet, android_addr);
                    }
                    mono_buf.drain(..FRAME_SIZE.min(mono_buf.len()));
                }
            } else {
                // 高品质模式: 立体声直通编码
                for sample_f32 in samples {
                    let s = (sample_f32.clamp(-1.0, 1.0) * 32767.0) as i16;
                    pcm_buf.push(s);
                }
                while pcm_buf.len() >= frame_samps {
                    accum_buf[..frame_samps].copy_from_slice(&pcm_buf[..frame_samps]);
                    pcm_buf.drain(..frame_samps);
                    if let Ok(n) = encoder.encode(&accum_buf[..frame_samps], &mut encode_out) {
                        let plen = n.min(1472);
                        if plen > 0 {
                            hdr[0] = 2;
                            hdr[1] = (1u8 << 7) | (codec << 4) | sample_rate_id;
                            hdr[2] = 0;
                            hdr[3] = (plen >> 8) as u8;
                            hdr[4] = plen as u8;
                            hdr[5] = 0;
                            hdr[6] = seq_num;
                            seq_num = seq_num.wrapping_add(1);
                            hdr[7..15].copy_from_slice(&device_id);
                            let mut packet = Vec::with_capacity(15 + plen);
                            packet.extend_from_slice(&hdr[..15]);
                            packet.extend_from_slice(&encode_out[..plen]);
                            let _ = send_sock.send_to(&packet, android_addr);
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        eprintln!("[Reverse] 反向音频线程退出");
    }
}

// ═══════════════════════════════════════════════════════════════════
// WASAPI 环回捕获
// ═══════════════════════════════════════════════════════════════════

/// 初始化 WASAPI 环回捕获
fn init_loopback() -> Option<IAudioCaptureClient> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(
            &MMDeviceEnumerator, None, CLSCTX_ALL,
        ).ok()?;

        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;

        let fmt = match client.GetMixFormat() {
            Ok(f) => f,
            Err(_) => return None,
        };

        let dur = (Duration::from_millis(20).as_micros() * 10) as i64;
        if client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            dur, 0, fmt, None,
        ).is_err() {
            CoTaskMemFree(Some(fmt as _));
            return None;
        }
        CoTaskMemFree(Some(fmt as _));

        let capture: IAudioCaptureClient = match client.GetService() {
            Ok(c) => c,
            Err(_) => return None,
        };

        client.Start().ok()?;
        eprintln!("[Reverse] WASAPI 环回捕获已启动");
        Some(capture)
    }
}

/// 从环回捕获读取一帧 32 位浮点 PCM，输出立体声交错（L,R,L,R,...）
fn read_loopback_stereo(capture: &IAudioCaptureClient) -> Option<Vec<f32>> {
    unsafe {
        let mut data_ptr: *mut u8 = std::ptr::null_mut();
        let mut packet_size: u32 = 0;
        let mut flags: u32 = 0;
        let mut dev_pos: u64 = 0;
        let mut qpc_pos: u64 = 0;

        let hr = capture.GetBuffer(
            &mut data_ptr as *mut *mut u8,
            &mut packet_size,
            &mut flags,
            Some(&mut dev_pos),
            Some(&mut qpc_pos),
        );
        if hr.is_err() || packet_size == 0 { return None; }

        let num_frames = packet_size as usize;
        let total_samples = num_frames * 2;
        let src = std::slice::from_raw_parts(data_ptr as *const f32, total_samples);

        let mut stereo = Vec::with_capacity(total_samples);
        stereo.extend_from_slice(src);

        let _ = capture.ReleaseBuffer(packet_size);
        Some(stereo)
    }
}
