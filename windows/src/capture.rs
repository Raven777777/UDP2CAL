// WASAPI 环回捕获 → Opus（10 ms）→ RTP → UDP
//
// v3 发送端规则（规范第八节）：
//   * 采集 → 编码 → 立即发送，应用层只允许 ≤40 ms 的组帧缓冲，
//     丢弃时时间戳必须跟随真实媒体跳变（接收端据此 PLC/重同步）；
//   * 固定 10 ms 帧，时间戳每帧 +1；序列号只对**真正发出的包** +1
//     （DTX / 发送失败不占序号，RFC 3550）；
//   * SSRC 由控制面生成并随 CONNECT_ACK 下发，本模块只使用；
//   * 反向 Opus 配置来自 CONNECT，**只在变化时**应用（每秒重配会重置编码器状态）。
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use audiopus::{Application, Bitrate, Channels, SampleRate};

use crate::hires::HiResTimer;
use crate::logf;
use crate::protocol;
use crate::protocol::rtp;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// DSCP EF（46）→ IP TOS 字节 0xB8。反向音频流的转发优先级最高，
/// 让 Wi-Fi 接入点 / 路由器把它排在其他流量前面，抑制排队抖动。
const DSCP_EF_TOS: u32 = 0xB8;

/// 每声道 480 采样 = 10 ms @ 48 kHz
const FRAME_MONO: usize = 480;
/// 立体声交错样本数（一帧）
const FRAME_INTERLEAVED: usize = FRAME_MONO * 2;
/// 组帧缓冲硬上限：4 帧（40 ms）。超出的旧数据直接丢弃，绝不无限积压。
const MAX_FRAME_BACKLOG: usize = FRAME_INTERLEAVED * 4;
// 端点空闲时**不再**由发送端合成静音：接收端在本机用静音维持
// AudioTrack，RTP 只在有真实媒体时发送。这样端点失效不会被静音掩盖。
/// 帧长（毫秒）：发送节拍与空闲时间戳补偿的基准
const FRAME_MS: u32 = rtp::FRAME_MS;
/// WASAPI 事件等待上限：超时仅用于轮询/重建判断，不用于播放节拍
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
            complexity: if p.complexity == 0 {
                10
            } else {
                p.complexity.min(protocol::OPUS_COMPLEXITY_MAX)
            },
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
    /// 分段耗时（微秒，30 s 窗口累加）：定位隐藏积压/调度阻塞
    pub wait_us: AtomicU64,
    pub read_us: AtomicU64,
    pub encode_us: AtomicU64,
    pub send_us: AtomicU64,
    /// 实际组帧数（用于把耗时累加换算成均值）
    pub frames: AtomicU64,
    /// 组帧缓冲峰值（交错样本数；2 样本 = 1 个 48 kHz 帧）
    pub max_backlog: AtomicU64,
    /// 节拍迟到量（微秒）：实际发送时刻 − 计划发送时刻。
    ///
    /// 这是判断"发送网格是否准"的唯一直接指标：均值为 0 说明 10 ms 网格
    /// 被完整保持；出现周期性几毫秒的迟到就是定时器精度不足（见 `hires.rs`）。
    /// 统计口径照抄 Sunshine 的 `Sleep overshoot`：min/max/avg，**不用 EWMA**
    /// ——EWMA 会把偶发的百毫秒级卡顿抹平，而卡顿恰恰是最该看见的东西。
    pub late_us: AtomicU64,
    pub late_min_us: AtomicU64,
    pub late_max_us: AtomicU64,
    pub late_n: AtomicU64,
    /// 网格重锚次数：迟到超过半个帧就重锚（见 `slot_is_late`），这类帧
    /// **不计入**上面的迟到统计——它不是"网格不准"，而是网格被空闲期、
    /// 线程抢占或设备重开撕开过。混在一起会污染均值：实测一次 8 s 的
    /// 无声间隔就把窗口均值从 0.3 ms 拉到 4.2 ms。
    pub reanchors: AtomicU64,
}

static REVERSE_STATS: ReverseStats = ReverseStats {
    packets: AtomicU64::new(0),
    bytes: AtomicU64::new(0),
    encode_errors: AtomicU64::new(0),
    oversize: AtomicU64::new(0),
    dropped_samples: AtomicU64::new(0),
    send_errors: AtomicU64::new(0),
    gaps: AtomicU64::new(0),
    wait_us: AtomicU64::new(0),
    read_us: AtomicU64::new(0),
    encode_us: AtomicU64::new(0),
    send_us: AtomicU64::new(0),
    frames: AtomicU64::new(0),
    max_backlog: AtomicU64::new(0),
    late_us: AtomicU64::new(0),
    late_min_us: AtomicU64::new(u64::MAX),
    late_max_us: AtomicU64::new(0),
    late_n: AtomicU64::new(0),
    reanchors: AtomicU64::new(0),
};

/// 一个统计窗口的快照（读取即清零，调用方为 UI 线程）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReverseSnapshot {
    pub packets: u64,
    pub bytes: u64,
    pub encode_errors: u64,
    pub oversize: u64,
    pub dropped_samples: u64,
    pub send_errors: u64,
    pub gaps: u64,
    pub wait_us: u64,
    pub read_us: u64,
    pub encode_us: u64,
    pub send_us: u64,
    pub frames: u64,
    pub max_backlog: u64,
    pub late_us: u64,
    /// 窗口内最小迟到量（无样本时为 0）
    pub late_min_us: u64,
    pub late_max_us: u64,
    pub late_n: u64,
    /// 窗口内因迟到超过半个帧而重锚的次数（不计入上面的迟到统计）
    pub reanchors: u64,
}

