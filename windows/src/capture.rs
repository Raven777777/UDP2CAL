// WASAPI 环回捕获 → Opus（10 ms）→ RTP → UDP
//
// v3 发送端规则（规范第八节）：
//   * 采集 → 编码 → 立即发送，应用层只允许 ≤40 ms 的组帧缓冲，
//     丢弃时时间戳必须跟随真实媒体跳变（接收端据此 PLC/重同步）；
//   * 固定 10 ms 帧，时间戳每帧 +480（采样数）；序列号只对**真正发出的包** +1
//     （DTX / 发送失败不占序号，RFC 3550）；
//   * SSRC 由控制面生成并随 CONNECT_ACK 下发，本模块只使用；
//   * 反向 Opus 配置来自 CONNECT，**只在变化时**应用（每秒重配会重置编码器状态）。
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audiopus::{Application, Bitrate, Channels, SampleRate};

use crate::hires::HiResTimer;
use crate::logf;
use crate::power::MmcssGuard;
use crate::protocol;
use crate::protocol::resampler::Resampler;
use crate::protocol::rtp;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST, WaitForSingleObject,
};

/// DSCP EF（46）→ IP TOS 字节 0xB8。反向音频流的转发优先级最高，
/// IPv4 可用时让 Wi-Fi 接入点 / 路由器优先转发，抑制排队抖动；IPv6 在 Windows 上不设置 TCLASS。
const DSCP_EF_TOS: u32 = 0xB8;

/// 每声道 480 采样 = 10 ms @ 48 kHz
const FRAME_MONO: usize = 480;
/// 48 kHz 采样率下 1 ms 的样本数，仅用于把 lookahead 样本数换成毫秒打印
const OPUS_SAMPLE_RATE_PER_MS: u32 = 48;
/// 立体声交错样本数（一帧）
const FRAME_INTERLEAVED: usize = FRAME_MONO * 2;
/// 组帧缓冲硬上限：4 帧（40 ms）。超出的旧数据直接丢弃，绝不无限积压。
const MAX_FRAME_BACKLOG: usize = FRAME_INTERLEAVED * 4;
/// 发送侧允许的流水线深度（帧）：当前时隙 1 帧 + 下一时隙 1 帧。
/// 超过它的帧是迟到积压，10 ms 节拍永远追不上，必须在入队时丢弃。
const PCM_MAX_QUEUED_FRAMES: usize = 2;
// 端点空闲时**不再**由发送端合成静音：接收端在本机用静音维持
// AudioTrack，RTP 只在有真实媒体时发送。这样端点失效不会被静音掩盖。
/// 帧长（毫秒）：发送节拍与空闲时间戳补偿的基准
const FRAME_MS: u32 = rtp::FRAME_MS;
/// WASAPI 事件等待上限：超时仅用于轮询/重建判断，不用于播放节拍
const WAIT_TIMEOUT_MS: u32 = 10;
/// UDP 发送超时：保证 `send_to` 不会无限阻塞 10 ms 发送节拍与 stop() 的 join
const SEND_TIMEOUT_MS: u64 = 20;

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

impl Default for ReverseStats {
    fn default() -> Self {
        Self {
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
            // 哨兵：`fetch_min` 的初始值必须大于任何真实样本
            late_min_us: AtomicU64::new(u64::MAX),
            late_max_us: AtomicU64::new(0),
            late_n: AtomicU64::new(0),
            reanchors: AtomicU64::new(0),
        }
    }
}

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

impl ReverseStats {
    /// 读取并清零（30 s 汇总用，调用方为 UI 线程）
    fn take(&self) -> ReverseSnapshot {
        let s = ReverseSnapshot {
            packets: self.packets.swap(0, Ordering::Relaxed),
            bytes: self.bytes.swap(0, Ordering::Relaxed),
            encode_errors: self.encode_errors.swap(0, Ordering::Relaxed),
            oversize: self.oversize.swap(0, Ordering::Relaxed),
            dropped_samples: self.dropped_samples.swap(0, Ordering::Relaxed),
            send_errors: self.send_errors.swap(0, Ordering::Relaxed),
            gaps: self.gaps.swap(0, Ordering::Relaxed),
            wait_us: self.wait_us.swap(0, Ordering::Relaxed),
            read_us: self.read_us.swap(0, Ordering::Relaxed),
            encode_us: self.encode_us.swap(0, Ordering::Relaxed),
            send_us: self.send_us.swap(0, Ordering::Relaxed),
            frames: self.frames.swap(0, Ordering::Relaxed),
            max_backlog: self.max_backlog.swap(0, Ordering::Relaxed),
            late_us: self.late_us.swap(0, Ordering::Relaxed),
            late_min_us: self.late_min_us.swap(u64::MAX, Ordering::Relaxed),
            late_max_us: self.late_max_us.swap(0, Ordering::Relaxed),
            late_n: self.late_n.swap(0, Ordering::Relaxed),
            reanchors: self.reanchors.swap(0, Ordering::Relaxed),
        };
        // 窗口内没有任何发送时 min 仍是哨兵值，对外一律报 0
        let late_min_us = if s.late_n == 0 { 0 } else { s.late_min_us };
        ReverseSnapshot { late_min_us, ..s }
    }
}

/// 反向发送链路的会话级共享状态：控制面写配置，发送线程读配置、写统计。
///
/// 历史实现是两份进程静态（`REVERSE_CFG` / `REVERSE_STATS`），测试无法隔离、
/// 也无法同时存在两个会话实例。现在由 [`Session`](crate::session::Session)
/// 持有并传给 `ReverseSender`，锁序见 session 模块头部。
pub struct ReverseShared {
    config: Mutex<OpusReverseConfig>,
    stats: ReverseStats,
}

impl ReverseShared {
    pub fn new() -> Self {
        Self {
            config: Mutex::new(OpusReverseConfig::default()),
            stats: ReverseStats::default(),
        }
    }

