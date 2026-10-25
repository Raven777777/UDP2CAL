// WASAPI 环回捕获 → Opus（10 ms）→ RTP → UDP
//
// v3 发送端规则（规范第八节）：
//   * 采集 → 编码 → 立即发送，**没有应用层音频队列**，失败只丢帧并计数；
//   * 固定 10 ms 帧，时间戳与序列号**都**每帧 +1（不发包的帧同样 +1）；
//   * SSRC 由控制面生成并随 CONNECT_ACK 下发，本模块只使用；
//   * 反向 Opus 配置来自 CONNECT，**只在变化时**应用（每秒重配会重置编码器状态）。
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use audiopus::{Application, Bitrate, Channels, SampleRate};

use crate::logf;
use crate::protocol;
use crate::protocol::rtp;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
    MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// 每声道 480 采样 = 10 ms @ 48 kHz
const FRAME_MONO: usize = 480;
/// 立体声交错样本数（一帧）
const FRAME_INTERLEAVED: usize = FRAME_MONO * 2;
/// 组帧缓冲硬上限：4 帧（40 ms）。超出的旧数据直接丢弃，绝不无限积压。
const MAX_FRAME_BACKLOG: usize = FRAME_INTERLEAVED * 4;
/// 静音补齐上限：连续 2 s 无数据后停止补帧（DTX 语义），恢复出声时重新开始
const MAX_SILENCE_FRAMES: u32 = 200;
/// 帧长（毫秒）：静音补齐的墙钟节拍基准
const FRAME_MS: u32 = rtp::FRAME_MS;
/// WASAPI 事件等待上限：超时仅用于"静音补齐"判断，不用于播放节拍
const WAIT_TIMEOUT_MS: u32 = 10;

/// AUDCLNT_BUFFERFLAGS_SILENT：置位时缓冲内容未定义，必须按静音处理
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;

// ═══════════════════════════════════════════════════════════════════
// 反向 Opus 编码配置（由 CONNECT 同步，配置变化才重新下发）
// ═══════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpusReverseConfig {
    pub bitrate_kbps: u16,
    pub bandwidth: u8,
    pub complexity: u8,
    pub signal: u8,
    pub vbr: bool,
    pub dtx: bool,
    pub fec: bool,
    pub vbr_constraint: bool,
    pub packet_loss: u8,
}

impl Default for OpusReverseConfig {
    fn default() -> Self {
        Self {
            bitrate_kbps: protocol::BITRATE_AUTO_KBPS,
            bandwidth: 4,
            complexity: 10,
            signal: 1,
            vbr: true,
            dtx: false,
            fec: false,
            vbr_constraint: false,
            packet_loss: 0,
        }
    }
}