impl ReverseSnapshot {
    /// 每帧平均耗时（微秒）：`packets == 0` 时按 1 帧算，避免除零
    pub fn avg_us(&self, sum: u64) -> f64 {
        sum as f64 / self.frames.max(1) as f64
    }

    /// 节拍迟到均值（微秒）
    pub fn late_avg_us(&self) -> u64 {
        if self.late_n == 0 {
            return 0;
        }
        self.late_us / self.late_n
    }
}

/// 读取并清零（30 s 汇总用，调用方为 UI 线程）
pub fn take_reverse_stats() -> ReverseSnapshot {
    let s = ReverseSnapshot {
        packets: REVERSE_STATS.packets.swap(0, Ordering::Relaxed),
        bytes: REVERSE_STATS.bytes.swap(0, Ordering::Relaxed),
        encode_errors: REVERSE_STATS.encode_errors.swap(0, Ordering::Relaxed),
        oversize: REVERSE_STATS.oversize.swap(0, Ordering::Relaxed),
        dropped_samples: REVERSE_STATS.dropped_samples.swap(0, Ordering::Relaxed),
        send_errors: REVERSE_STATS.send_errors.swap(0, Ordering::Relaxed),
        gaps: REVERSE_STATS.gaps.swap(0, Ordering::Relaxed),
        wait_us: REVERSE_STATS.wait_us.swap(0, Ordering::Relaxed),
        read_us: REVERSE_STATS.read_us.swap(0, Ordering::Relaxed),
        encode_us: REVERSE_STATS.encode_us.swap(0, Ordering::Relaxed),
        send_us: REVERSE_STATS.send_us.swap(0, Ordering::Relaxed),
        frames: REVERSE_STATS.frames.swap(0, Ordering::Relaxed),
        max_backlog: REVERSE_STATS.max_backlog.swap(0, Ordering::Relaxed),
        late_us: REVERSE_STATS.late_us.swap(0, Ordering::Relaxed),
        late_min_us: REVERSE_STATS.late_min_us.swap(u64::MAX, Ordering::Relaxed),
        late_max_us: REVERSE_STATS.late_max_us.swap(0, Ordering::Relaxed),
        late_n: REVERSE_STATS.late_n.swap(0, Ordering::Relaxed),
        reanchors: REVERSE_STATS.reanchors.swap(0, Ordering::Relaxed),
    };
    // 窗口内没有任何发送时 min 仍是哨兵值，对外一律报 0
    let late_min_us = if s.late_n == 0 { 0 } else { s.late_min_us };
    ReverseSnapshot { late_min_us, ..s }
}

static REVERSE_CFG: OnceLock<Mutex<OpusReverseConfig>> = OnceLock::new();

fn reverse_config() -> &'static Mutex<OpusReverseConfig> {
    REVERSE_CFG.get_or_init(|| Mutex::new(OpusReverseConfig::default()))
}

