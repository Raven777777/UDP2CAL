// WASAPI 环回捕获 + Opus 编码 + 反向 UDP 发送
// 捕获系统音频输出（VB-Cable/扬声器）→ Opus 编码 → 发送到 Android 手机
// 支持运行时从 CONNECT 保活包同步手机端 Opus 设置

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::protocol;

use windows::Win32::Media::Audio::{
    eConsole, eRender, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    CLSCTX_ALL, COINIT_MULTITHREADED,
};

const FRAME_SIZE: usize = 480;               // 48000 * 10ms 单声道帧
const FRAME_SAMPLES: usize = FRAME_SIZE * 2; // 立体声交错样本数

/// AUDCLNT_BUFFERFLAGS_SILENT：置位时缓冲内容未定义，必须按静音处理
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;

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

    let _ = encoder.set_complexity(cfg.complexity);
    let _ = encoder.set_vbr(cfg.vbr);
    let _ = encoder.set_dtx(cfg.dtx);
    let _ = encoder.set_inband_fec(cfg.fec);
    let _ = encoder.set_packet_loss_perc(cfg.packet_loss);
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
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl ReverseSender {
    /// 立体声 48kHz 全频带编码，码率由手机端 CONNECT 携带的 Opus 参数热同步
    pub fn start(android_addr: SocketAddr, device_id: [u8; 8]) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        let handle = std::thread::Builder::new()
            .name("reverse-capture".into())
            .spawn(move || {
                Self::run_loop(android_addr, device_id, &running_clone);
            })
            .ok();

        ReverseSender { running, thread_handle: handle }
    }

    /// 停止并等待线程退出。
    /// 必须 join：否则端口切换时新旧线程会重叠，手机端会收到重复音频。
    pub fn stop_and_join(mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }

    fn run_loop(android_addr: SocketAddr, device_id: [u8; 8], running: &AtomicBool) {
        eprintln!("[Reverse] 启动反向音频发送到 {}", android_addr);

        // COM 初始化与反初始化严格配对，避免每次重连泄漏引用计数
        let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };
        let _com_guard = ComGuard { active: com_ok };

        let mut encoder = match audiopus::coder::Encoder::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Stereo,
            audiopus::Application::Audio,
        ) {
            Ok(e) => e,
            Err(err) => { eprintln!("[Reverse] Opus 编码器失败: {:?}", err); return; }
        };
        // 默认参数：最大码率 + 全频带；手机端配置到达后由 apply_config 覆盖
        let _ = encoder.set_bitrate(audiopus::Bitrate::Max);
        let _ = encoder.set_bandwidth(audiopus::Bandwidth::Fullband);
        let _ = encoder.set_vbr(true);

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

        let capture = match LoopbackCapture::new() {
            Some(c) => c,
            None => { eprintln!("[Reverse] 环回捕获初始化失败"); return; }
        };

        let mut pcm_buf: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES);
        let mut accum_buf = vec![0i16; FRAME_SAMPLES];
        let mut encode_out = [0u8; 1500];
        let mut seq_num: u8 = 0;
        // 栈上发送缓冲区，build_packet_to 直接写入，消除 Vec 分配
        let mut pkt_buf = [0u8; 1500];
        // 复用 loopback 读取缓冲区，消除高频 Vec 分配
        let mut loopback_samples: Vec<f32> = Vec::with_capacity(960);
        let mut last_cfg_check = std::time::Instant::now();
        // pcm_buf 硬上限 ~3 秒（288000 交错样本 / 96000 样本每秒）
        const MAX_PCM: usize = 288000;
        let mut send_errors: u64 = 0;

        while running.load(Ordering::Relaxed) {
            // ── 每 1 秒检查配置变化并热应用 ──
            if last_cfg_check.elapsed() > Duration::from_secs(1) {
                if let Ok(cfg) = reverse_config().lock() {
                    apply_config(&mut encoder, &cfg);
                }
                last_cfg_check = std::time::Instant::now();
            }

            if capture.read_stereo(&mut loopback_samples).is_none() {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }

            for &sample_f32 in &*loopback_samples {
                let s = (sample_f32.clamp(-1.0, 1.0) * 32767.0) as i16;
                pcm_buf.push(s);
            }
            if pcm_buf.len() > MAX_PCM {
                pcm_buf.drain(0..pcm_buf.len() - MAX_PCM);
            }

            while pcm_buf.len() >= FRAME_SAMPLES {
                accum_buf[..FRAME_SAMPLES].copy_from_slice(&pcm_buf[..FRAME_SAMPLES]);
                pcm_buf.drain(..FRAME_SAMPLES);
                if let Ok(n) = encoder.encode(&accum_buf[..FRAME_SAMPLES], &mut encode_out) {
                    // 超长包必须丢弃：截断会产出非法 Opus 包，解码端只会报错
                    if n > 0 && n <= protocol::MAX_PAYLOAD {
                        seq_num = seq_num.wrapping_add(1);
                        if let Some(total) = protocol::build_packet_to(
                            &mut pkt_buf, 0, true, protocol::TYPE_DATA,
                            protocol::SAMPLE_RATE_48K, seq_num,
                            &device_id, &encode_out[..n], 0,
                        ) {
                            if let Err(e) = send_sock.send_to(&pkt_buf[..total], android_addr) {
                                send_errors += 1;
                                if send_errors == 1 {
                                    eprintln!("[Reverse] 发送失败（后续同类错误不再打印）: {}", e);
                                }
                            }
                        }
                    }
                }
            }

            std::thread::sleep(Duration::from_millis(2));
        }

        eprintln!("[Reverse] 反向音频线程退出（发送错误 {} 次）", send_errors);
    }
}