impl From<&protocol::ConnectPayload> for OpusReverseConfig {
    fn from(p: &protocol::ConnectPayload) -> Self {
        // 码率 0（自动）= 上限；高于上限一律钳制（旧客户端的 768/960/1000 都落到 512）
        Self {
            bitrate_kbps: p.effective_bitrate_kbps(),
            bandwidth: p.bandwidth.min(4),
            complexity: if p.complexity == 0 { 10 } else { p.complexity.min(protocol::OPUS_COMPLEXITY_MAX) },
            signal: p.signal.min(2),
            vbr: p.vbr(),
            dtx: p.dtx(),
            fec: p.fec(),
            vbr_constraint: p.vbr_constraint(),
            packet_loss: p.packet_loss.min(100),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// 反向链路统计（原子计数，由 UI 线程读取并落盘）
//
// **绝不能**在发送线程里直接写日志：`logf!` 是同步文件 I/O，一次写盘
// 在杀毒软件扫描的机器上可以阻塞上百毫秒。发送线程一旦阻塞就会造成
// RTP 断流——实测表现为"每 30 s 一次"的破音：诊断日志本身把流打断了。
// ═══════════════════════════════════════════════════════════════════

#[derive(Default)]
pub struct ReverseStats {
    pub packets: AtomicU64,
    pub bytes: AtomicU64,
    pub encode_errors: AtomicU64,
    pub oversize: AtomicU64,
    pub dropped_samples: AtomicU64,
    pub send_errors: AtomicU64,
    /// 发送中断次数（两次发送间隔 > 200 ms，接收端会据此重同步）
    pub gaps: AtomicU64,
}

static REVERSE_STATS: ReverseStats = ReverseStats {
    packets: AtomicU64::new(0),
    bytes: AtomicU64::new(0),
    encode_errors: AtomicU64::new(0),
    oversize: AtomicU64::new(0),
    dropped_samples: AtomicU64::new(0),
    send_errors: AtomicU64::new(0),
    gaps: AtomicU64::new(0),
};

/// 读取并清零（30 s 汇总用，调用方为 UI 线程）
pub fn take_reverse_stats() -> (u64, u64, u64, u64, u64, u64, u64) {
    (
        REVERSE_STATS.packets.swap(0, Ordering::Relaxed),
        REVERSE_STATS.bytes.swap(0, Ordering::Relaxed),
        REVERSE_STATS.encode_errors.swap(0, Ordering::Relaxed),
        REVERSE_STATS.oversize.swap(0, Ordering::Relaxed),
        REVERSE_STATS.dropped_samples.swap(0, Ordering::Relaxed),
        REVERSE_STATS.send_errors.swap(0, Ordering::Relaxed),
        REVERSE_STATS.gaps.swap(0, Ordering::Relaxed),
    )
}

static REVERSE_CFG: OnceLock<Mutex<OpusReverseConfig>> = OnceLock::new();

fn reverse_config() -> &'static Mutex<OpusReverseConfig> {
    REVERSE_CFG.get_or_init(|| Mutex::new(OpusReverseConfig::default()))
}

/// 由 main.rs 的控制面调用；只有内容真正变化才落盘日志
pub fn update_reverse_config(cfg: OpusReverseConfig) {
    if let Ok(mut c) = reverse_config().lock() {
        if *c != cfg {
            logf!(
                "[Reverse] Opus 配置更新: br={} bw={} cplx={} sig={} vbr={} dtx={} fec={} pl={}",
                cfg.bitrate_kbps, cfg.bandwidth, cfg.complexity, cfg.signal,
                cfg.vbr, cfg.dtx, cfg.fec, cfg.packet_loss
            );
            *c = cfg;
        }
    }
}

fn apply_config(encoder: &mut audiopus::coder::Encoder, cfg: &OpusReverseConfig) {
    let kbps = protocol::clamp_bitrate_kbps(cfg.bitrate_kbps) as i32;
    // BITRATE 必须在 BANDWIDTH 之后：OPUS_SET_BANDWIDTH 会重置内部码率
    let _ = encoder.set_vbr(cfg.vbr);
    let _ = encoder.set_dtx(cfg.dtx);
    let _ = encoder.set_inband_fec(cfg.fec);
    let _ = encoder.set_packet_loss_perc(cfg.packet_loss);
    let _ = encoder.set_vbr_constraint(if cfg.vbr { cfg.vbr_constraint } else { false });
    // DRED（Opus 1.6 深度冗余）：FEC 开启时附带最长 500 ms 冗余
    let _ = encoder.set_dred_duration(if cfg.fec { 50 } else { 0 });
    let _ = encoder.set_complexity(cfg.complexity);
    let _ = encoder.set_signal(match cfg.signal {
        1 => audiopus::Signal::Voice,
        2 => audiopus::Signal::Music,
        _ => audiopus::Signal::Auto,
    });
    let _ = encoder.set_bandwidth(match cfg.bandwidth {
        0 => audiopus::Bandwidth::Narrowband,
        1 => audiopus::Bandwidth::Mediumband,
        2 => audiopus::Bandwidth::Wideband,
        3 => audiopus::Bandwidth::Superwideband,
        _ => audiopus::Bandwidth::Fullband,
    });
    let _ = encoder.set_bitrate(Bitrate::BitsPerSecond(kbps * 1000));
}

// ═══════════════════════════════════════════════════════════════════
// 反向音频发送器
// ═══════════════════════════════════════════════════════════════════

pub struct ReverseSender {
    running: Arc<AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl ReverseSender {
    /// `ssrc` 由控制面生成并写入 CONNECT_ACK；本函数不自行生成。
    pub fn start(target: SocketAddr, ssrc: u32) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        let handle = std::thread::Builder::new()
            .name("reverse-capture".into())
            .spawn(move || {
                Self::run_loop(target, ssrc, &running_clone);
            })
            .ok();

        ReverseSender { running, thread_handle: handle }
    }

    /// 请求停止，`join` 交给后台线程：调用方可能是 UI/执行器线程，
    /// 反向线程可能正卡在事件等待上，同步 join 会传导成界面卡顿。
    pub fn stop(mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.thread_handle.take() {
            let _ = std::thread::Builder::new()
                .name("reverse-join".into())
                .spawn(move || {
                    let _ = handle.join();
                });
        }
    }

    fn run_loop(target: SocketAddr, ssrc: u32, running: &AtomicBool) {
        logf!("[Reverse] 启动反向音频 → {} (ssrc={:#010x})", target, ssrc);

        let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };
        let _com_guard = ComGuard { active: com_ok };

        let mut encoder = match audiopus::coder::Encoder::new(
            SampleRate::Hz48000,
            Channels::Stereo,
            Application::Audio,
        ) {
            Ok(e) => e,
            Err(err) => {
                logf!("[Reverse] Opus 编码器创建失败: {err:?}");
                return;
            }
        };

        // 启动时下发一次；之后仅在配置变化时下发
        let mut applied = OpusReverseConfig::default();
        if let Ok(cfg) = reverse_config().lock() {
            applied = *cfg;
        }
        apply_config(&mut encoder, &applied);

        let send_sock = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                logf!("[Reverse] UDP socket 创建失败: {e}");
                return;
            }
        };