    /// 由 session 控制面调用；只有内容真正变化才落盘日志
    pub fn update_config(&self, cfg: OpusReverseConfig) {
        // 中毒恢复与其他 Mutex 一致：这里只是一份配置快照，没有跨字段不变量。
        //
        // 锁内只做比较 + 赋值：`logf!` 是同步文件 I/O，而反向发送线程每 500 ms
        // 要抢同一把锁（capture.rs 的 run_loop）。在锁内落盘意味着磁盘慢时
        // 会把发送线程一起拖住，反向 RTP 断流。
        let changed = {
            let mut c = self.config.lock().unwrap_or_else(|e| e.into_inner());
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

    fn config(&self) -> OpusReverseConfig {
        *self.config.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn take_stats(&self) -> ReverseSnapshot {
        self.stats.take()
    }
}

impl Default for ReverseShared {
    fn default() -> Self {
        Self::new()
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
    pub fn start(target: SocketAddr, ssrc: u32, shared: Arc<ReverseShared>) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let alive = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let alive_clone = alive.clone();

        let handle = std::thread::Builder::new()
            .name("reverse-capture".into())
            .spawn(move || {
                // 发送线程的节拍是 10 ms 网格：被后台线程抢占一次就表现为
                // 一个 ≥10 ms 的发送空档，接收端对应一个 PLC 帧（可闻）。
                // 优先注册 MMCSS "Pro Audio" 获取音频调度配额；失败才回退到
                // THREAD_PRIORITY_HIGHEST（受限权限/组策略只告警，继续运行）。
                let mmcss = MmcssGuard::acquire("Reverse");
                if !mmcss.is_active() {
                    // SAFETY: GetCurrentThread 返回伪句柄，不转移所有权、无需关闭。
                    unsafe {
                        if let Err(e) =
                            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST)
                        {
                            logf!("[Reverse] 发送线程提权失败（{e}），继续用默认优先级");
                        }
                    }
                }
                Self::run_loop(target, ssrc, &running_clone, &alive_clone, &shared);
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
    /// 环回端点，正是“重启后无声”的竞态来源。进入发送循环后，反向线程的
    /// 等待上限是 `WAIT_TIMEOUT_MS`（10 ms）+ `SEND_TIMEOUT_MS`（20 ms）；
    /// 若 stop 恰逢线程启动阶段（设备/Opus 初始化），join 需等初始化结束，
    /// 但不会无限阻塞。
    pub fn stop(mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
        self.alive.store(false, Ordering::Release);
    }

    fn run_loop(
        target: SocketAddr,
        ssrc: u32,
        running: &AtomicBool,
        alive: &AtomicBool,
        shared: &ReverseShared,
    ) {
        let stats = &shared.stats;
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
        let mut applied = shared.config();
        apply_config(&mut encoder, &applied);

        // Opus 固有前瞻：它无法关掉，也不算在网络/抖动缓冲里，但会真实
        // 体现在端到端延迟上（目标水位只算抖动缓冲 + 播放缓冲）。启动打一次
        // 就够，值是编码器参数的函数，不随码率变。
        match encoder.lookahead() {
            Ok(samples) => logf!(
                "[Reverse] Opus lookahead={} 样本（≈{}ms）：这部分延迟不受水位控制",
                samples,
                samples / OPUS_SAMPLE_RATE_PER_MS
            ),
            Err(e) => logf!("[Reverse] 读取 Opus lookahead 失败: {e:?}"),
        }

        let send_sock = match bind_reverse_socket(target) {
            Ok(s) => s,
            Err(e) => {
                logf!("[Reverse] UDP socket 创建失败: {e}");
                return;
            }
        };

        // 发送超时：网络栈/发送缓冲卡住时 `send_to` 最多阻塞 20 ms，
        // 既避免拖垮 10 ms 发送节拍，也保证 stop() 的 join 有界（10 + 20 ms）。
        // 超时按发送失败计数，下一帧继续。
        if let Err(e) = send_sock.set_write_timeout(Some(Duration::from_millis(SEND_TIMEOUT_MS))) {
            logf!("[Reverse] 发送超时设置失败（{e}），继续使用阻塞发送");
        }

        // ── DSCP EF 标记（A2）──
        // 设置本身可能被组策略拦下（DisableUserTOSSetting=1）或需要管理员，
        // 失败不影响功能：仅告警，继续用默认优先级发送。
        if target.is_ipv4() {
            match socket2::SockRef::from(&send_sock).set_tos_v4(DSCP_EF_TOS) {
                Ok(()) => logf!("[Reverse] 发送套接字已标记 DSCP EF（TOS=0xB8）"),
                Err(e) => logf!(
                    "[Reverse] DSCP 标记失败（{e}）：需管理员权限或 DisableUserTOSSetting=0；音频仍可正常发送"
                ),
            }
        }
        // socket2 在 Windows 不提供 IPv6 的 set_tclass_v6；不对 IPv6 套接字设置 IPv4 TOS。

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
        // 端点混音率不是 48 kHz 时先归一再组帧（编码器固定 48 kHz）。
        // 重采样器与播放侧、Android 共用 `protocol/resampler.rs`。
        let mut resampler: Option<Resampler> = None;
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
        // 缺口补偿的待应用量：`timestamp` 只在发送时赋给队首帧，若在追加
        // 新数据时立即推进，会把仍在排队的旧帧一起前移。每个补偿记账为
        // “再消费对应帧数后应用”，补偿才落在旧内容之后、新内容之前。
        // 用队列而非单值：连续缺口各自保留边界，前一次补偿不会被后一次
        // 合并推到新数据之后。
        let mut pending_ts_advances = PendingTsAdvances::default();

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
                        let mut rs = Resampler::new(2);
                        rs.set_source_rate(rate, 48_000);
                        resampler = Some(rs);
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
                let cfg = shared.config();
                if cfg != applied {
                    apply_config(&mut encoder, &cfg);
                    applied = cfg;
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
            stats
                .wait_us
                .fetch_add(t_wait.elapsed().as_micros() as u64, Ordering::Relaxed);
            let t_read = Instant::now();
            let outcome = if let Some(c) = capture.as_ref() {
                c.read_stereo(&mut loopback)
            } else {
                ReadOutcome::Empty
            };
            stats
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
                    rs.process_into(&loopback, &mut resampled);
                } else {
                    resampled.clear();
                }
                // 缺口按**丢弃前**的完整覆盖时长计算：下面丢弃的原始媒体
                // 会作为独立补偿显式记账，若 covered 只算保留部分，同一段
                // 丢弃媒体会被 gap_advance 再计一次。
                let covered_ms = (resampled.len() as u64 / 2) * 1000 / 48_000;
                let mut dropped_advance: u32 = 0;
                if resampled.len() > MAX_FRAME_BACKLOG {
                    let excess = resampled.len() - MAX_FRAME_BACKLOG;
                    stats
                        .dropped_samples
                        .fetch_add(excess as u64, Ordering::Relaxed);
                    resampled.drain(..excess);
                    // 丢的是真实媒体：时间戳必须跨过它，否则接收端把断流当
                    // 连续。但它位于已排队的旧帧之后，立即推进会把旧帧一起
                    // 前移；并入下面的队列补偿，随旧帧消费到边界时兑现。
                    dropped_advance = frame_grid_advance(excess);
                }
                // 补偿必须落在“已入队的旧帧之后、新数据之前”：此时全局
                // timestamp 描述的是队首帧，立即推进会把旧帧一起前移。
                // 边界与 drain 路径同样用 div_ceil：不足一帧的队尾残留也
                // 算占用一个帧边界，避免两侧对残留的折算口径不一致。
                pending_ts_advances.queue(
                    pcm_buf.len().div_ceil(FRAME_INTERLEAVED),
                    gap_advance_samples(idle_ms, covered_ms).wrapping_add(dropped_advance),
                    &mut timestamp,
                );
                for &s in &resampled {
                    let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    pcm_buf.push(v);
                }
            }

            // 积压保护：超过 40 ms 只保留最新部分（发送端不得积压）
            if pcm_buf.len() > MAX_FRAME_BACKLOG {
                let excess = pcm_buf.len() - MAX_FRAME_BACKLOG;
                pcm_buf.drain(..excess);
                stats
                    .dropped_samples
                    .fetch_add(excess as u64, Ordering::Relaxed);
                // 同上：时间戳跟随真实媒体前进，接收端才能在缺口处 PLC。
                // 推进量取整到整帧，保持 480 网格（见 `frame_grid_advance`）。
                timestamp = timestamp.wrapping_add(frame_grid_advance(excess));
                pending_ts_advances.after_drain(excess, &mut timestamp);
            }
            // 只保留最近 40 ms 的统计峰值；超限时旧数据已经丢掉
            stats
                .max_backlog
                .fetch_max(pcm_buf.len() as u64, Ordering::Relaxed);

            // 高精度定时器路径不消费 WASAPI 事件：一次读到 2 个周期或线程被
            // 抢占后，组帧缓冲会多出整帧。发送严格 1 帧/10 ms，超过流水线
            // 深度的积压永远等不到节拍，只会永久抬高发送端延迟（40 ms 上限
            // 只截不断）。丢弃最旧的超额帧，保留最新 1-2 帧；时间戳/序号不动。
            let excess = backlog_trim(pcm_buf.len(), PCM_MAX_QUEUED_FRAMES);
            if excess > 0 {
                pcm_buf.drain(..excess);
                stats
                    .dropped_samples
                    .fetch_add(excess as u64, Ordering::Relaxed);
                pending_ts_advances.after_drain(excess, &mut timestamp);
            }

            // 只发到点的帧：节拍整形。槽位落后超过半个帧就重锚（见 `advance_slot`），
            // 绝不用连续突发把落后的时间补回来——那正是水位锯齿的来源。
            while pcm_buf.len() >= FRAME_INTERLEAVED && Instant::now() >= next_send_at {
                let now = Instant::now();
                if slot_is_late(next_send_at, frame_gap, now) {
                    // 网格已被撕开（空闲期无声、线程被抢占、设备重开）：这一帧的
                    // "迟到量"描述的是停顿多久，不是网格多准，混进迟到统计会把
                    // 均值带偏（实测一次 8 s 无声间隔 → 均值 0.3 ms 变 4.2 ms）。
                    // 只计数。真正的卡顿幅度看 `积压峰值` 与下面的重锚次数。
                    stats.reanchors.fetch_add(1, Ordering::Relaxed);
                } else {
                    // 节拍迟到量：这是判断“网格是否真的准时”的唯一直接证据
                    let late_us = now.duration_since(next_send_at).as_micros() as u64;
                    stats.late_us.fetch_add(late_us, Ordering::Relaxed);
                    stats.late_min_us.fetch_min(late_us, Ordering::Relaxed);
                    stats.late_max_us.fetch_max(late_us, Ordering::Relaxed);
                    stats.late_n.fetch_add(1, Ordering::Relaxed);
                }
                next_send_at = advance_slot(next_send_at, frame_gap, now);
                frame.copy_from_slice(&pcm_buf[..FRAME_INTERLEAVED]);
                pcm_buf.drain(..FRAME_INTERLEAVED);

                let t_encode = Instant::now();
                let encoded = encoder.encode(&frame, &mut payload);
                stats
                    .encode_us
                    .fetch_add(t_encode.elapsed().as_micros() as u64, Ordering::Relaxed);
                stats.frames.fetch_add(1, Ordering::Relaxed);
                // `len == 0` 表示这一帧不产出负载（DTX 空帧 / 编码失败 / 超限）。
                // 时间戳仍按媒体时间前进，但**序列号不占号**：序号只属于真正
                // 发到网络上的包。接收端 PLC/静音同样不推进序号，两边一致。
                let len = match encoded {
                    Ok(n) if n > 0 && n <= rtp::OPUS_MAX_PAYLOAD => n,
                    Ok(n) if n > rtp::OPUS_MAX_PAYLOAD => {
                        // 超过 1275 字节：整帧丢弃，绝不截断
                        stats.oversize.fetch_add(1, Ordering::Relaxed);
                        0
                    }
                    Ok(_) => 0, // DTX 空帧：时间戳推进，不发包
                    Err(e) => {
                        let n = stats.encode_errors.fetch_add(1, Ordering::Relaxed);
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
                        stats.gaps.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Some(total) =
                        rtp::write_packet(&mut packet, marker, seq, ts_start, ssrc, &payload[..len])
                    {
                        let t_send = Instant::now();
                        let sent = send_sock.send_to(&packet[..total], target);
                        stats
                            .send_us
                            .fetch_add(t_send.elapsed().as_micros() as u64, Ordering::Relaxed);
                        match sent {
                            Ok(_) => {
                                stats.packets.fetch_add(1, Ordering::Relaxed);
                                stats.bytes.fetch_add(total as u64, Ordering::Relaxed);
                                last_sent = Some(Instant::now());
                                last_sent_ts = Some(ts_start);
                                first_packet = false;
                                // 只有真正发出的包才消耗一个序号
                                seq = seq.wrapping_add(1);
                            }
                            Err(e) => {
                                let n = stats.send_errors.fetch_add(1, Ordering::Relaxed);
                                if n == 0 {
                                    if matches!(
                                        e.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::TimedOut
                                    ) {
                                        logf!(
                                            "[Reverse] 发送超时（{} ms），已计入发送错误（后续同类错误不再打印）",
                                            SEND_TIMEOUT_MS
                                        );
                                    } else {
                                        logf!("[Reverse] 发送失败（后续同类错误不再打印）: {e}");
                                    }
                                }
                            }
                        }
                    }
                }
                // 缺口补偿在本帧的 ts_start 已经使用之后才兑现：它是紧跟在
                // 队首旧帧后面的静默间隔，下一帧（新数据）才落在缺口之后。
                pending_ts_advances.after_frame_sent(&mut timestamp);
            }
        }

        logf!("[Reverse] 反向音频线程退出");
    }
}

// 反向目标可来自 IPv4 或 IPv6 的监听端；源套接字必须使用同一地址族。
fn bind_reverse_socket(target: SocketAddr) -> std::io::Result<UdpSocket> {
    if target.is_ipv4() {
        UdpSocket::bind("0.0.0.0:0")
    } else {
        UdpSocket::bind("[::]:0")
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

/// 返回 `pcm_buf` 需要从队首丢弃的交错样本数，使队列不超过 `max_frames` 帧。
///
/// 只做数量决策，不碰时间戳/序号：调用方负责 drain 并计入丢样统计。
fn backlog_trim(len: usize, max_frames: usize) -> usize {
    len.saturating_sub(max_frames * FRAME_INTERLEAVED)
}

/// 槽位是否已经"过期到该重锚"：迟到超过半个帧就算。
///
/// 阈值取半个帧的理由：10 ms 网格上 5 ms 的迟到不可能靠下一帧追回来
/// （追就是连发突发），只可能是外部停顿；没超过半个帧的迟到反而是网格
/// 正常抖动的证据，要计入 `late_*` 统计。
fn slot_is_late(slot: Instant, gap: Duration, now: Instant) -> bool {
    now > slot + gap / 2
}

/// 环回断流补偿：只有超过 2 帧的缺口才推进 RTP 时间戳。
/// 读到的样本已覆盖一段时间（`covered_ms`），必须从墙钟缺口里扣掉，
/// 否则正常抖动会被重复计时。返回值取整到整帧（10 ms = 480）。
///
/// 接收端用 `ext_ts % 480 != next_ts % 480` 判定时间线断裂并全量重同步。
/// 墙钟毫秒受事件唤醒抖动与 `as_millis()` 截断影响，通常不是 10 ms 的
/// 整数倍；若直接按 `gap_ms * 48` 换算，恢复包会偏离 480 网格，20–200 ms
/// 的普通静默恢复（提示音、换歌、暂停）每次都会触发一次可闻爆音。
fn gap_advance_samples(idle_ms: u64, covered_ms: u64) -> u32 {
    let gap_ms = idle_ms.saturating_sub(covered_ms);
    if gap_ms > u64::from(rtp::FRAME_MS) * 2 {
        let frame_ms = u64::from(rtp::FRAME_MS);
        // 四舍五入到最近的整帧，保持时间戳余数不变。
        let frames = (gap_ms + frame_ms / 2) / frame_ms;
        (frames * u64::from(rtp::FRAME_SAMPLES)) as u32
    } else {
        0
    }
}

/// 丢弃媒体对应的时间戳推进：交错样本数 → 四舍五入到整帧。
///
/// 接收端用 `ext_ts % 480 != next_ts % 480` 判定时间线断裂并全量重同步。
/// 丢弃量（重采样预入队超额、`pcm_buf` 超额 drain）通常不是整帧；若直接按
/// `samples / 2` 推进，恢复包会落在 480 网格之外，每丢一次就触发一次可闻
/// 爆音。四舍五入到最近整帧，保持时间戳余数不变；不足半帧的丢弃不补偿
/// （与 `gap_advance_samples` 的两帧阈值同样属于正常抖动范畴）。
fn frame_grid_advance(interleaved_samples: usize) -> u32 {
    let mono = interleaved_samples / 2;
    let frames = (mono + FRAME_MONO / 2) / FRAME_MONO;
    (frames as u32) * rtp::FRAME_SAMPLES
}

/// 缺口补偿的待兑现队列。
///
/// `timestamp` 描述的是下一帧发送时的时戳；若队列里还有缺口前的旧帧，
/// 立即推进会把它们一起前移，接收端看到的时间跳变就落在旧内容之前。
/// 每个补偿因此记为“再消费 `after` 帧后把 `advance` 计入时间戳”。
///
/// 必须用队列而非单值：两个缺口之间若还有一批新数据尚未发出，合并成
/// “最大边界”会把前一次补偿从旧数据与第一批新数据之间推到第一批之后。
/// 边界随队列前移逐帧递减，兑现顺序与缺口发生顺序一致。
#[derive(Default)]
struct PendingTsAdvances {
    /// `(after_frames, advance)`，按 `after_frames` 递增；项数受队列深度约束。
    entries: std::collections::VecDeque<(usize, u32)>,
}

impl PendingTsAdvances {
    /// 新缺口到达。`queued_frames` 是当前 `pcm_buf` 的帧数。
    fn queue(&mut self, queued_frames: usize, advance: u32, timestamp: &mut u32) {
        if advance == 0 {
            return;
        }
        if queued_frames == 0 {
            // 队列已空：没有需要保护的旧帧，含此前未兑现的补偿一并立即推进。
            let total = self
                .entries
                .drain(..)
                .fold(advance, |acc, (_, a)| acc.wrapping_add(a));
            *timestamp = timestamp.wrapping_add(total);
            return;
        }
        // 两个缺口落在同一边界（两次读取之间没有发出帧）时直接合并，
        // 队列长度保持在很小的范围内。
        if let Some(last) = self.entries.back_mut() {
            if last.0 == queued_frames {
                last.1 = last.1.wrapping_add(advance);
                return;
            }
        }
        self.entries.push_back((queued_frames, advance));
    }

    /// 已从队首发出一帧：所有边界前移一帧，兑现落到 0 的项。
    fn after_frame_sent(&mut self, timestamp: &mut u32) {
        for entry in &mut self.entries {
            entry.0 = entry.0.saturating_sub(1);
        }
        while let Some((after, advance)) = self.entries.front().copied() {
            if after > 0 {
                break;
            }
            self.entries.pop_front();
            *timestamp = timestamp.wrapping_add(advance);
        }
    }

    /// 队首被丢弃 `drained_samples` 个交错样本后，修正待应用补偿的位置。
    /// 边界前的帧若已被丢弃，补偿必须立刻兑现，不能等一个不存在的“旧帧”。
    /// 丢弃量按整帧折算；不足一帧的零头也视为跨过了一帧边界。
    fn after_drain(&mut self, drained_samples: usize, timestamp: &mut u32) {
        if drained_samples == 0 {
            return;
        }
        let drained_frames = drained_samples.div_ceil(FRAME_INTERLEAVED);
        for entry in &mut self.entries {
            entry.0 = entry.0.saturating_sub(drained_frames);
        }
        while let Some((after, advance)) = self.entries.front().copied() {
            if after > 0 {
                break;
            }
            self.entries.pop_front();
            *timestamp = timestamp.wrapping_add(advance);
        }
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
            // SAFETY: 仅当本线程上一次 CoInitializeEx 返回成功时 active 为 true，
            // 因此这里恰好抵消那一次初始化；Drop 只执行一次。
            unsafe { CoUninitialize() };
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

/// 环回端点支持的声道数上限（与 `parse_mix_format` 保持一致）。
const MAX_CAPTURE_CHANNELS: usize = 32;

// WAVEFORMATEXTENSIBLE 声道掩码位（KS Speaker 常量）
const SPEAKER_FRONT_LEFT: u32 = 0x1;
const SPEAKER_FRONT_RIGHT: u32 = 0x2;
const SPEAKER_FRONT_CENTER: u32 = 0x4;
const SPEAKER_BACK_LEFT: u32 = 0x10;
const SPEAKER_BACK_RIGHT: u32 = 0x20;
const SPEAKER_FRONT_LEFT_OF_CENTER: u32 = 0x40;
const SPEAKER_FRONT_RIGHT_OF_CENTER: u32 = 0x80;
const SPEAKER_SIDE_LEFT: u32 = 0x200;
const SPEAKER_SIDE_RIGHT: u32 = 0x400;

/// 多声道 → 立体声下混权重（按输入声道索引）。
/// 系数取 0.707（-3 dB），LFE 不混入，避免低频能量叠加削波。
#[derive(Clone, Copy, Debug)]
struct Downmix {
    left: [f32; MAX_CAPTURE_CHANNELS],
    right: [f32; MAX_CAPTURE_CHANNELS],
}

/// 没有可靠 channelMask 时，按常见声道数推测标准顺序。
fn default_channel_mask(channels: usize) -> u32 {
    match channels {
        3 => 0x7,   // FL FR FC
        4 => 0x33,  // FL FR BL BR
        5 => 0x37,  // FL FR FC BL BR
        6 => 0x3F,  // 5.1
        7 => 0x70F, // FL FR FC LFE SL SR BC
        8 => 0x63F, // 7.1
        _ if channels >= MAX_CAPTURE_CHANNELS => u32::MAX,
        _ => (1u32 << channels) - 1,
    }
}

/// 按 channelMask 生成下混权重：
/// `L' = L + 0.707*C + 0.707*Ls + 0.707*Lc`，右声道对称；LFE 不混入。
///
/// WAVEFORMATEXTENSIBLE 的交错顺序 = mask 中置位从低到高的顺序。mask 缺失
/// 或与声道数不符时退回 `default_channel_mask`，至少保证中央/环绕声道不丢。
fn build_downmix(channels: usize, channel_mask: u32) -> Option<Downmix> {
    if channels <= 2 {
        return None;
    }
    let mask = if channel_mask != 0 && channel_mask.count_ones() as usize == channels {
        channel_mask
    } else {
        default_channel_mask(channels)
    };

    // 声道索引 → 扬声器位
    let mut speaker_bits = [0u32; MAX_CAPTURE_CHANNELS];
    let mut n = 0;
    for bit in 0..MAX_CAPTURE_CHANNELS {
        let speaker = 1u32 << bit;
        if mask & speaker != 0 && n < channels {
            speaker_bits[n] = speaker;
            n += 1;
        }
    }

    let mut downmix = Downmix {
        left: [0.0; MAX_CAPTURE_CHANNELS],
        right: [0.0; MAX_CAPTURE_CHANNELS],
    };
    let mut add = |speaker: u32, l: f32, r: f32| {
        if let Some(ch) = speaker_bits[..n].iter().position(|&s| s == speaker) {
            downmix.left[ch] += l;
            downmix.right[ch] += r;
        }
    };

    let surround = std::f32::consts::FRAC_1_SQRT_2;
    add(SPEAKER_FRONT_LEFT, 1.0, 0.0);
    add(SPEAKER_FRONT_RIGHT, 0.0, 1.0);
    add(SPEAKER_FRONT_CENTER, surround, surround);
    add(SPEAKER_BACK_LEFT, surround, 0.0);
    add(SPEAKER_SIDE_LEFT, surround, 0.0);
    add(SPEAKER_BACK_RIGHT, 0.0, surround);
    add(SPEAKER_SIDE_RIGHT, 0.0, surround);
    add(SPEAKER_FRONT_LEFT_OF_CENTER, surround, 0.0);
    add(SPEAKER_FRONT_RIGHT_OF_CENTER, 0.0, surround);
    // LFE / BACK_CENTER / 顶部声道不参与：低频叠加最容易过载。

    Some(downmix)
}

/// 把一帧多声道样本写成固定立体声：≤2 声道直接取前两路（单声道复制），
/// >2 声道按 `Downmix` 权重求和并限幅。
#[inline]
fn write_stereo<T: Copy>(
    src: &[T],
    frames: usize,
    channels: usize,
    c0: usize,
    c1: usize,
    downmix: Option<&Downmix>,
    out: &mut Vec<f32>,
    to_f32: impl Fn(T) -> f32,
) {
    match downmix {
        Some(dm) => {
            for f in 0..frames {
                let base = f * channels;
                let mut l = 0.0f32;
                let mut r = 0.0f32;
                for ch in 0..channels {
                    let s = to_f32(src[base + ch]);
                    l += s * dm.left[ch];
                    r += s * dm.right[ch];
                }
                out.push(l.clamp(-1.0, 1.0));
                out.push(r.clamp(-1.0, 1.0));
            }
        }
        None => {
            for f in 0..frames {
                let base = f * channels;
                out.push(to_f32(src[base + c0]));
                out.push(to_f32(src[base + c1]));
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CaptureFormat {
    channels: usize,
    kind: SampleKind,
    sample_rate: u32,
    /// channels > 2 时的下混权重；≤2 声道为 None（保持原样直取）。
    downmix: Option<Downmix>,
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
            if let Some(h) = event
                && client.SetEventHandle(h).is_err() {
                    logf!("[Reverse] 环回端点不支持事件回调，改用 10 ms 超时轮询");
                    let _ = CloseHandle(h);
                    event = None;
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
        // `frames * channels`，`c0/c1` 与下混声道索引都小于 `channels`。
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
            let downmix = self.format.downmix.as_ref();

            match self.format.kind {
                SampleKind::F32 => {
                    let src =
                        std::slice::from_raw_parts(data_ptr as *const f32, frame_count * channels);
                    write_stereo(src, frame_count, channels, c0, c1, downmix, out, |s| s);
                }
                SampleKind::I16 => {
                    let src =
                        std::slice::from_raw_parts(data_ptr as *const i16, frame_count * channels);
                    write_stereo(src, frame_count, channels, c0, c1, downmix, out, |s| {
                        s as f32 / 32768.0
                    });
                }
                SampleKind::I32 => {
                    let src =
                        std::slice::from_raw_parts(data_ptr as *const i32, frame_count * channels);
                    write_stereo(src, frame_count, channels, c0, c1, downmix, out, |s| {
                        s as f32 / 2_147_483_648.0
                    });
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
    let wf = unsafe { &*fmt };
    let channels = wf.nChannels as usize;
    if channels == 0 || channels > MAX_CAPTURE_CHANNELS {
        return None;
    }
    let sample_rate = wf.nSamplesPerSec;
    if !(8000..=384_000).contains(&sample_rate) {
        return None;
    }
    let bits = wf.wBitsPerSample;
    let tag = wf.wFormatTag;
    let mut channel_mask = 0u32;

    let is_float = match (tag, wf.cbSize) {
        (3, _) => true,  // WAVE_FORMAT_IEEE_FLOAT
        (1, _) => false, // WAVE_FORMAT_PCM
        (0xFFFE, cb_size) if cb_size >= 22 => {
            let ext = unsafe { &*(fmt as *const WAVEFORMATEXTENSIBLE) };
            // 多声道（5.1/7.1）下混依赖 mask 识别中央/环绕；非扩展格式没有该字段。
            channel_mask = ext.dwChannelMask;
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
        downmix: build_downmix(channels, channel_mask),
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
    fn reverse_socket_matches_ipv4_and_ipv6_destinations() -> std::io::Result<()> {
        for listen_addr in ["127.0.0.1:0", "[::1]:0"] {
            let receiver = UdpSocket::bind(listen_addr)?;
            let target = receiver.local_addr()?;
            let sender = bind_reverse_socket(target)?;
            assert_eq!(sender.local_addr()?.is_ipv4(), target.is_ipv4());
            receiver.set_read_timeout(Some(Duration::from_secs(1)))?;
            sender.send_to(b"rtp", target)?;
            let mut buf = [0u8; 3];
            let (len, source) = receiver.recv_from(&mut buf)?;
            assert_eq!(&buf[..len], b"rtp");
            assert_eq!(source.is_ipv4(), target.is_ipv4());
        }
        Ok(())
    }

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
    fn gap_advance_ignores_normal_jitter_and_counts_long_gaps() {
        // 正常抖动/读取覆盖：不补时间戳，避免重复计时
        assert_eq!(gap_advance_samples(0, 10), 0);
        assert_eq!(gap_advance_samples(20, 0), 0);
        assert_eq!(gap_advance_samples(20, 5), 0);
        // 超过 2 帧才补，且四舍五入到整帧：21 ms -> 2 帧 = 960 采样
        assert_eq!(gap_advance_samples(21, 0), 960);
        assert_eq!(gap_advance_samples(24, 0), 960);
        assert_eq!(gap_advance_samples(25, 0), 1440);
        // 扣掉样本自身覆盖的时间：100 - 40 = 60 ms -> 2880 采样
        assert_eq!(gap_advance_samples(100, 40), 2880);
    }

    #[test]
    fn gap_advance_stays_on_the_480_sample_grid() {
        // 接收端用时间戳余数判定时间线断裂；任何超过 2 帧的墙钟缺口，
        // 补偿量都必须是整帧，否则每次静默恢复都会全量重同步。
        for idle in 21u64..=300 {
            for covered in [0u64, 7, 13, 29] {
                let advance = gap_advance_samples(idle, covered);
                assert_eq!(
                    advance % rtp::FRAME_SAMPLES,
                    0,
                    "idle={idle} covered={covered} -> {advance}"
                );
            }
        }
    }

    #[test]
    fn dropped_media_advance_stays_on_the_480_grid() {
        // 合成场景：缺口补偿（已按整帧取整）与丢弃媒体推进叠加后，最终
        // 时间戳推进必须仍是 480 的整数倍，否则接收端会全量重同步。
        for interleaved in [0usize, 1, 239, 240, 479, 480, 959, 960, 961, 1_920, 3_841] {
            let dropped = frame_grid_advance(interleaved);
            assert_eq!(
                dropped % rtp::FRAME_SAMPLES,
                0,
                "interleaved={interleaved} -> {dropped}"
            );
            for idle in [0u64, 5, 21, 47, 300] {
                for covered in [0u64, 7, 13, 29] {
                    let advance = gap_advance_samples(idle, covered).wrapping_add(dropped);
                    assert_eq!(
                        advance % rtp::FRAME_SAMPLES,
                        0,
                        "idle={idle} covered={covered} interleaved={interleaved} -> {advance}"
                    );
                }
            }
        }
        // 取整边界：不足半帧不补偿，半帧及以上落到最近的整帧。
        assert_eq!(frame_grid_advance(479), 0);
        assert_eq!(frame_grid_advance(480), rtp::FRAME_SAMPLES);
        assert_eq!(frame_grid_advance(959), rtp::FRAME_SAMPLES);
        assert_eq!(frame_grid_advance(1_440), 2 * rtp::FRAME_SAMPLES);
    }

    #[test]
    fn pending_ts_advance_waits_for_queued_frames() {
        let mut ts = 1_000u32;
        let mut pending = PendingTsAdvances::default();

        // 队列非空：补偿记账，不立即推进 timestamp
        pending.queue(2, 960, &mut ts);
        assert_eq!(ts, 1_000);
        assert_eq!(pending.entries.back(), Some(&(2, 960)));

        // 新缺口位于更远的边界：保留两个独立边界，不合并
        pending.queue(3, 480, &mut ts);
        assert_eq!(ts, 1_000);
        assert_eq!(
            pending.entries.iter().copied().collect::<Vec<_>>(),
            vec![(2, 960), (3, 480)]
        );
    }

    #[test]
    fn pending_ts_advance_applies_immediately_when_queue_is_empty() {
        let mut ts = 1_000u32;
        let mut pending = PendingTsAdvances::default();
        pending.queue(2, 960, &mut ts);
        // 队列已空：此前未兑现的补偿与新缺口一起立即兑现
        pending.queue(0, 480, &mut ts);
        assert_eq!(ts, 2_440);
        assert!(pending.entries.is_empty());
    }

    #[test]
    fn pending_ts_advance_lands_after_old_frames() {
        // 模拟发送循环：旧帧按 480 连续，补偿在它们全部发出后才进入时间戳。
        let mut ts = 1_000u32;
        let mut pending = PendingTsAdvances::default();
        pending.queue(2, 960, &mut ts);

        let mut starts = Vec::new();
        for _ in 0..3 {
            ts = ts.wrapping_add(rtp::FRAME_SAMPLES);
            starts.push(ts.wrapping_sub(rtp::FRAME_SAMPLES));
            pending.after_frame_sent(&mut ts);
        }
        // 前两帧（旧内容）连续；第三帧（新数据）晚一个缺口 960 采样。
        assert_eq!(starts, vec![1_000, 1_480, 2_920]);
    }

    #[test]
    fn second_gap_keeps_the_first_compensation_boundary() {
        // 回归：两个缺口之间还有一批新数据未发出时，单值记账会把第一个
        // 补偿推到这批数据之后。每个缺口必须挂在各自的队列边界上。
        let mut ts = 1_000u32;
        let mut pending = PendingTsAdvances::default();
        pending.queue(1, 960, &mut ts);
        pending.queue(3, 480, &mut ts);

        let mut starts = Vec::new();
        for _ in 0..4 {
            ts = ts.wrapping_add(rtp::FRAME_SAMPLES);
            starts.push(ts.wrapping_sub(rtp::FRAME_SAMPLES));
            pending.after_frame_sent(&mut ts);
        }
        // 旧帧 1000；第一个缺口 960 之后第一批新数据从 2440 开始；
        // 第一批（2 帧）之后第二个缺口 480 才生效。
        assert_eq!(starts, vec![1_000, 2_440, 2_920, 3_880]);
    }

    #[test]
    fn queued_ts_advance_is_flushed_when_its_frames_are_drained() {
        let mut ts = 1_000u32;
        let mut pending = PendingTsAdvances::default();
        pending.queue(2, 960, &mut ts);
        assert_eq!(pending.entries.back(), Some(&(2, 960)));

        // 丢掉 1 帧：边界仍在，补偿继续等待
        pending.after_drain(FRAME_INTERLEAVED, &mut ts);
        assert_eq!(ts, 1_000);
        assert_eq!(pending.entries.back(), Some(&(1, 960)));

        // 边界前的最后一帧也丢了：补偿必须立刻兑现
        pending.after_drain(FRAME_INTERLEAVED, &mut ts);
        assert_eq!(ts, 1_960);
        assert!(pending.entries.is_empty());
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
    fn backlog_trim_bounds_queue_and_actually_drains_it() {
        // 2 帧是流水线深度（当前时隙 + 下一时隙），不丢
        assert_eq!(backlog_trim(FRAME_INTERLEAVED, 2), 0);
        assert_eq!(backlog_trim(FRAME_INTERLEAVED * 2, 2), 0);
        // 卡顿后攒到 3/4 帧：最旧的超额帧被丢弃，队列真的下降
        assert_eq!(backlog_trim(FRAME_INTERLEAVED * 3, 2), FRAME_INTERLEAVED);
        assert_eq!(
            backlog_trim(FRAME_INTERLEAVED * 4, 2),
            FRAME_INTERLEAVED * 2
        );

        // 模拟连续发送时隙：每轮先到 1 帧，裁剪到流水线深度，再发出 1 帧。
        // 卡顿攒下的积压必须收敛到 1 帧流水线，而不是停在 40 ms 上限。
        let mut queued = FRAME_INTERLEAVED * 4;
        for _ in 0..4 {
            queued += FRAME_INTERLEAVED;
            queued -= backlog_trim(queued, PCM_MAX_QUEUED_FRAMES);
            assert!(
                queued <= PCM_MAX_QUEUED_FRAMES * FRAME_INTERLEAVED,
                "裁剪后不得超过流水线深度"
            );
            queued = queued.saturating_sub(FRAME_INTERLEAVED);
            assert!(
                queued <= FRAME_INTERLEAVED,
                "每轮发出 1 帧后只允许剩 1 帧流水线"
            );
        }
        assert_eq!(queued, FRAME_INTERLEAVED, "积压必须收敛到流水线深度");
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
    fn downmix_51_mixes_center_and_surround_and_ignores_lfe() {
        const S: f32 = std::f32::consts::FRAC_1_SQRT_2;
        let dm = build_downmix(6, 0x3F).expect("5.1 需要下混");
        assert_eq!(dm.left[0], 1.0); // FL
        assert_eq!(dm.right[1], 1.0); // FR
        assert_eq!(dm.left[2], S); // FC
        assert_eq!(dm.right[2], S);
        assert_eq!(dm.left[3], 0.0); // LFE 不混入
        assert_eq!(dm.right[3], 0.0);
        assert_eq!(dm.left[4], S); // BL
        assert_eq!(dm.right[5], S); // BR
    }

    #[test]
    fn downmix_71_uses_mask_order_for_side_surround() {
        const S: f32 = std::f32::consts::FRAC_1_SQRT_2;
        // FL FR FC LFE BL BR SL SR
        let dm = build_downmix(8, 0x63F).expect("7.1 需要下混");
        assert_eq!(dm.left[4], S); // BL
        assert_eq!(dm.right[5], S); // BR
        assert_eq!(dm.left[6], S); // SL
        assert_eq!(dm.right[7], S); // SR
    }

    #[test]
    fn downmix_preserves_stereo_and_mono_paths() {
        assert!(build_downmix(1, 0).is_none());
        assert!(build_downmix(2, 0x3).is_none());
    }

    #[test]
    fn downmix_falls_back_when_mask_is_missing() {
        const S: f32 = std::f32::consts::FRAC_1_SQRT_2;
        // 非扩展格式没有 channelMask：按 5.1 标准顺序，中央声道不能丢
        let dm = build_downmix(6, 0).expect("多声道必须有下混");
        assert_eq!((dm.left[2], dm.right[2]), (S, S));
    }

    #[test]
    fn write_stereo_downmixes_and_clamps() {
        // 3 声道 FL=FR=FC=0.8 -> 0.8 + 0.707*0.8 = 1.365，必须限幅
        let src = [0.8f32, 0.8, 0.8];
        let dm = build_downmix(3, 0x7).expect("3 声道需要下混");
        let mut out = Vec::new();
        write_stereo(&src, 1, 3, 0, 1, Some(&dm), &mut out, |s| s);
        assert_eq!(out, vec![1.0, 1.0]);
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

    #[test]
    fn reverse_stats_are_per_instance_and_use_a_min_sentinel() {
        let a = ReverseShared::new();
        let b = ReverseShared::new();

        // 空窗口对外报 0，而不是 fetch_min 的哨兵值
        let empty = a.take_stats();
        assert_eq!(empty.late_min_us, 0);
        assert_eq!(empty.packets, 0);

        a.stats.packets.fetch_add(3, Ordering::Relaxed);
        a.stats.late_min_us.fetch_min(120, Ordering::Relaxed);
        a.stats.late_n.fetch_add(1, Ordering::Relaxed);

        assert_eq!(b.take_stats().packets, 0, "统计不再共享进程静态");
        let snap = a.take_stats();
        assert_eq!(snap.packets, 3);
        assert_eq!(snap.late_min_us, 120);
        // 读取即清零，下一窗口重新从哨兵开始
        assert_eq!(a.take_stats().late_min_us, 0);
    }
}