/// COM 初始化守卫：无论从哪条路径退出都会正确反初始化
struct ComGuard {
    active: bool,
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.active {
            unsafe { CoUninitialize() };
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// WASAPI 环回捕获
// ═══════════════════════════════════════════════════════════════════

/// 混音格式的样本类型
#[derive(Clone, Copy, Debug, PartialEq)]
enum SampleKind {
    F32,
    I16,
    I32,
}

/// 设备混音格式（决定如何解释 GetBuffer 返回的裸内存）
#[derive(Clone, Copy, Debug)]
struct CaptureFormat {
    channels: usize,
    kind: SampleKind,
}

struct LoopbackCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    format: CaptureFormat,
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        unsafe { let _ = self.client.Stop(); }
    }
}

impl LoopbackCapture {
    fn new() -> Option<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(
                &MMDeviceEnumerator, None, CLSCTX_ALL,
            ).ok()?;

            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;

            let fmt_ptr = client.GetMixFormat().ok()?;
            // 关键：必须先解析格式再释放。历史实现直接丢弃格式信息，
            // 导致下方按“f32 立体声”硬解码，在 i16 / 多声道设备上越界或错音。
            let format = parse_mix_format(fmt_ptr);

            let dur = (Duration::from_millis(20).as_micros() * 10) as i64;
            let init_ok = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                dur, 0, fmt_ptr, None,
            ).is_ok();
            CoTaskMemFree(Some(fmt_ptr as _));
            if !init_ok {
                return None;
            }

            let format = match format {
                Some(f) => f,
                None => {
                    eprintln!("[Reverse] 设备混音格式不受支持（需 f32/i16/i32）");
                    return None;
                }
            };

            let capture: IAudioCaptureClient = client.GetService().ok()?;
            client.Start().ok()?;
            eprintln!(
                "[Reverse] WASAPI 环回捕获已启动（{} 声道 / {:?}）",
                format.channels, format.kind
            );
            Some(Self { client, capture, format })
        }
    }

    /// 读取一帧 PCM，输出**固定立体声交错** f32（L,R,L,R,...）
    /// 返回 None 表示当前无数据
    fn read_stereo(&self, out: &mut Vec<f32>) -> Option<()> {
        unsafe {
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            let mut dev_pos: u64 = 0;
            let mut qpc_pos: u64 = 0;

            let hr = self.capture.GetBuffer(
                &mut data_ptr as *mut *mut u8,
                &mut frames,
                &mut flags,
                Some(&mut dev_pos),
                Some(&mut qpc_pos),
            );
            if hr.is_err() || frames == 0 {
                return None;
            }

            let frame_count = frames as usize;
            out.clear();
            out.reserve(frame_count * 2);

            // 静音标志：缓冲内容未定义，必须当作零样本
            if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 || data_ptr.is_null() {
                out.resize(frame_count * 2, 0.0);
                let _ = self.capture.ReleaseBuffer(frames);
                return Some(());
            }

            let channels = self.format.channels;
            let c0 = 0usize;
            let c1 = if channels > 1 { 1 } else { 0 };

            match self.format.kind {
                SampleKind::F32 => {
                    let src = std::slice::from_raw_parts(data_ptr as *const f32, frame_count * channels);
                    for f in 0..frame_count {
                        out.push(src[f * channels + c0]);
                        out.push(src[f * channels + c1]);
                    }
                }
                SampleKind::I16 => {
                    let src = std::slice::from_raw_parts(data_ptr as *const i16, frame_count * channels);
                    for f in 0..frame_count {
                        out.push(src[f * channels + c0] as f32 / 32768.0);
                        out.push(src[f * channels + c1] as f32 / 32768.0);
                    }
                }
                SampleKind::I32 => {
                    let src = std::slice::from_raw_parts(data_ptr as *const i32, frame_count * channels);
                    for f in 0..frame_count {
                        out.push(src[f * channels + c0] as f32 / 2_147_483_648.0);
                        out.push(src[f * channels + c1] as f32 / 2_147_483_648.0);
                    }
                }
            }

            let _ = self.capture.ReleaseBuffer(frames);
            Some(())
        }
    }
}

/// 解析 WAVEFORMATEX / WAVEFORMATEXTENSIBLE
/// 返回 None 表示不支持的格式（宁可失败也不越界读取）
unsafe fn parse_mix_format(fmt: *const WAVEFORMATEX) -> Option<CaptureFormat> {
    if fmt.is_null() {
        return None;
    }
    let wf = &*fmt;
    let channels = wf.nChannels as usize;
    if channels == 0 || channels > 32 {
        return None;
    }
    let bits = wf.wBitsPerSample;
    let tag = wf.wFormatTag;

    let is_float = match (tag, wf.cbSize) {
        (3, _) => true,   // WAVE_FORMAT_IEEE_FLOAT
        (1, _) => false,  // WAVE_FORMAT_PCM
        // WAVE_FORMAT_EXTENSIBLE：真实格式在 SubFormat GUID 的 Data1
        (0xFFFE, cb_size) if cb_size >= 22 => {
            let ext = &*(fmt as *const WAVEFORMATEXTENSIBLE);
            ext.SubFormat.data1 == 3
        }
        _ => return None,
    };

    let kind = match (is_float, bits) {
        (true, 32) => SampleKind::F32,
        (false, 16) => SampleKind::I16,
        (false, 32) => SampleKind::I32,
        _ => return None,
    };

    Some(CaptureFormat { channels, kind })
}