        let capture = match LoopbackCapture::new() {
            Some(c) => c,
            None => {
                logf!("[Reverse] 环回捕获初始化失败");
                return;
            }
        };

        // 端点混音率不是 48 kHz 时先归一再组帧（编码器固定 48 kHz）
        let mut resampler = StereoResampler::new(capture.sample_rate(), 48_000);
        let mut loopback: Vec<f32> = Vec::with_capacity(2_048);
        let mut resampled: Vec<f32> = Vec::with_capacity(2_048);
        let mut pcm_buf: Vec<i16> = Vec::with_capacity(MAX_FRAME_BACKLOG);
        let mut frame = vec![0i16; FRAME_INTERLEAVED];
        let mut payload = [0u8; rtp::OPUS_MAX_PAYLOAD];
        let mut packet = [0u8; rtp::RTP_MAX_PACKET];

        let mut seq: u16 = random_u16();
        let mut timestamp: u32 = random_u32();
        let mut first_packet = true;
        let mut last_sent: Option<Instant> = None;
        let mut silence_run: u32 = 0;
        // 端点最近一次真正给出数据的时间：只有它够久没更新时才补静音。
        // 缺少这个判据时，事件等待超时与"事件带回 20 ms 数据"会同时成立，
        // 结果每秒多发 3~4 帧，接收端缓冲十几秒就被灌满 → 周期性重同步（破音）。
        let mut last_data_at = Instant::now();
        let mut last_frame_at = Instant::now();

        // ── 1 帧节拍整形 ──
        // WASAPI 环回事件通常一次带回一个周期（10/20 ms），直接发就是"两包挨着、
        // 然后静默 20 ms"，接收端水位呈锯齿。这里把发送时刻钉在 10 ms 网格上：
        // 不是攒队列（最多只压 1 帧），而是把成簇的输出摊平。
        let frame_gap = Duration::from_millis(FRAME_MS as u64);
        let mut next_send_at = Instant::now();