/// 由 main.rs 的控制面调用；只有内容真正变化才落盘日志
pub fn update_reverse_config(cfg: OpusReverseConfig) {
    // 中毒恢复与其他 Mutex 一致：这里只是一份配置快照，没有跨字段不变量。
    //
    // 锁内只做比较 + 赋值：`logf!` 是同步文件 I/O，而反向发送线程每 500 ms
    // 要抢同一把锁（capture.rs 的 run_loop）。在锁内落盘意味着磁盘慢时
    // 会把发送线程一起拖住，反向 RTP 断流。
    let changed = {
        let mut c = reverse_config().lock().unwrap_or_else(|e| e.into_inner());
        if *c == cfg {
            false
        } else {
            *c = cfg;
            true
        }
    };
    if changed {
        logf!(
            "[Reverse] Opus 配置更新: br={} bw={} cplx={} sig={} vbr={} dtx={} fec={} pl={}",
            cfg.bitrate_kbps,
            cfg.bandwidth,
            cfg.complexity,
            cfg.signal,
            cfg.vbr,
            cfg.dtx,
            cfg.fec,
            cfg.packet_loss
        );
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
    // DRED 不在本项目中启用：接收端没有 DRED 解析/恢复路径，发送端单方面
    // 打开只会白占码率。保留 0 显式关闭，避免配置漂移。
    let _ = encoder.set_dred_duration(0);
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
    /// 线程真正退出后置 false；仅看 `Some(ReverseSender)` 不能判断它还在跑。
    alive: Arc<AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

struct AliveGuard<'a>(&'a AtomicBool);

impl Drop for AliveGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl ReverseSender {
    /// `ssrc` 由控制面生成并写入 CONNECT_ACK；本函数不自行生成。
    pub fn start(target: SocketAddr, ssrc: u32) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let alive = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let alive_clone = alive.clone();

        let handle = std::thread::Builder::new()
            .name("reverse-capture".into())
            .spawn(move || {
                Self::run_loop(target, ssrc, &running_clone, &alive_clone);
            })
            .ok();
        if handle.is_none() {
            alive.store(false, Ordering::Release);
            logf!("[Reverse] 反向音频线程创建失败");
        }

        ReverseSender {
            running,
            alive,
            thread_handle: handle,
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// 请求停止并等待线程退出。
    ///
    /// 旧的异步 join 会让 `restart_reverse_sender` 在新线程启动时仍持有
    /// 环回端点，正是“重启后无声”的竞态来源。反向线程的等待上限是
    /// `WAIT_TIMEOUT_MS`（10 ms），同步 join 的阻塞可接受。
    pub fn stop(mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
        self.alive.store(false, Ordering::Release);
    }

    fn run_loop(target: SocketAddr, ssrc: u32, running: &AtomicBool, alive: &AtomicBool) {
        let _alive_guard = AliveGuard(alive);
        logf!("[Reverse] 启动反向音频 → {} (ssrc={:#010x})", target, ssrc);

        // SAFETY: 无指针参数；HRESULT 成功（含 S_FALSE）时本线程的 COM 初始化
        // 计数 +1，与下面 ComGuard 里唯一一次 CoUninitialize 严格配对。
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
        let mut applied = *reverse_config().lock().unwrap_or_else(|e| e.into_inner());
        apply_config(&mut encoder, &applied);

        let send_sock = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                logf!("[Reverse] UDP socket 创建失败: {e}");
                return;
            }
        };

        // ── DSCP EF 标记（A2）──
        // 设置本身可能被组策略拦下（DisableUserTOSSetting=1）或需要管理员，
        // 失败不影响功能：仅告警，继续用默认优先级发送。
        match socket2::SockRef::from(&send_sock).set_tos_v4(DSCP_EF_TOS) {
            Ok(()) => logf!("[Reverse] 发送套接字已标记 DSCP EF（TOS=0xB8）"),
            Err(e) => logf!(
                "[Reverse] DSCP 标记失败（{e}）：需管理员权限或 DisableUserTOSSetting=0；音频仍可正常发送"
            ),
        }

        // ── 发送缓冲（借鉴 Sunshine 的 1 MiB `SO_SNDBUF`）──
        // Windows 默认发送缓冲只有 8 KiB 量级：一次调度抖动里连发几包就可能
        // `send` 阻塞或丢包，而丢一包就是一个可闻的 PLC。反向音频码率很低
        // （≤512 kbps，每 10 ms 一包 ≤1275 B），1 MiB 只是把突发吸收掉，
        // 不会引入排队延迟（发送线程仍是每 10 ms 一包）。
        const SEND_BUFFER_BYTES: usize = 1 << 20;
        if let Err(e) = socket2::SockRef::from(&send_sock).set_send_buffer_size(SEND_BUFFER_BYTES) {
            logf!("[Reverse] 发送缓冲设置失败（{e}），继续使用系统默认值");
        }

        // 环回采集客户端可重建：设备失效（AUDCLNT_E_DEVICE_INVALIDATED）、
        // 音频服务重启、端点切换后，旧客户端可能永远返回错误。不能把它当成
        // “端点空闲”而一直发合成静音，否则重连前手机永远听不到声音。
        let mut capture: Option<LoopbackCapture> = None;
        // 端点混音率不是 48 kHz 时先归一再组帧（编码器固定 48 kHz）
        let mut resampler: Option<StereoResampler> = None;
        let mut last_capture_attempt: Option<Instant> = None;
        let mut last_capture_fail_log = Instant::now() - Duration::from_secs(10);
        let mut read_errors: u32 = 0;
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
        let mut last_sent_ts: Option<u32> = None;
        // 端点最近一次真正给出数据的时间：只用于恢复时补偿媒体时间戳。
        let mut last_frame_at = Instant::now();

        // ── 1 帧节拍整形 ──
        // WASAPI 环回事件通常一次带回一个周期（10/20 ms），直接发就是"两包挨着、
        // 然后静默 20 ms"，接收端水位呈锯齿。这里把发送时刻钉在 10 ms 网格上：
        // 不是攒队列（最多只压 1 帧），而是把成簇的输出摊平。
        let frame_gap = Duration::from_millis(FRAME_MS as u64);
        let mut next_send_at = Instant::now();

        // 高精度等待（详见 `hires.rs`）：毫秒级 `WaitForSingleObject` / `Sleep`
        // 会被 Windows 默认 15.6 ms 的定时器 tick 量化，“睡 10 ms”往往变成
        // 15.6 ms，10 ms 发送网格因此被打成“两包挨着 + 一段空”，接收端水位
        // 呈锯齿、阶跃吸收被动触发。拿不到高精度定时器就退回原行为（功能不变）。
        let hi_timer = HiResTimer::new();
        match hi_timer.as_ref() {
            Some(_) => logf!("[Reverse] 发送节拍使用高精度定时器"),
            None => logf!("[Reverse] 高精度定时器不可用，发送节拍退回毫秒级等待"),
        }

        let mut cfg_check_at = Instant::now();
        while running.load(Ordering::Relaxed) {
            // 没有可用采集客户端时，按 500 ms 退避重建；初始化失败不能直接退出线程。
            if capture.is_none()
                && last_capture_attempt
                    .map(|t| t.elapsed() >= Duration::from_millis(500))
                    .unwrap_or(true)
            {
                last_capture_attempt = Some(Instant::now());
                match LoopbackCapture::new() {
                    Some(c) => {
                        let rate = c.sample_rate();
                        resampler = Some(StereoResampler::new(rate, 48_000));
                        capture = Some(c);
                        read_errors = 0;
                        logf!("[Reverse] 环回捕获已就绪（{} Hz）", rate);
                    }
                    None => {
                        if last_capture_fail_log.elapsed() >= Duration::from_secs(5) {
                            last_capture_fail_log = Instant::now();
                            logf!("[Reverse] 环回捕获初始化失败，500 ms 后重试");
                        }
                    }
                }
            }

            // ── 配置变化检测：500 ms 一次，避免每帧抢锁（发送线程不该有锁流量）──
            if cfg_check_at.elapsed() > Duration::from_millis(500) {
                cfg_check_at = Instant::now();
                let cfg = reverse_config().lock().unwrap_or_else(|e| e.into_inner());
                if *cfg != applied {
                    apply_config(&mut encoder, &cfg);
                    applied = *cfg;
                }
            }

            // 有帧在等发送窗口时用**高精度定时器**睡到下一个发送时刻；
            // 没有待发帧（端点空闲）时才等 WASAPI 事件——空闲路径上的等待
            // 精度无关紧要，没必要唤醒系统定时器。
            let has_frame = pcm_buf.len() >= FRAME_INTERLEAVED;
            let wait_ms = if has_frame {
                let now = Instant::now();
                if next_send_at > now {
                    (next_send_at - now)
                        .as_millis()
                        .clamp(1, WAIT_TIMEOUT_MS as u128) as u32
                } else {
                    0
                }
            } else {
                WAIT_TIMEOUT_MS
            };
            let t_wait = Instant::now();
            match (has_frame, hi_timer.as_ref()) {
                (true, Some(timer)) if next_send_at > t_wait => {
                    timer.wait_until(next_send_at);
                }
                _ => {
                    // 已到点（无需等待）或没有高精度定时器：退回事件等待。
                    // 事件是否触发只影响延迟；发送仍按 10 ms 网格整形。
                    let _ = if let Some(c) = capture.as_ref() {
                        c.wait(wait_ms)
                    } else {
                        std::thread::sleep(Duration::from_millis(wait_ms.max(5) as u64));
                        false
                    };
                }
            }
            REVERSE_STATS
                .wait_us
                .fetch_add(t_wait.elapsed().as_micros() as u64, Ordering::Relaxed);
            let t_read = Instant::now();
            let outcome = if let Some(c) = capture.as_ref() {
                c.read_stereo(&mut loopback)
            } else {
                ReadOutcome::Empty
            };
            REVERSE_STATS
                .read_us
                .fetch_add(t_read.elapsed().as_micros() as u64, Ordering::Relaxed);
            let got_data = match outcome {
                ReadOutcome::Data => {
                    read_errors = 0;
                    true
                }
                ReadOutcome::Empty => {
                    read_errors = 0;
                    false
                }
                ReadOutcome::Error(code) => {
                    read_errors = read_errors.saturating_add(1);
                    if read_errors == 1 || read_errors.is_multiple_of(50) {
                        logf!(
                            "[Reverse] 环回 GetBuffer 失败（hr=0x{code:08X}，连续 {read_errors} 次）"
                        );
                    }
                    if read_errors >= 3 {
                        logf!("[Reverse] 环回端点连续失败，重建采集客户端");
                        capture = None;
                        resampler = None;
                        read_errors = 0;
                        last_frame_at = Instant::now();
                        last_capture_attempt = Some(Instant::now());
                    }
                    false
                }
            };

            if got_data {
                // 这段“无真实媒体”的墙钟时间要补进 RTP 时间戳，否则接收端
                // 本地静音已经把播放头推进，恢复包会被判成 late。读取到的
                // 样本本身覆盖一段时间，必须从缺口里扣掉，避免重复计时。
                let idle_ms = last_frame_at.elapsed().as_millis() as u64;
                last_frame_at = Instant::now();
                if let Some(rs) = resampler.as_mut() {
                    rs.process(&loopback, &mut resampled);
                } else {
                    resampled.clear();
                }
                if resampled.len() > MAX_FRAME_BACKLOG {
                    let excess = resampled.len() - MAX_FRAME_BACKLOG;
                    REVERSE_STATS
                        .dropped_samples
                        .fetch_add(excess as u64, Ordering::Relaxed);
                    resampled.drain(..excess);
                    // 丢的是真实媒体：时间戳必须跨过它，否则接收端把断流当连续
                    timestamp = timestamp.wrapping_add((excess / 2) as u32);
                }
                let new_ms = (resampled.len() as u64 / 2) * 1000 / 48_000;
                let gap_ms = idle_ms.saturating_sub(new_ms);
                if gap_ms > (rtp::FRAME_MS as u64 * 2) {
                    timestamp = timestamp.wrapping_add(
                        (gap_ms * rtp::FRAME_SAMPLES as u64 / rtp::FRAME_MS as u64) as u32,
                    );
                }
                for &s in &resampled {
                    let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    pcm_buf.push(v);
                }
            }

            // 积压保护：超过 40 ms 只保留最新部分（发送端不得积压）
            if pcm_buf.len() > MAX_FRAME_BACKLOG {
                let excess = pcm_buf.len() - MAX_FRAME_BACKLOG;
                pcm_buf.drain(..excess);
                REVERSE_STATS
                    .dropped_samples
                    .fetch_add(excess as u64, Ordering::Relaxed);
                // 同上：时间戳跟随真实媒体前进，接收端才能在缺口处 PLC
                timestamp = timestamp.wrapping_add((excess / 2) as u32);
            }
            // 只保留最近 40 ms 的统计峰值；超限时旧数据已经丢掉
            REVERSE_STATS
                .max_backlog
                .fetch_max(pcm_buf.len() as u64, Ordering::Relaxed);

            // 只发到点的帧：节拍整形。槽位落后超过半个帧就重锚（见 `advance_slot`），
            // 绝不用连续突发把落后的时间补回来——那正是水位锯齿的来源。
            while pcm_buf.len() >= FRAME_INTERLEAVED && Instant::now() >= next_send_at {
                let now = Instant::now();
                if slot_is_late(next_send_at, frame_gap, now) {
                    // 网格已被撕开（空闲期无声、线程被抢占、设备重开）：这一帧的
                    // "迟到量"描述的是停顿多久，不是网格多准，混进迟到统计会把
                    // 均值带偏（实测一次 8 s 无声间隔 → 均值 0.3 ms 变 4.2 ms）。
                    // 只计数。真正的卡顿幅度看 `积压峰值` 与下面的重锚次数。
                    REVERSE_STATS.reanchors.fetch_add(1, Ordering::Relaxed);
                } else {
                    // 节拍迟到量：这是判断“网格是否真的准时”的唯一直接证据
                    let late_us = now.duration_since(next_send_at).as_micros() as u64;
                    REVERSE_STATS.late_us.fetch_add(late_us, Ordering::Relaxed);
                    REVERSE_STATS
                        .late_min_us
                        .fetch_min(late_us, Ordering::Relaxed);
                    REVERSE_STATS
                        .late_max_us
                        .fetch_max(late_us, Ordering::Relaxed);
                    REVERSE_STATS.late_n.fetch_add(1, Ordering::Relaxed);
                }
                next_send_at = advance_slot(next_send_at, frame_gap, now);
                frame.copy_from_slice(&pcm_buf[..FRAME_INTERLEAVED]);
                pcm_buf.drain(..FRAME_INTERLEAVED);

                let t_encode = Instant::now();
                let encoded = encoder.encode(&frame, &mut payload);
                REVERSE_STATS
                    .encode_us
                    .fetch_add(t_encode.elapsed().as_micros() as u64, Ordering::Relaxed);
                REVERSE_STATS.frames.fetch_add(1, Ordering::Relaxed);
                // `len == 0` 表示这一帧不产出负载（DTX 空帧 / 编码失败 / 超限）。
                // 时间戳仍按媒体时间前进，但**序列号不占号**：序号只属于真正
                // 发到网络上的包。接收端 PLC/静音同样不推进序号，两边一致。
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
                    let gap_ms = last_sent
                        .map(|t| t.elapsed().as_millis())
                        .unwrap_or(u128::MAX);
                    let ts_start = timestamp.wrapping_sub(rtp::FRAME_SAMPLES);
                    let ts_gap_ms = last_sent_ts
                        .map(|p| {
                            ts_start.wrapping_sub(p) as u64
                                / (rtp::FRAME_SAMPLES as u64 / rtp::FRAME_MS as u64)
                        })
                        .unwrap_or(u64::MAX);
                    let marker = first_packet
                        || gap_ms > rtp::RESYNC_GAP_MS as u128
                        || ts_gap_ms > rtp::RESYNC_GAP_MS as u64;
                    if !first_packet && marker {
                        // 发送中断会让接收端重同步（flush + 重建解码器），必须计数备查
                        REVERSE_STATS.gaps.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Some(total) =
                        rtp::write_packet(&mut packet, marker, seq, ts_start, ssrc, &payload[..len])
                    {
                        let t_send = Instant::now();
                        let sent = send_sock.send_to(&packet[..total], target);
                        REVERSE_STATS
                            .send_us
                            .fetch_add(t_send.elapsed().as_micros() as u64, Ordering::Relaxed);
                        match sent {
                            Ok(_) => {
                                REVERSE_STATS.packets.fetch_add(1, Ordering::Relaxed);
                                REVERSE_STATS
                                    .bytes
                                    .fetch_add(total as u64, Ordering::Relaxed);
                                last_sent = Some(Instant::now());
                                last_sent_ts = Some(ts_start);
                                first_packet = false;
                                // 只有真正发出的包才消耗一个序号
                                seq = seq.wrapping_add(1);
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
            }
        }

        logf!("[Reverse] 反向音频线程退出");
    }
}

// ═══════════════════════════════════════════════════════════════════
// 发送节拍槽位推进
// ═══════════════════════════════════════════════════════════════════

/// 一次发送之后，下一个发送槽位应该取什么时刻。
///
/// 语义借鉴 Sunshine 的“组起点 + n×帧间隔，错过就作废整组”：
///
/// * **未迟到**（`now` 不晚于 `slot + gap/2`）：正常前进一个 `gap`，
///   保持 10 ms 网格，把成簇到货的帧摊平；
/// * **迟到超过半个 gap**：网格已经失真，把相位重锚到 `now + gap`
///   ——下一帧从**这次实际发送时刻**起算一个 `gap`，而不是立刻连发。
///
/// 为什么必须重锚：`[Pacer]` 实测发现只靠“落后 4 帧才重置”是不行的。
/// 线程启动时初始槽位、一次卡顿、一次设备重开，都会把槽位永久推到一个
/// 固定偏移上（实测稳定 15.8 ms ≈ 一个系统 tick）：发送循环本身是 10 ms
/// 周期的，偏移一旦形成就**永久不变**，永远到不了 40 ms 的重置阈值。
/// 后果不只是指标难看——槽位一直过期时，`while now >= slot` 会把积压
/// 的帧连发出去，而这正是 10 ms 网格本要消灭的突发。
///
/// 重锚值必须带 `+ gap`：返回 `now` 会让槽位当场过期，同一个 `while`
/// 里积压的第二帧立刻被发出（回退成连发），且迟到量恒等于一个 `gap`
/// （实测 min/avg = 8945/10007 µs），指标从此失去意义。
fn advance_slot(slot: Instant, gap: Duration, now: Instant) -> Instant {
    if slot_is_late(slot, gap, now) {
        now + gap
    } else {
        slot + gap
    }
}

/// 槽位是否已经"过期到该重锚"：迟到超过半个帧就算。
///
/// 阈值取半个帧的理由：10 ms 网格上 5 ms 的迟到不可能靠下一帧追回来
/// （追就是连发突发），只可能是外部停顿；没超过半个帧的迟到反而是网格
/// 正常抖动的证据，要计入 `late_*` 统计。
fn slot_is_late(slot: Instant, gap: Duration, now: Instant) -> bool {
    now > slot + gap / 2
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
            // SAFETY: 仅当本线程上一次 CoInitializeEx 返回成功时 active 为 true，
            // 因此这里恰好抵消那一次初始化；Drop 只执行一次。
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
        let ratio = if dst_hz == 0 {
            1.0
        } else {
            src_hz as f64 / dst_hz as f64
        };
        Self {
            ratio,
            phase: 0.0,
            in_buf: Vec::with_capacity(4_096),
        }
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

        // 丢弃已经完全消费的整帧，保留 1 帧给下一块做插值。
        //
        // `consumed` 必须钳到 `frames - 1`：循环的退出条件是
        // `floor(phase) >= frames - 1`，而最后一步
        // `phase += ratio` 可以越过 `frames`。ratio > 2（44.1k 以上回环端点，
        // 例如 176.4k / 192k / 384k）时 `consumed` 会等于 `frames + 1..`，
        // `drain(0..consumed * 2)` 的末端下标越过 `in_buf.len()` → panic，
        // 捕获线程直接死掉且反向音频永久静音。钳位同时也保证
        // `phase - consumed >= 0`，不会出现负相位（`as usize` 饱和到 0 后
        // 做负 frac 外插）。
        let consumed = (self.phase as usize).min(frames - 1);
        if consumed > 0 {
            self.in_buf.drain(0..consumed * 2);
            self.phase -= consumed as f64;
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// WASAPI 环回捕获（事件驱动；失效时由外层重建）
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
        // SAFETY: `client` 是本结构独占拥有的 COM 接口指针（Drop 只跑一次，
        // 无其它别名）；`event` 用 take() 取走，保证 CloseHandle 只执行一次。
        // 两个调用都只接受有效句柄，失败返回值被忽略即等于不做事。
        unsafe {
            let _ = self.client.Stop();
            if let Some(h) = self.event.take() {
                let _ = CloseHandle(h);
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ReadOutcome {
    /// 拿到真实（非 SILENT）样本
    Data,
    /// 本次没有新数据
    Empty,
    /// GetBuffer 失败；0x8007xxxx / AUDCLNT_E_* 都在这里
    Error(i32),
}

impl LoopbackCapture {
    fn new() -> Option<Self> {
        // SAFETY: 整块只做 COM 调用，不解引用裸指针；`GetMixFormat` 返回的
        // 指针传给 parse_mix_format 后立刻 CoTaskMemFree，且只释放一次；
        // 失败分支逐个关闭已取得的句柄；事件句柄的最终关闭在 Drop 里。
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

            // 从这里往后不能再提前返回：`event` 是内核句柄，`Self` 没建好就没有
            // Drop 去关它。run_loop 每 500 ms 重试一次，用 `?` 提前返回就是 2 个
            // 句柄/秒的无界泄漏（最终 CreateEventW 失败、环回彻底起不来）。
            let capture: IAudioCaptureClient = match client.GetService() {
                Ok(c) => c,
                Err(e) => {
                    logf!(
                        "[Reverse] GetService(IAudioCaptureClient) 失败: {}",
                        e.code().0
                    );
                    if let Some(h) = event {
                        let _ = CloseHandle(h);
                    }
                    return None;
                }
            };
            if let Err(e) = client.Start() {
                logf!("[Reverse] IAudioClient::Start 失败: {}", e.code().0);
                if let Some(h) = event {
                    let _ = CloseHandle(h);
                }
                return None;
            }
            logf!(
                "[Reverse] WASAPI 环回已启动（{} 声道 / {:?} / {} Hz / {})",
                format.channels,
                format.kind,
                format.sample_rate,
                if event.is_some() {
                    "事件驱动"
                } else {
                    "轮询"
                }
            );
            Some(Self {
                client,
                capture,
                format,
                event,
            })
        }
    }

    fn sample_rate(&self) -> u32 {
        self.format.sample_rate
    }

    /// 等待一次数据就绪。返回 `true` 表示事件已触发，`false` 表示超时。
    fn wait(&self, timeout_ms: u32) -> bool {
        match self.event {
            Some(h) => {
                // SAFETY: `h` 来自 CreateEventW，在本结构存活期内始终有效
                // （只在 Drop 里关闭），且 WaitForSingleObject 不转移所有权。
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
    fn read_stereo(&self, out: &mut Vec<f32>) -> ReadOutcome {
        // SAFETY: `GetBuffer` 成功时保证 `data_ptr` 指向连续 `frames * channels`
        // 个样本的内存，布局与混音格式一致（`self.format`），并且在 `ReleaseBuffer`
        // 之前不被释放；SILENT 标志或空指针时不读。切片长度严格取
        // `frames * channels`，`c0/c1` 的下标都小于 `channels`。
        unsafe {
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;

            if let Err(e) = self.capture.GetBuffer(
                &mut data_ptr as *mut *mut u8,
                &mut frames,
                &mut flags,
                None,
                None,
            ) {
                return ReadOutcome::Error(e.code().0);
            }
            if frames == 0 {
                // WASAPI 契约：每次成功的 GetBuffer 都必须配对 ReleaseBuffer，
                // 即使这次拿到 0 帧（AUDCLNT_S_NO_CURRENT_DATA 也走这里）。
                // 这是本文件唯一曾漏调 ReleaseBuffer 的读取分支。
                let _ = self.capture.ReleaseBuffer(frames);
                return ReadOutcome::Empty;
            }

            let frame_count = frames as usize;
            out.clear();
            out.reserve(frame_count * 2);

            // 静音标志：缓冲内容未定义。按“没有真实媒体”处理，不向网络发送
            // 合成静音；接收端会在本地维持 AudioTrack。
            if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 || data_ptr.is_null() {
                out.clear();
                let _ = self.capture.ReleaseBuffer(frames);
                return ReadOutcome::Empty;
            }

            let channels = self.format.channels;
            let c0 = 0usize;
            let c1 = if channels > 1 { 1 } else { 0 };

            match self.format.kind {
                SampleKind::F32 => {
                    let src =
                        std::slice::from_raw_parts(data_ptr as *const f32, frame_count * channels);
                    for f in 0..frame_count {
                        out.push(src[f * channels + c0]);
                        out.push(src[f * channels + c1]);
                    }
                }
                SampleKind::I16 => {
                    let src =
                        std::slice::from_raw_parts(data_ptr as *const i16, frame_count * channels);
                    for f in 0..frame_count {
                        out.push(src[f * channels + c0] as f32 / 32768.0);
                        out.push(src[f * channels + c1] as f32 / 32768.0);
                    }
                }
                SampleKind::I32 => {
                    let src =
                        std::slice::from_raw_parts(data_ptr as *const i32, frame_count * channels);
                    for f in 0..frame_count {
                        out.push(src[f * channels + c0] as f32 / 2_147_483_648.0);
                        out.push(src[f * channels + c1] as f32 / 2_147_483_648.0);
                    }
                }
            }

            let _ = self.capture.ReleaseBuffer(frames);
            ReadOutcome::Data
        }
    }
}

/// 解析 WAVEFORMATEX / WAVEFORMATEXTENSIBLE
///
/// # Safety
/// `fmt` 必须指向至少 `size_of::<WAVEFORMATEX>()` 字节的有效内存（典型来源是
/// `IAudioClient::GetMixFormat`）。只有 `cbSize >= 22`（WAVEFORMATEXTENSIBLE
/// 比 WAVEFORMATEX 多出的字节数）时才会按扩展结构读取尾部字段。
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

    // 读取侧按 `frame_count * channels` 造切片，即假定每声道每帧占
    // `wBitsPerSample/8` 字节。若驱动的 `nBlockAlign` 与这个假定不一致：
    // 偏小 → 切片长度大于真实步进，越界读相邻内存（UB）；
    // 偏大 → 把下一帧的字节当样本，输出夹带垃圾。
    // 环回端点用的是共享模式的混音格式，正常驱动总是自洽，但格式来自外部，
    // 不能假设。
    let bytes_per_sample = bits as usize / 8;
    if wf.nBlockAlign as usize != channels * bytes_per_sample {
        return None;
    }

    Some(CaptureFormat {
        channels,
        kind,
        sample_rate,
    })
}

// ═══════════════════════════════════════════════════════════════════
// 单元测试：捕获侧重采样
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    const GAP: Duration = Duration::from_millis(10);

    #[test]
    fn slot_is_late_uses_half_a_gap_as_the_boundary() {
        let t0 = Instant::now();
        // 半个 gap 之内算"准时"（计入迟到统计），超过就算网格被撕开
        assert!(!slot_is_late(t0, GAP, t0));
        assert!(!slot_is_late(t0, GAP, t0 + Duration::from_millis(4)));
        assert!(!slot_is_late(t0, GAP, t0 + GAP / 2));
        assert!(slot_is_late(t0, GAP, t0 + Duration::from_millis(6)));
        // 一次哑火几个帧（空闲期无声 / 线程被抢占）必然重锚，且与迟到量无关
        assert!(slot_is_late(t0, GAP, t0 + Duration::from_secs(8)));
    }

    #[test]
    fn advance_slot_keeps_grid_when_on_time() {
        let t0 = Instant::now();
        // 准时发出：下一个槽位正好差一个 gap
        assert_eq!(advance_slot(t0, GAP, t0), t0 + GAP);
        // 晚了一点点（≤ 半个 gap）仍留在网格上
        assert_eq!(
            advance_slot(t0, GAP, t0 + Duration::from_millis(4)),
            t0 + GAP
        );
    }

    #[test]
    fn advance_slot_reanchors_when_late() {
        let t0 = Instant::now();
        // 迟到超过半个 gap：网格已失真，相位重锚到“这次发送 + 一个 gap”
        let late = t0 + Duration::from_micros(15_788);
        assert_eq!(advance_slot(t0, GAP, late), late + GAP);
        // 大幅卡顿（设备重开/调度停滞）同样重锚，不积攒补发
        let stalled = t0 + Duration::from_millis(546);
        assert_eq!(advance_slot(t0, GAP, stalled), stalled + GAP);
    }

    #[test]
    fn advance_slot_reanchor_does_not_send_queued_frames_back_to_back() {
        // 回归：重锚返回 `now` 时槽位当场过期，积压的第二帧会被立刻发出。
        // 重锚必须把下一槽位推到 now + gap，连发才不可能发生。
        let t0 = Instant::now();
        let late = t0 + Duration::from_millis(15);
        let next = advance_slot(t0, GAP, late);
        assert!(
            next >= late + GAP,
            "重锚后下一槽位 {next:?} 早于 {late:?} + {GAP:?}"
        );
    }

    #[test]
    fn advance_slot_never_returns_a_past_slot() {
        // 回归：槽位一旦过期，`while now >= slot` 就会把积压的帧连发出去，
        // 而那正是网格要消灭的突发。新槽位必须不早于 now−半帧。
        let t0 = Instant::now();
        for us in (0..=45_000).step_by(500) {
            let now = t0 + Duration::from_micros(us);
            let next = advance_slot(t0, GAP, now);
            assert!(
                next + GAP / 2 >= now,
                "us={us} 得到过期槽位：{next:?} 早于 {now:?}"
            );
            assert!(next >= now, "us={us} 新槽位 {next:?} 不应早于 now {now:?}");
        }
    }

    #[test]
    fn advance_slot_keeps_even_spacing_after_a_stall() {
        // 卡顿后重锚：槽位 = 本次发送 + 一个 gap，之后回到正常网格
        let t0 = Instant::now();
        let stall = t0 + Duration::from_millis(37);
        let s1 = advance_slot(t0, GAP, stall);
        assert_eq!(s1, stall + GAP);
        // 此刻（stall + 1 ms）下一帧还不到点，积压帧不会被连发
        assert!(stall + Duration::from_millis(1) < s1);
        // 到了 s1 就正常前进一个 gap
        assert_eq!(advance_slot(s1, GAP, s1), s1 + GAP);
    }

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

    /// 回归：高采样率回环端点（ratio > 2）下逐帧步进会越过 `frames`，
    /// 曾经的 `drain(0..consumed * 2)` 直接 panic 掉整个捕获线程。
    /// 覆盖 144k/176.4k/192k/384k 的典型 packet 帧数与相邻的 ±1 帧。
    #[test]
    fn resampler_high_ratio_never_overruns() {
        for &src_hz in &[144_000u32, 176_400, 192_000, 384_000] {
            for &(base, span) in &[(64usize, 32usize), (3_840, 4), (7_680, 4)] {
                for frames in base.saturating_sub(span)..=base + span {
                    let mut rs = StereoResampler::new(src_hz, 48_000);
                    let input = vec![0.25f32; frames * 2];
                    let mut out = Vec::new();
                    rs.process(&input, &mut out);
                    assert_eq!(out.len() % 2, 0, "输出必须是整帧");
                    assert!(
                        rs.phase >= 0.0,
                        "相位不能为负（src={src_hz} frames={frames}）：{}",
                        rs.phase
                    );
                    // 输出的帧数不得超过本次可用输入能支撑的量（多留 1 帧余量）
                    let ratio = src_hz as f64 / 48_000.0;
                    let max_out = (frames as f64 / ratio).ceil() as usize + 2;
                    assert!(
                        out.len() / 2 <= max_out,
                        "src={src_hz} frames={frames} out={} max={max_out}",
                        out.len() / 2
                    );
                }
            }
        }
    }

    #[test]
    fn parse_mix_format_rejects_inconsistent_block_align() {
        // 读取侧按 frame_count * channels 造切片，nBlockAlign 必须与之相符。
        let base = WAVEFORMATEX {
            wFormatTag: 3,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            wBitsPerSample: 32,
            nBlockAlign: 8,
            nAvgBytesPerSec: 48_000 * 8,
            cbSize: 0,
        };
        // SAFETY: 栈上 WAVEFORMATEX，指针有效且生命周期覆盖调用。
        let ok = unsafe { parse_mix_format(&base as *const WAVEFORMATEX) };
        assert!(ok.is_some(), "自洽的 2ch/f32 应被接受");

        let mut small = base;
        small.nBlockAlign = 4; // 小于 2ch * 4B → 会越界读
                               // SAFETY: 同上。
        let small = unsafe { parse_mix_format(&small as *const WAVEFORMATEX) };
        assert!(small.is_none(), "nBlockAlign 偏小必须拒绝");

        let mut large = base;
        large.nBlockAlign = 16; // 大于 2ch * 4B → 会读到相邻帧
                                // SAFETY: 同上。
        let large = unsafe { parse_mix_format(&large as *const WAVEFORMATEX) };
        assert!(large.is_none(), "nBlockAlign 偏大必须拒绝");
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
        // SAFETY: `wf` 是栈上的 WAVEFORMATEX，指针有效且生命周期覆盖调用。
        let fmt =
            unsafe { parse_mix_format(&mut wf as *mut WAVEFORMATEX) }.expect("应支持 f32/2ch");
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

        let p = protocol::ConnectPayload {
            bitrate_kbps: 1024,
            ..protocol::ConnectPayload::default()
        };
        assert_eq!(
            OpusReverseConfig::from(&p).bitrate_kbps,
            512,
            "高于上限必须钳制"
        );
        let p = protocol::ConnectPayload {
            bitrate_kbps: 64,
            ..protocol::ConnectPayload::default()
        };
        assert_eq!(
            OpusReverseConfig::from(&p).bitrate_kbps,
            64,
            "低档位原样透传"
        );
    }
}