        let mut cfg_check_at = Instant::now();
        while running.load(Ordering::Relaxed) {
            // ── 配置变化检测：500 ms 一次，避免每帧抢锁（发送线程不该有锁流量）──
            if cfg_check_at.elapsed() > Duration::from_millis(500) {
                cfg_check_at = Instant::now();
                if let Ok(cfg) = reverse_config().lock() {
                    if *cfg != applied {
                        apply_config(&mut encoder, &cfg);
                        applied = *cfg;
                    }
                }
            }

            // 有帧在等发送窗口时，等待时长按"离下一个发送时刻还有多久"算，
            // 这样既不阻塞采集读取，又能准点发出
            let wait_ms = if pcm_buf.len() >= FRAME_INTERLEAVED {
                let now = Instant::now();
                if next_send_at > now {
                    (next_send_at - now).as_millis().clamp(1, WAIT_TIMEOUT_MS as u128) as u32
                } else {
                    0
                }
            } else {
                WAIT_TIMEOUT_MS
            };
            let signaled = capture.wait(wait_ms);
            let got_data = capture.read_stereo(&mut loopback).is_some();

            if got_data {
                silence_run = 0;
                last_data_at = Instant::now();
                last_frame_at = Instant::now();
                resampler.process(&loopback, &mut resampled);
                if resampled.len() > MAX_FRAME_BACKLOG {
                    let excess = resampled.len() - MAX_FRAME_BACKLOG;
                    REVERSE_STATS.dropped_samples.fetch_add(excess as u64, Ordering::Relaxed);
                    resampled.drain(..excess);
                }
                for &s in &resampled {
                    let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    pcm_buf.push(v);
                }
            } else if !signaled
                && silence_run < MAX_SILENCE_FRAMES
                && last_data_at.elapsed() > Duration::from_millis(200)
                && last_frame_at.elapsed() >= Duration::from_millis(FRAME_MS as u64)
            {
                // 端点空闲超过 200 ms 才补静音，且严格按 10 ms 墙钟节拍——
                // 这是"静音补齐"，不是播放定时器，也绝不允许超过实时速率。
                silence_run += 1;
                last_frame_at = Instant::now();
                pcm_buf.extend(std::iter::repeat(0i16).take(FRAME_INTERLEAVED));
            }

            // 积压保护：超过 40 ms 只保留最新部分（发送端不得积压）
            if pcm_buf.len() > MAX_FRAME_BACKLOG {
                let excess = pcm_buf.len() - MAX_FRAME_BACKLOG;
                pcm_buf.drain(..excess);
                REVERSE_STATS.dropped_samples.fetch_add(excess as u64, Ordering::Relaxed);
            }

            // 只发到点的帧：节拍整形。落后超过 4 帧（调度抖动/设备暂停）时
            // 重置网格，绝不用连续突发把落后的时间补回来——那正是水位锯齿的来源。
            while pcm_buf.len() >= FRAME_INTERLEAVED && Instant::now() >= next_send_at {
                next_send_at += frame_gap;
                if next_send_at + 4 * frame_gap < Instant::now() {
                    next_send_at = Instant::now();
                }
                frame.copy_from_slice(&pcm_buf[..FRAME_INTERLEAVED]);
                pcm_buf.drain(..FRAME_INTERLEAVED);

                let encoded = encoder.encode(&frame, &mut payload);
                // `len == 0` 表示这一帧不产出负载（DTX 空帧 / 编码失败 / 超限），
                // 但**序列号仍然每帧 +1**：序列号与时间戳必须同频推进。
                //
                // 历史上这里按"是否发包"决定是否递增序列号（DTX 不递增，符合
                // 规范 4.4 的字面表述），但接收端在断流/静音期间会用 PLC/静音
                // 推进 next_seq，滞后量永不闭合；恢复包会先被"序号迟到"判定
                // 拦下，M=1 重同步分支永远不可达 → 反向音频永久静音。
                let len = match encoded {
                    Ok(n) if n > 0 && n <= rtp::OPUS_MAX_PAYLOAD => n,
                    Ok(n) if n > rtp::OPUS_MAX_PAYLOAD => {
                        // 超过 1275 字节：整帧丢弃，绝不截断
                        REVERSE_STATS.oversize.fetch_add(1, Ordering::Relaxed);
                        0
                    }
                    Ok(_) => 0, // DTX 空帧：时间戳推进，不发包
                    Err(e) => {
                        let n = REVERSE_STATS.encode_errors.fetch_add(1, Ordering::Relaxed);
                        if n < 3 {
                            logf!("[Reverse] 编码失败: {e:?}");
                        }
                        0
                    }
                };

                // 时间戳每帧推进 480（含 DTX / 丢帧）
                timestamp = timestamp.wrapping_add(rtp::FRAME_SAMPLES);
                if len > 0 {
                    let gap_ms = last_sent.map(|t| t.elapsed().as_millis()).unwrap_or(u128::MAX);
                    let marker = first_packet
                        || gap_ms > rtp::RESYNC_GAP_MS as u128;
                    if !first_packet && marker {
                        // 发送中断会让接收端重同步（flush + 重建解码器），必须计数备查
                        REVERSE_STATS.gaps.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Some(total) =
                        rtp::write_packet(&mut packet, marker, seq, timestamp.wrapping_sub(rtp::FRAME_SAMPLES), ssrc, &payload[..len])
                    {
                        match send_sock.send_to(&packet[..total], target) {
                            Ok(_) => {
                                REVERSE_STATS.packets.fetch_add(1, Ordering::Relaxed);
                                REVERSE_STATS.bytes.fetch_add(total as u64, Ordering::Relaxed);
                                last_sent = Some(Instant::now());
                                first_packet = false;
                            }
                            Err(e) => {
                                let n = REVERSE_STATS.send_errors.fetch_add(1, Ordering::Relaxed);
                                if n == 0 {
                                    logf!("[Reverse] 发送失败（后续同类错误不再打印）: {e}");
                                }
                            }
                        }
                    }
                }
                // 序列号与时间戳同频：每帧 +1（发送失败同样 +1）
                seq = seq.wrapping_add(1);
            }

        }

        logf!("[Reverse] 反向音频线程退出");
    }
}

fn random_u32() -> u32 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(SEQ.fetch_add(1, AtomicOrdering::Relaxed));
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    hasher.finish() as u32
}

fn random_u16() -> u16 {
    (random_u32() >> 8) as u16
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
// 立体声交错重采样（捕获侧）
// ═══════════════════════════════════════════════════════════════════

/// 线性插值 + 跨缓冲相位连续。
///
/// 只做"把端点混音率搬到 48 kHz"，不引入低通滤波：反向串流的目标是
/// 语音/系统提示音，48 kHz 以上的镜像分量功率极低。
struct StereoResampler {
    /// 每个输出帧消耗的输入帧数（src/dst）
    ratio: f64,
    /// 输入帧坐标系中的当前位置，始终保留 1 帧余量供下次插值
    phase: f64,
    in_buf: Vec<f32>,
}

impl StereoResampler {
    fn new(src_hz: u32, dst_hz: u32) -> Self {
        let ratio = if dst_hz == 0 { 1.0 } else { src_hz as f64 / dst_hz as f64 };
        Self { ratio, phase: 0.0, in_buf: Vec::with_capacity(4_096) }
    }

    #[inline]
    fn is_passthrough(&self) -> bool {
        (self.ratio - 1.0).abs() < 1e-9
    }

    /// 处理一块交错立体声输入，结果写入 `out`（会先清空）
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();
        if input.is_empty() {
            return;
        }
        if self.is_passthrough() {
            out.extend_from_slice(input);
            return;
        }

        self.in_buf.extend_from_slice(input);
        let frames = self.in_buf.len() / 2;
        if frames < 2 {
            return;
        }

        loop {
            let i = self.phase as usize;
            if i + 1 >= frames {
                break; // 需要下一帧才能插值
            }
            let frac = (self.phase - i as f64) as f32;
            for ch in 0..2 {
                let a = self.in_buf[i * 2 + ch];
                let b = self.in_buf[(i + 1) * 2 + ch];
                out.push(a + (b - a) * frac);
            }
            self.phase += self.ratio;
        }

        // 丢弃已经完全消费的整帧，保留 1 帧给下一块做插值
        let consumed = self.phase as usize;
        if consumed > 0 {
            self.in_buf.drain(0..consumed * 2);
            self.phase -= consumed as f64;
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// WASAPI 环回捕获（事件驱动，带静音补齐）
// ═══════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleKind {
    F32,
    I16,
    I32,
}

#[derive(Clone, Copy, Debug)]
struct CaptureFormat {
    channels: usize,
    kind: SampleKind,
    sample_rate: u32,
}

struct LoopbackCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    format: CaptureFormat,
    event: Option<HANDLE>,
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            if let Some(h) = self.event.take() {
                let _ = CloseHandle(h);
            }
        }
    }
}

impl LoopbackCapture {
    fn new() -> Option<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;

            let fmt_ptr = client.GetMixFormat().ok()?;
            let format = parse_mix_format(fmt_ptr);

            // 事件驱动：WASAPI 事件 + 10 ms 逻辑帧。缓冲区取 20 ms（事件粒度）。
            let event = CreateEventW(None, false, false, windows::core::PCWSTR::null()).ok();
            let mut flags = AUDCLNT_STREAMFLAGS_LOOPBACK;
            if event.is_some() {
                flags |= AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
            }
            let dur = 200_000i64; // 20 ms，单位 100 ns
            let init_ok = client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, dur, 0, fmt_ptr, None)
                .is_ok();
            CoTaskMemFree(Some(fmt_ptr as _));
            if !init_ok {
                if let Some(h) = event {
                    let _ = CloseHandle(h);
                }
                return None;
            }

            let format = match format {
                Some(f) => f,
                None => {
                    logf!("[Reverse] 设备混音格式不受支持（需 f32/i16/i32）");
                    if let Some(h) = event {
                        let _ = CloseHandle(h);
                    }
                    return None;
                }
            };

            // 环回端点在某些驱动上不接受事件回调：失败就退回超时轮询（10 ms 粒度）
            let mut event = event;
            if let Some(h) = event {
                if client.SetEventHandle(h).is_err() {
                    logf!("[Reverse] 环回端点不支持事件回调，改用 10 ms 超时轮询");
                    let _ = CloseHandle(h);
                    event = None;
                }
            }

            let capture: IAudioCaptureClient = client.GetService().ok()?;
            client.Start().ok()?;
            logf!(
                "[Reverse] WASAPI 环回已启动（{} 声道 / {:?} / {} Hz / {})",
                format.channels,
                format.kind,
                format.sample_rate,
                if event.is_some() { "事件驱动" } else { "轮询" }
            );
            Some(Self { client, capture, format, event })
        }
    }

    fn sample_rate(&self) -> u32 {
        self.format.sample_rate
    }

    /// 等待一次数据就绪。返回 `true` 表示事件已触发，`false` 表示超时。
    fn wait(&self, timeout_ms: u32) -> bool {
        match self.event {
            Some(h) => {
                let r = unsafe { WaitForSingleObject(h, timeout_ms) };
                r == WAIT_OBJECT_0
            }
            None => {
                // 无事件可用：用一次短睡代替（这是降级路径，正常不会走到）
                std::thread::sleep(Duration::from_millis(timeout_ms as u64));
                false
            }
        }
    }

    /// 读取一帧 PCM，输出**固定立体声交错** f32（L,R,L,R,...）
    fn read_stereo(&self, out: &mut Vec<f32>) -> Option<()> {
        unsafe {
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;

            let hr = self.capture.GetBuffer(
                &mut data_ptr as *mut *mut u8,
                &mut frames,
                &mut flags,
                None,
                None,
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
unsafe fn parse_mix_format(fmt: *const WAVEFORMATEX) -> Option<CaptureFormat> {
    if fmt.is_null() {
        return None;
    }
    let wf = &*fmt;
    let channels = wf.nChannels as usize;
    if channels == 0 || channels > 32 {
        return None;
    }
    let sample_rate = wf.nSamplesPerSec;
    if !(8000..=384_000).contains(&sample_rate) {
        return None;
    }
    let bits = wf.wBitsPerSample;
    let tag = wf.wFormatTag;

    let is_float = match (tag, wf.cbSize) {
        (3, _) => true,  // WAVE_FORMAT_IEEE_FLOAT
        (1, _) => false, // WAVE_FORMAT_PCM
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

    Some(CaptureFormat { channels, kind, sample_rate })
}

// ═══════════════════════════════════════════════════════════════════
// 单元测试：捕获侧重采样
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_passthrough_is_bit_exact() {
        let mut rs = StereoResampler::new(48_000, 48_000);
        let input: Vec<f32> = (0..960).map(|i| i as f32 * 1e-4).collect();
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn resampler_44k1_to_48k_first_block() {
        let mut rs = StereoResampler::new(44_100, 48_000);
        let input = vec![0.5f32; 441 * 2];
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        let frames = out.len() / 2;
        assert!((frames as i64 - 480).abs() <= 2, "frames={frames}");
    }

    #[test]
    fn resampler_keeps_phase_across_blocks() {
        let mut rs = StereoResampler::new(44_100, 48_000);
        let input = vec![0.5f32; 441 * 2];
        let mut out = Vec::new();
        let mut total = 0usize;
        for _ in 0..10 {
            rs.process(&input, &mut out);
            total += out.len() / 2;
        }
        assert!((total as i64 - 4_800).abs() <= 4, "total={total}");
    }

    #[test]
    fn parse_mix_format_reads_sample_rate() {
        let mut wf = WAVEFORMATEX {
            wFormatTag: 3,
            nChannels: 2,
            nSamplesPerSec: 44_100,
            wBitsPerSample: 32,
            nBlockAlign: 8,
            nAvgBytesPerSec: 44_100 * 8,
            cbSize: 0,
        };
        let fmt = unsafe { parse_mix_format(&mut wf as *mut WAVEFORMATEX) }.expect("应支持 f32/2ch");
        assert_eq!(fmt.sample_rate, 44_100);
        assert_eq!(fmt.channels, 2);
        assert_eq!(fmt.kind, SampleKind::F32);
    }

    #[test]
    fn connect_payload_maps_to_encoder_config() {
        let p = protocol::ConnectPayload {
            bitrate_kbps: 0,
            complexity: 0,
            bandwidth: 9,
            signal: 7,
            opus_flags: protocol::OPUS_FLAG_VBR | protocol::OPUS_FLAG_FEC,
            ..protocol::ConnectPayload::default()
        };
        let cfg = OpusReverseConfig::from(&p);
        assert_eq!(cfg.bitrate_kbps, 512); // 0 → 自动 = 上限
        assert_eq!(cfg.complexity, 10); // 0 → 默认
        assert_eq!(cfg.bandwidth, 4); // 非法值钳制
        assert_eq!(cfg.signal, 2); // 非法值钳制
        assert!(cfg.vbr && cfg.fec && !cfg.dtx);

        let p = protocol::ConnectPayload { bitrate_kbps: 1024, ..protocol::ConnectPayload::default() };
        assert_eq!(OpusReverseConfig::from(&p).bitrate_kbps, 512, "高于上限必须钳制");
        let p = protocol::ConnectPayload { bitrate_kbps: 64, ..protocol::ConnectPayload::default() };
        assert_eq!(OpusReverseConfig::from(&p).bitrate_kbps, 64, "低档位原样透传");
    }
}
