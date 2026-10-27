// UDP2CAL Windows 接收端 - 局域网音频串流
// 双状态机架构 + P2P 独占通信系统
#![windows_subsystem = "windows"]

mod audio;
mod capture;
mod config;
mod decoder;
mod diag;
mod firewall;
mod hires;
mod logger;
mod protocol;

use iced::futures::SinkExt;
use iced::widget::{button, column, container, progress_bar, row, text, text_input, Space};
use iced::{Alignment, Color, Element, Length, Subscription, Task, Theme};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, BOOL, ERROR_ALREADY_EXISTS, HWND, LPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
    MessageBoxW, SetForegroundWindow, ShowWindow, MB_ICONINFORMATION, MB_OK, SW_RESTORE,
};

// ==========================================
// 图标加载 (编译期嵌入 icon.png)
// ==========================================
fn load_icon_rgba() -> (Vec<u8>, u32, u32) {
    static ICON_PNG: &[u8] = include_bytes!("../../icon.png");
    if let Ok(img) = image::load_from_memory(ICON_PNG) {
        let rgba = img.into_rgba8();
        let (w, h) = rgba.dimensions();
        (rgba.into_raw(), w, h)
    } else {
        // 回退：生成 32x32 绿色圆点
        let width = 32u32;
        let height = 32u32;
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let dx = x as f32 - 15.5;
                let dy = y as f32 - 15.5;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist < 14.0 {
                    rgba.extend_from_slice(&[0, 204, 115, 255]);
                } else if dist < 15.0 {
                    rgba.extend_from_slice(&[0, 204, 115, 128]);
                } else {
                    rgba.extend_from_slice(&[0, 0, 0, 0]);
                }
            }
        }
        (rgba, width, height)
    }
}

#[derive(Debug, Clone, Copy)]
enum TrayCommand {
    Restore,
    Quit,
}

static TRAY_COMMANDS: OnceLock<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<TrayCommand>>> =
    OnceLock::new();

// ══════════════════════════════════════════════
// 控制面状态（P2P 独占绑定）
// ══════════════════════════════════════════════

const DEVICE_READY: u8 = 0;
const DEVICE_BUSY: u8 = 1;

/// 绑定关系。
///
/// 历史实现把"状态"和"绑定 ID"放在两个独立全局里分别用 Relaxed 访问，
/// 二者构成的复合不变式没有任何顺序保证，可能读到 "BUSY + 旧 ID"。
/// 现在合并进一把锁，状态与全部绑定字段永远同时可见。
///
/// 正向 RTP 的准入条件是 (源地址, SSRC, PT)，三者在绑定建立时一次性确定；
/// 反向 RTP 由 Windows 发出，只需要 `reverse_ssrc` 与目标端口。
#[derive(Clone, Copy)]
struct P2pState {
    state: u8,
    device_id: [u8; protocol::DEVICE_ID_SIZE],
    /// CONNECT 源地址（Android 主 socket）：正向 RTP 必须来自同一 IP + 端口
    addr: Option<SocketAddr>,
    forward_ssrc: u32,
    reverse_ssrc: u32,
    reverse_port: u16,
    last_connect: Option<Instant>,
}

impl P2pState {
    const fn new() -> Self {
        Self {
            state: DEVICE_READY,
            device_id: [0u8; protocol::DEVICE_ID_SIZE],
            addr: None,
            forward_ssrc: 0,
            reverse_ssrc: 0,
            reverse_port: 0,
            last_connect: None,
        }
    }
    fn is_busy(&self) -> bool {
        self.state == DEVICE_BUSY
    }
}

/// A v3 device ID is public, so an established binding is the exact peer endpoint,
/// not merely the ID. Endpoint migration is reserved for an authenticated protocol.
fn is_bound_peer_source(
    peer: &P2pState,
    device_id: [u8; protocol::DEVICE_ID_SIZE],
    src: SocketAddr,
) -> bool {
    peer.is_busy() && peer.device_id == device_id && peer.addr == Some(src)
}

/// 控制包只有消息类型已知且设备 ID 非全零时才允许进入状态机。
///
/// 设备 ID 不是认证凭据，但协议规范明确把全零定义为无效值；若放行，
/// Busy 状态会建立一个以后永远无法被同一 ID 正常识别的绑定。
fn is_valid_control_header(header: &protocol::ControlHeader) -> bool {
    header.is_known_type() && protocol::is_valid_device_id(&header.device_id)
}

const DISCOVERY_RATE_PER_SOURCE: u32 = 5;
const DISCOVERY_MAX_TRACKED_SOURCES: usize = 1024;

/// RTP 连续空闲超过该值：下一个通过校验的包到达时先 reset 抖动缓冲。
///
/// 与 Android 端 `REVERSE_IDLE_RESET_MS`、`docs/PROTOCOL_V3_RTP.md` §7.12 一致。
/// 不做这一步时，本地 PLC/静音分支会按 10 ms/帧把 `next_ts` 一路推过新包的
/// 时间戳，发送端时间线一旦回退（编码器/采集重启），恢复包永远被判「迟到」，
/// 结果是整条反向音频永久静音、只留下单调增长的 `stats.late`。
const RTP_IDLE_RESET_MS: u64 = 1_000;

/// 发现请求的按源限流，并为流量状态表设置硬上限。
///
/// UDP 源地址可被伪造；只按 30 s 周期清理而不限制表大小，会让持续到达的
/// 不同源地址在清理前无界占用内存。达到上限后拒绝新源，已有的合法源仍按
/// 原令牌桶规则工作，直到下一轮清理回收过期项。
fn allow_discovery_request(
    bucket: &mut std::collections::HashMap<std::net::IpAddr, (u32, Instant)>,
    source: std::net::IpAddr,
    now: Instant,
) -> bool {
    if let Some((count, window_start)) = bucket.get_mut(&source) {
        if now.duration_since(*window_start) >= Duration::from_secs(1) {
            *count = 0;
            *window_start = now;
        }
        if *count >= DISCOVERY_RATE_PER_SOURCE {
            return false;
        }
        *count += 1;
        return true;
    }
    if bucket.len() >= DISCOVERY_MAX_TRACKED_SOURCES {
        return false;
    }
    bucket.insert(source, (1, now));
    true
}

#[cfg(test)]
mod control_binding_tests {
    use super::*;

    #[test]
    fn discovery_limiter_is_bounded_and_resets_per_second() {
        let mut bucket = std::collections::HashMap::new();
        let source = "192.0.2.1".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..DISCOVERY_RATE_PER_SOURCE {
            assert!(allow_discovery_request(&mut bucket, source, t0));
        }
        assert!(!allow_discovery_request(&mut bucket, source, t0));
        assert!(allow_discovery_request(
            &mut bucket,
            source,
            t0 + Duration::from_secs(1)
        ));

        let mut full = std::collections::HashMap::new();
        for i in 0..DISCOVERY_MAX_TRACKED_SOURCES {
            let ip = std::net::IpAddr::V4(std::net::Ipv4Addr::from(i as u32 + 1));
            assert!(allow_discovery_request(&mut full, ip, t0));
        }
        let new_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 1));
        assert!(!allow_discovery_request(&mut full, new_ip, t0));
        assert_eq!(full.len(), DISCOVERY_MAX_TRACKED_SOURCES);
        // 已在表中的源不受新源上限影响，避免一次伪造洪泛把正常设备也锁死。
        assert!(allow_discovery_request(
            &mut full,
            std::net::IpAddr::V4(std::net::Ipv4Addr::from(1)),
            t0 + Duration::from_secs(1)
        ));
    }

    #[test]
    fn control_header_rejects_all_zero_device_id() {
        let valid_id = [1u8; protocol::DEVICE_ID_SIZE];
        let valid = protocol::ControlHeader::new(protocol::TYPE_CONNECT, 15, valid_id);
        assert!(is_valid_control_header(&valid));

        let zero_id = protocol::ControlHeader::new(
            protocol::TYPE_CONNECT,
            15,
            [0u8; protocol::DEVICE_ID_SIZE],
        );
        assert!(!is_valid_control_header(&zero_id));

        let unknown_type = protocol::ControlHeader::new(protocol::TYPE_DATA, 0, valid_id);
        assert!(!is_valid_control_header(&unknown_type));
    }

    #[test]
    fn v3_binding_requires_exact_source_endpoint() {
        let id = [7u8; protocol::DEVICE_ID_SIZE];
        let original: SocketAddr = "192.0.2.10:50000".parse().unwrap();
        let mut peer = P2pState::new();
        peer.state = DEVICE_BUSY;
        peer.device_id = id;
        peer.addr = Some(original);

        assert!(is_bound_peer_source(&peer, id, original));
        assert!(!is_bound_peer_source(
            &peer,
            id,
            "192.0.2.11:50000".parse().unwrap()
        ));
        assert!(!is_bound_peer_source(
            &peer,
            id,
            "192.0.2.10:50001".parse().unwrap()
        ));
        assert!(!is_bound_peer_source(
            &peer,
            [8u8; protocol::DEVICE_ID_SIZE],
            original
        ));
    }
}

static P2P_STATE: OnceLock<Mutex<P2pState>> = OnceLock::new();

/// 非法控制包的日志闸门：控制面与 RTP 共用一个接收任务，攻击者/故障设备
/// 刷非法 CONNECT 时不能让它用文件 I/O 拖慢实时音频接收。每秒最多一条。
fn should_log_control_reject() -> bool {
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    let gate = LAST.get_or_init(|| Mutex::new(None));
    // 中毒恢复与 p2p_lock 一致：闸门里只有 Instant，没有需要保持的不变量；
    // 因中毒而静默不记日志反而会丢掉现场证据。
    let mut last = gate.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if last
        .map(|t| now.duration_since(t) >= Duration::from_secs(1))
        .unwrap_or(true)
    {
        *last = Some(now);
        return true;
    }
    false
}

/// 合法控制事件的日志闸门：同样与实时接收共线程，重连/SSRC 抖动时
/// 不能让同步文件 I/O 每秒刷屏。
fn should_log_control_event() -> bool {
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    let gate = LAST.get_or_init(|| Mutex::new(None));
    let mut last = gate.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if last
        .map(|t| now.duration_since(t) >= Duration::from_secs(1))
        .unwrap_or(true)
    {
        *last = Some(now);
        return true;
    }
    false
}

fn p2p_lock() -> std::sync::MutexGuard<'static, P2pState> {
    P2P_STATE
        .get_or_init(|| Mutex::new(P2pState::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn p2p_snapshot() -> P2pState {
    *p2p_lock()
}

/// UI 是否已启动（停止时禁止广播、禁止接收 RTP）
static APP_RUNNING: AtomicU8 = AtomicU8::new(0);

/// 反向串流用户开关（全局，供控制面读取）
static REVERSE_ENABLED: AtomicU8 = AtomicU8::new(0);

/// 反向发送器重启的**串行化锁**。
///
/// 槽位锁（`reverse_sender()`）只用于短平快地 take/store；`stop()` 里的
/// `join` 必须放在槽位锁之外（否则 keepalive 路径的 `reverse_sender_alive()`
/// 以及 UI 线程会被一串到底）。但“同一时刻只有一个发送线程持有环回端点”这个
/// 不变量需要跨线程串行化（控制面 async 任务 + UI 线程都会重启），
/// 所以单独用一把锁。
static REVERSE_RESTART: Mutex<()> = Mutex::new(());

/// 反向音频发送器句柄
static REVERSE_SENDER: OnceLock<Mutex<Option<crate::capture::ReverseSender>>> = OnceLock::new();
fn reverse_sender() -> &'static Mutex<Option<crate::capture::ReverseSender>> {
    REVERSE_SENDER.get_or_init(|| Mutex::new(None))
}

/// 反向发送线程是否仍存活。槽位为 `Some` 不代表线程还在跑：
/// 环回初始化失败、编码器创建失败、线程 panic 都会让线程退出而槽位不动。
fn reverse_sender_alive() -> bool {
    reverse_sender()
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(|s| s.is_alive()))
        .unwrap_or(false)
}

/// 获取本机设备 ID 字节数组（进程内只解析一次，避免每包读注册表）
fn my_device_id() -> [u8; protocol::DEVICE_ID_SIZE] {
    static ID: OnceLock<[u8; protocol::DEVICE_ID_SIZE]> = OnceLock::new();
    *ID.get_or_init(|| config::Config::load().get_device_id_bytes())
}

/// 获取本机设备名（系统主机名，进程内只读一次）
fn my_device_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| std::env::var("COMPUTERNAME").unwrap_or_else(|_| "UDP2CAL".into()))
        .as_str()
}

// ══════════════════════════════════════════════
// 接收侧：抖动缓冲 + 解码线程
// ══════════════════════════════════════════════

/// 正向媒体流的抖动缓冲。网络线程只插入，解码线程只按播放头拉取。
static JITTER: OnceLock<Mutex<protocol::rtp::JitterBuffer>> = OnceLock::new();
fn jitter() -> &'static Mutex<protocol::rtp::JitterBuffer> {
    JITTER.get_or_init(|| Mutex::new(protocol::rtp::JitterBuffer::new()))
}

/// 解码线程句柄：网络线程与复位路径据此唤醒它（unpark 为原子置位，无分配）。
///
/// 必须**可替换**：音频初始化失败后重试会创建新的解码线程，而 `OnceLock`
/// 只能写入一次——重试后 `wake_decoder()` 会一直唤醒已退出的旧线程，新线程
/// 只能靠 4 ms 轮询兜底（表现为卡顿与延迟上升）。
static DECODE_THREAD: Mutex<Option<std::thread::Thread>> = Mutex::new(None);

fn set_decode_thread(t: std::thread::Thread) {
    *DECODE_THREAD.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
}

fn wake_decoder() {
    if let Some(t) = DECODE_THREAD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        t.unpark();
    }
}

/// 音频初始化状态。
/// 历史实现用裸 u32 + 魔数 0/1/2/3 表达四态，读写点分散在三个函数里，
/// 新增状态时极易漏改。改为枚举后编译器可覆盖穷举检查。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum AudioInitState {
    /// 初始化中（含冷启动等待）
    Pending = 0,
    /// Opus 解码器创建失败
    DecoderFailed = 1,
    /// WASAPI 输出设备打开失败
    DeviceFailed = 2,
    /// 就绪
    Ready = 3,
}

impl AudioInitState {
    fn load() -> Self {
        match AUDIO_INIT_STATE.load(Ordering::Acquire) {
            1 => Self::DecoderFailed,
            2 => Self::DeviceFailed,
            3 => Self::Ready,
            _ => Self::Pending,
        }
    }
    fn store(self) {
        AUDIO_INIT_STATE.store(self as u8, Ordering::Release);
    }
}

static AUDIO_INIT_STATE: AtomicU8 = AtomicU8::new(AudioInitState::Pending as u8);

/// 音频电平（dBFS），解码线程每 100 ms 更新一次
static AUDIO_LEVEL_DB: AtomicU32 = AtomicU32::new((-60.0f32).to_bits());

/// 总延迟估算（ms）：`now − 播放头那一帧到达本机的时刻 + 声卡自报延迟`。
///
/// 口径说明：这是接收端**唯一**能在本地测到的延迟——发送端采集到发包、
/// 以及单程网络时延，在没有跨设备时钟的情况下无法测出，不计入。
/// 它等于"此刻正在从音箱出来的那个样本，是多久以前到达网卡的"，
/// 也就是缓冲水位 + 声卡缓冲的真实合并值（比水位更接近听感延迟）。
static LATENCY_MS: AtomicU32 = AtomicU32::new(0);
/// 声卡自报的缓冲延迟（ms），供 UI/日志解释"总延迟"的构成
static DEVICE_LATENCY_MS: AtomicU32 = AtomicU32::new(0);
/// 当前重采样比率微调量（f32 位模式，±0.005）
static DRIFT_ADJUST_BITS: AtomicU32 = AtomicU32::new(0);

/// 用 Opus 带内 FEC 重建成功的帧数（每个统计窗口清零，诊断用）
static FEC_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// RTP 接收计数（诊断用）
static RTP_RX_PACKETS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RTP_RX_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RTP_RX_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 音频诊断计数器（进程级单例，UI 展示 + 落盘日志共用）
static AUDIO_STATS: OnceLock<Arc<audio::AudioStats>> = OnceLock::new();
fn audio_stats() -> &'static Arc<audio::AudioStats> {
    AUDIO_STATS.get_or_init(|| Arc::new(audio::AudioStats::default()))
}

/// 生成非零随机 32 位值（反向 SSRC 用）
fn random_u32_nonzero() -> u32 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    loop {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        let v = hasher.finish() as u32;
        if v != 0 {
            return v;
        }
    }
}

/// 启动（或重启）音频解码/播放线程。
///
/// 播放节拍完全由声卡播放头决定：
///   * 解码线程只在 `AudioWriter` 有空位时拉取帧，绝不无限预解码；
///   * 是否需要 PLC 由 `JitterBuffer::pull` 依据 `playhead_ts` 判定，
///     不存在"超过 N 毫秒没收到包就补"的计时器；
///   * 等待用 `park`/`unpark`（网络线程与音频回调共同唤醒），
///     超时值只是兜底唤醒，不参与播放决策。
fn init_audio_worker() {
    AudioInitState::Pending.store();
    let stats = audio_stats().clone();

    let spawn_result = std::thread::Builder::new()
        .name("udp2cal-decode".into())
        .spawn(move || {
            set_decode_thread(std::thread::current());

            let mut dec = match decoder::OpusDecoder::new() {
                Ok(d) => d,
                Err(e) => {
                    logf!("[Audio] Opus 解码器初始化失败: {e:?}");
                    AudioInitState::DecoderFailed.store();
                    return;
                }
            };
            let mut aw = match audio::start_audio(stats) {
                Ok(a) => a,
                Err(e) => {
                    logf!("[Audio] 初始化失败: {e}");
                    AudioInitState::DeviceFailed.store();
                    return;
                }
            };
            aw.set_waker(std::thread::current());
            AudioInitState::Ready.store();
            logf!(
                "[Audio] 就绪：输出设备 {} Hz（解码 48 kHz 单声道 / 10 ms），声卡缓冲 {} ms",
                aw.device_rate,
                aw.device_latency_ms()
            );

            // ── ASRC：唯一时钟是声卡，唯一观测量是缓冲水位，唯一手段是比率 ──
            //
            // 这一层不再有任何"设备帧 → 媒体时间戳"的换算：播放决策只看
            // 缓冲里还压着多少媒体，比率只改"一帧媒体换多少设备帧"。
            // 设备采样率作除数（ms 换算、ppm 归一、漂移控制器基准）。
            // `DeviceRateEstimator::with_rate` / `DriftController::new` 与
            // audio.rs 里的同类换算都要求非零；cpal 理论上不会给出 0，
            // 但这里是解码线程顶层，除零 = 线程 panic = 整条音频链死亡，
            // 必须栅位而不是赌。
            let device_rate = aw.device_rate.max(1);
            let mut drift = protocol::rtp::DriftController::new(device_rate);
            // 前馈只看**声卡设备进度 vs QPC**：网络攒批/重排不进入比率。
            // 发送端媒体时间线 vs Instant 的旧 RateEstimator 已从比率路径移除。
            let mut device_rate_est =
                protocol::rtp::DeviceRateEstimator::with_rate(device_rate);
            // 阶跃吸收（方案 C）：判据与限额与 Android 端同构
            let mut step = protocol::rtp::StepAbsorber::new();
            // 实际生效的目标水位（环境变量可覆盖，两端各自读各自的）
            let water_target = protocol::rtp::water_target_ms();
            // 启动预缓冲：先攒够本方向目标再开播，之后靠 ASRC 慢慢收敛；
            // 不再出现“3 帧就开播、再用 24 s 爬坡”的隐性瞬态。
            jitter()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_start_prebuffer_ms(water_target);
            let mut payload = [0u8; protocol::rtp::OPUS_MAX_PAYLOAD];
            let mut trim_payload = [0u8; protocol::rtp::OPUS_MAX_PAYLOAD];
            let silence_frame = [0.0f32; protocol::rtp::FRAME_SAMPLES as usize];
            let mut last_epoch = 0u64;
            let mut idle = false;
            let mut rms_sum = 0.0f64;
            let mut rms_count = 0u64;
            let mut last_stats = Instant::now();
            let mut last_latency_log = Instant::now();
            let mut last_packet_at = Instant::now();
            let mut last_newest_ts = 0u64;
            let mut idle_feedback = false;

            // 最近一次下发的总比率微调（反馈 Δu 与前馈 r̂ 的合成）
            let mut adjust: f64;
            let mut diag = diag::AsrcDiag::from_env();
            if diag.is_on() {
                logf!("[Asrc] P0 诊断打点已开启：{}", diag.path_display());
            }

            loop {
                if APP_RUNNING.load(Ordering::Acquire) == 0 {
                    if !idle {
                        idle = true;
                        jitter().lock().unwrap_or_else(|e| e.into_inner()).reset();
                        drift.reset();
                        device_rate_est.reset();
                        step.reset();
                        dec.reset();
                        aw.flush();
                        last_packet_at = Instant::now();
                        idle_feedback = false;
                        AUDIO_LEVEL_DB.store((-60.0f32).to_bits(), Ordering::Relaxed);
                    }
                    std::thread::park_timeout(Duration::from_millis(50));
                    continue;
                }
                idle = false;

                // 新会话 / 重同步：epoch 变化 → 重建解码器与全部估计器。
                // 速率估计器必须在这里一并复位：resync 会重建 highest_ts 基准。
                let epoch = jitter().lock().unwrap_or_else(|e| e.into_inner()).epoch;
                if epoch != last_epoch {
                    last_epoch = epoch;
                    dec.reset();
                    drift.reset();
                    device_rate_est.reset();
                    step.reset();
                    aw.flush();
                }

                // ── 水位（唯一观测量）──
                // ring = 已解码、已写进 PCM 环、还没被声卡接手的媒体量
                // pipe = ring + 声卡已接手还没播完的（= 可听点之前压着的总量）
                let ring_frames = aw.filled_frames() as u64;
                let dev_frames = aw.device_latency_frames();
                let pipe_frames = ring_frames + dev_frames;
                let ring_ms = ((ring_frames * 1000) / device_rate as u64) as u32;
                let pipe_ms = ((pipe_frames * 1000) / device_rate as u64) as u32;
                // 把解码缓冲折回媒体样本，才知道"此刻出声的那一帧"是哪个时间戳
                let queued_media = (pipe_frames as f64 * aw.current_ratio()) as u64;

                let (
                    pull,
                    jb_water,
                    next_ts,
                    newest_ts,
                    arrival,
                    plc_frames,
                    trims,
                    step_hit,
                    playing,
                ) = {
                    let mut jb = jitter().lock().unwrap_or_else(|e| e.into_inner());

                    // ── 阶跃吸收（方案 C）：持续超额才裁，且逐帧解码后再丢 PCM ──
                    // 判据：总水位持续 STEP_TRIM_HOLD_MS 超过 target+STEP_TRIM_EXCESS_MS。
                    // 单次 90 ms 攒批不再触发。裁剪必须按播放顺序把每一帧交给
                    // Opus 解码器（PLC 缺口走 conceal），只丢弃解码后的 PCM；
                    // 直接清槽位会让有状态解码器与发送端错位，接缝处爆音。
                    let before_water = jb.occupancy_ms();
                    let before_total = before_water.saturating_add(pipe_ms);
                    let decision = step.decide(before_water, pipe_ms, jb.is_playing(), water_target);
                    let mut hit: Option<(u32, u32, u32, bool)> = None;
                    if decision.frames > 0 {
                        let mut trimmed = 0u32;
                        if !decision.limited {
                            for _ in 0..decision.frames {
                                match jb.pull_trim(&mut trim_payload) {
                                    protocol::rtp::Pull::Packet { len, .. } => {
                                        // 解码只为推进状态；PCM 不写环 = 裁掉
                                        let _ = dec.decode(&trim_payload[..len]);
                                        trimmed += 1;
                                    }
                                    protocol::rtp::Pull::Plc { .. } => {
                                        let _ = dec.conceal();
                                        trimmed += 1;
                                    }
                                    protocol::rtp::Pull::Silence { .. } => {
                                        trimmed += 1;
                                    }
                                    protocol::rtp::Pull::Wait => break,
                                }
                            }
                        }
                        hit = Some((before_total, decision.frames, trimmed, decision.limited));
                    }

                    let r = jb.pull(ring_ms, pipe_ms, &mut payload);
                    let next_ts = jb.next_ts();
                    let highest = jb.highest_ts();
                    let audible_ts = next_ts.saturating_sub(queued_media);
                    (
                        r,
                        jb.water_ms(),
                        next_ts,
                        highest,
                        jb.arrival_of(audible_ts),
                        jb.stats.plc_frames,
                        jb.stats.step_trims,
                        hit,
                        jb.is_playing(),
                    )
                };
                let water = jb_water.saturating_add(pipe_ms);
                if newest_ts != last_newest_ts {
                    last_newest_ts = newest_ts;
                    last_packet_at = Instant::now();
                }

                // 阶跃吸收的日志（锁外：logf 会写盘）。水位一律用**总水位**
                // （jb + pipe）口径，与判据一致；否则“跳了 120 ms 只掉 76 ms”
                // 会把人带坑里。限额分支每 60 s 窗口只放行一条：被压制时水位
                // 不会自己降下来，不节流就是 ≈250 条/秒。
                if let Some((before, want, skipped, limited)) = step_hit {
                    if limited {
                        if step.take_limited_log() {
                            logf!(
                                "[Step] 水位 {}ms（目标 {}）命中阶跃吸收，但每分钟 {} 次限额已满（本窗口已用 {} 次）：本次只记录不裁剪",
                                before,
                                water_target,
                                protocol::rtp::STEP_TRIM_MAX_PER_MIN,
                                step.trims_in_window()
                            );
                        }
                    } else if skipped > 0 {
                        logf!(
                            "[Step] 阶跃吸收：整帧跳 {} / {} 帧（{}ms），水位 {}→{}ms，累计 {} 帧",
                            skipped,
                            want,
                            skipped * protocol::rtp::FRAME_MS,
                            before,
                            water,
                            trims
                        );
                    }
                }

                // ── 前馈：只管“声卡设备时钟 vs QPC” ──
                // 设备进度与时刻都取自音频回调里的 IAudioClock 采样：设备帧数
                // 与 `timestamp().playback` 对应的 QPC 时刻绑定，而不是解码线程
                // 被唤醒时的 `Instant::now()`。网络到达节奏不进入这里，90 ms
                // 攒批因此不会调制音高。
                let (played, played_at) = aw
                    .device_clock_sample()
                    .unwrap_or_else(|| (aw.played_frames(), Instant::now()));
                if played > 0 {
                    device_rate_est.observe(played, played_at);
                }
                let ff = device_rate_est.feedforward().unwrap_or(0.0);

                // ── 反馈：只收长期残差 ──
                // 计时同样用声卡已消费帧数（断流时媒体样本不前进，用它计时会冻结积分）。
                // 长时间没有新 RTP 时冻结反馈并复位积分：否则 `pull` 推进播放头后
                // water≈0，PI 会一路积到 −0.5%，恢复出声时带明显音高偏移。
                let rtp_idle = last_packet_at.elapsed() > Duration::from_millis(500);
                // 反馈只在**真的在出声**时才评估：预缓冲阶段水位必然低于目标，
                // 照常喂 PI 会把积分一路压到负饱和（`PI_INTEGRAL_MAX`），开播后
                // 只能按 ≈3 ppm/s 慢慢退，稳态水位因此被顶在目标之上 10–30 ms
                // 长达数分钟。现场日志指纹就在这里：开播瞬间 `反馈 -1100ppm`，
                // 之后 -457 → -294 → +21 → +65 缓慢爬升。
                let feedback_on = !rtp_idle && playing;
                let du = if !feedback_on {
                    if !idle_feedback {
                        idle_feedback = true;
                        drift.reset();
                    }
                    0.0
                } else {
                    if idle_feedback {
                        idle_feedback = false;
                        drift.reset();
                    }
                    drift.update(water, played)
                };
                adjust = ff + du;
                // 两支路相合后仍须守比率钳位：前馈和反馈各自钳过了，乘积可能越界。
                // 在这里钳一次，诊断 CSV 里看到的才是真正下发给重采样器的数。
                adjust = adjust.clamp(
                    -protocol::rtp::RESAMPLE_MAX_ADJUST,
                    protocol::rtp::RESAMPLE_MAX_ADJUST,
                );
                aw.set_ratio_adjust(adjust);
                DRIFT_ADJUST_BITS.store((adjust as f32).to_bits(), Ordering::Relaxed);

                // ── 总延迟估算 ──
                // `audible_ts` 已经扣掉了“PCM 环 + 声卡排队”对应的媒体量，
                // 因此 now − arrival(audible_ts) **本身**就是听感延迟，不能再加
                // device_latency（旧公式把声卡等待算了两遍）。
                if let Some(at) = arrival {
                    let dev_ms = aw.device_latency_ms();
                    DEVICE_LATENCY_MS.store(dev_ms, Ordering::Relaxed);
                    let age_ms = at.elapsed().as_millis().min(u32::MAX as u128) as u32;
                    LATENCY_MS.store(age_ms, Ordering::Relaxed);
                }

                match pull {
                    protocol::rtp::Pull::Packet { len, .. } => {
                        if let Some(n) = dec.decode(&payload[..len]) {
                            let pcm = dec.pcm_mut(n);
                            rms_sum += pcm.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>();
                            rms_count += n as u64;
                            aw.write(pcm, protocol::rtp::OPUS_SAMPLE_RATE);
                        }
                        // 解码失败按丢包处理：不写数据，水位到期后自然 PLC
                    }
                    protocol::rtp::Pull::Plc { .. } => {
                        // 先用 Opus 带内 FEC 重建这一帧：丢帧的 LBRR 冗余在**下一个
                        // 包里**，所以前瞻一个包（不消费，下一轮 `pull` 仍会正常取它）
                        // 来还原；只有下一个包也还没到时才退回 PLC。发送端没开
                        // in-band FEC 时 libopus 会自动退化成 PLC，故不会更差。
                        // 现实中这正好覆盖"包迟到/被 AP 攒批挤掉"的短洞：
                        // 过去每洞必 PLC（30 ms 听起来就是一下音量塌陷）。
                        let fec_n = {
                            let jb = jitter().lock().unwrap_or_else(|e| e.into_inner());
                            match jb.peek_current(&mut payload) {
                                Some(len) => dec.decode_fec(&payload[..len]),
                                None => None,
                            }
                        };
                        if fec_n.is_some() {
                            FEC_FRAMES.fetch_add(1, Ordering::Relaxed);
                        }
                        match fec_n.or_else(|| dec.conceal()) {
                            Some(n) => {
                                let pcm = dec.pcm_mut(n);
                                rms_sum +=
                                    pcm.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>();
                                rms_count += n as u64;
                                aw.write(pcm, protocol::rtp::OPUS_SAMPLE_RATE);
                            }
                            None => {
                                aw.write(&silence_frame, protocol::rtp::OPUS_SAMPLE_RATE);
                            }
                        }
                    }
                    protocol::rtp::Pull::Silence { .. } => {
                        // 连续 PLC 已达上限：输出静音但**继续推进媒体时间**
                        // （`pull` 内部已 advance），否则恢复时真包会被判迟到
                        aw.write(&silence_frame, protocol::rtp::OPUS_SAMPLE_RATE);
                    }
                    protocol::rtp::Pull::Wait => {
                        // 现在不该生产：生产节奏由声卡消费节奏约束
                        std::thread::park_timeout(Duration::from_millis(4));
                    }
                }

                if diag.due() {
                    diag.row(format_args!(
                        "{},{},{},{},{},{:.0},{:.1},{:.1},{},{:.3},{},{},{},{},{}",
                        ring_ms,
                        pipe_ms,
                        jb_water,
                        water,
                        water_target,
                        adjust * 1e6,
                        ff * 1e6,
                        device_rate_est.sigma_ppm(),
                        device_rate_est.buckets(),
                        played as f64 / device_rate as f64,
                        newest_ts,
                        next_ts,
                        plc_frames,
                        audio_stats().snapshot().underrun,
                        trims,
                    ));
                }

                if last_stats.elapsed() > Duration::from_millis(100) {
                    let level_db = if rms_count > 0 {
                        let rms = (rms_sum / rms_count as f64).sqrt() as f32;
                        20.0 * (rms.max(1e-10)).log10()
                    } else {
                        -60.0
                    };
                    AUDIO_LEVEL_DB.store(level_db.to_bits(), Ordering::Relaxed);
                    rms_sum = 0.0;
                    rms_count = 0;
                    last_stats = Instant::now();
                }

                // 延迟构成每 10 s 落一次盘：水位分解、比率、前馈与估计质量
                // 一目了然，出现"延迟越来越大"时可直接判断是哪一段在涨。
                if last_latency_log.elapsed() >= Duration::from_secs(10) {
                    last_latency_log = Instant::now();
                    logf!(
                        "[Latency] 总延迟≈{}ms（水位 {}ms = 抖动 {} + 解码 {}，目标 {}）声卡 {}ms 比率 {:+.3}%（反馈 {:+.0}ppm / 前馈 {:+.1}ppm，σ {:.1}ppm，{} 桶，阶跃裁剪 {} 帧）",
                        LATENCY_MS.load(Ordering::Relaxed),
                        water,
                        jb_water,
                        pipe_ms,
                        water_target,
                        aw.device_latency_ms(),
                        adjust * 100.0,
                        du * 1e6,
                        ff * 1e6,
                        device_rate_est.sigma_ppm(),
                        device_rate_est.buckets(),
                        trims,
                    );
                }
            }
        });

    // 线程创建失败（极端堆耗尽 / 线程数上限）必须报错：
    // 原先用 `let _ =` 吞掉，`AudioInitState` 永远停在 Pending，等待超时后
    // 引擎照常启动 → UI 显示“运行中”但永远没有声音，日志一片空白。
    if let Err(e) = spawn_result {
        logf!("[Audio] 解码线程创建失败: {e}");
        AudioInitState::DeviceFailed.store();
    }
}

// ══════════════════════════════════════════════
// 广播状态机 (独立线程)
// ══════════════════════════════════════════════
// Ready 模式: 每 1 秒广播 TYPE_DISCOVER_REPLY 到 LAN
// Silent 模式: APP 停止 / DEVICE_BUSY / 反向不可用时停止广播

/// 当前监听端口（缓存，避免广播线程每秒读注册表）
static LISTEN_PORT: AtomicU32 = AtomicU32::new(protocol::DEFAULT_LISTEN_PORT as u32);

fn start_broadcast_state_machine() {
    std::thread::spawn(move || {
        let socket = match std::net::UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = socket.set_broadcast(true);
        let device_id = my_device_id();
        let mut buf = [0u8; protocol::CONTROL_HEADER_SIZE + protocol::CONTROL_MAX_PAYLOAD];
        loop {
            if APP_RUNNING.load(Ordering::Acquire) != 0 && !p2p_snapshot().is_busy() {
                let reply = protocol::DiscoverReply {
                    listen_port: LISTEN_PORT.load(Ordering::Acquire) as u16,
                    device_name: my_device_name().to_string(),
                };
                let payload = reply.encode();
                if let Some(total) = protocol::build_control_to(
                    &mut buf,
                    0,
                    protocol::TYPE_DISCOVER_REPLY,
                    &device_id,
                    &payload,
                ) {
                    let _ =
                        socket.send_to(&buf[..total], ("255.255.255.255", protocol::DISCOVER_PORT));
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// 释放绑定：回到 READY，停反向发送，清空抖动缓冲。
/// 控制面唯一的状态回退路径（超时 / 停止 / 引擎错误都走这里）。
fn reset_to_ready() {
    {
        let mut st = p2p_lock();
        *st = P2pState::new();
    }
    stop_reverse_sender();
    jitter().lock().unwrap_or_else(|e| e.into_inner()).reset();
    wake_decoder();
}

// ══════════════════════════════════════════════
// 发现服务 (独立线程)
// ══════════════════════════════════════════════
pub fn start_broadcast_listener() {
    std::thread::spawn(move || {
        let socket = match std::net::UdpSocket::bind(("0.0.0.0", protocol::DISCOVER_PORT)) {
            Ok(s) => s,
            Err(e) => {
                logf!("[Discovery] 绑定广播端口失败: {}", e);
                return;
            }
        };
        let mut buf = [0u8; 1500];
        let my_id = my_device_id();
        // 简单令牌桶：同一源 IP 每秒最多应答 MAX_PER_SEC 次。
        // 发现请求 15B、应答 ~15B+主机名，无限制时可被用于 UDP 反射放大。
        let mut bucket: std::collections::HashMap<std::net::IpAddr, (u32, Instant)> =
            std::collections::HashMap::new();
        let mut last_cleanup = Instant::now();
        loop {
            let (amt, src) = match socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if last_cleanup.elapsed() > Duration::from_secs(30) {
                bucket.retain(|_, (_, t)| t.elapsed() < Duration::from_secs(5));
                last_cleanup = Instant::now();
            }
            if !allow_discovery_request(&mut bucket, src.ip(), Instant::now()) {
                continue;
            }

            let Some((hdr, payload)) = protocol::parse_control(&buf[..amt]) else {
                continue;
            };
            // 只接受空负载的 DISCOVER_REQ；Busy / 未启动一律不回
            if !is_valid_control_header(&hdr)
                || hdr.msg_type != protocol::TYPE_DISCOVER_REQ
                || !payload.is_empty()
            {
                continue;
            }
            if APP_RUNNING.load(Ordering::Acquire) == 0 || p2p_snapshot().is_busy() {
                continue;
            }
            let reply = protocol::DiscoverReply {
                listen_port: LISTEN_PORT.load(Ordering::Acquire) as u16,
                device_name: my_device_name().to_string(),
            };
            let body = reply.encode();
            if let Some(total) = protocol::build_control_to(
                &mut buf,
                0,
                protocol::TYPE_DISCOVER_REPLY,
                &my_id,
                &body,
            ) {
                let _ = socket.send_to(&buf[..total], src);
            }
        }
    });
}

// ══════════════════════════════════════════════
// UI 及主控逻辑
// ══════════════════════════════════════════════

#[derive(Debug, Clone)]
enum Message {
    IpChanged(String),
    PortChanged(String),
    ToggleRunning,
    ToggleReverse(bool),
    ToggleAutoStart(bool),
    StatusUpdate(StatusInfo),
    Disconnected,
    /// 引擎级失败（监听失败 / 音频初始化失败）：必须回到停止态并把原因显示出来
    EngineError(String),
    /// P2P 绑定/重连状态变化：只触发一次重绘，不携带也不修改任何遥测数据。
    /// （历史实现用 bitrate=0 的 StatusUpdate 充当重绘信号，导致码率显示每秒闪一次 0）
    PeerStateChanged,
    HideWindow,
    ShowWindow,
    Quit,
}

#[derive(Debug, Clone)]
struct StatusInfo {
    bitrate_kbps: f32,
    level_db: f32,
}

/// 提示行的语义级别。
/// 历史实现靠中文子串（"失败/无效/不能/断开/不是合法"）猜测配色，
/// 任何文案调整都会让颜色错位；改为在写入处显式声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeLevel {
    Info,
    Error,
}

struct AppState {
    config: config::Config,
    ip_input: String,
    port_input: String,
    port_valid: bool,
    is_running: bool,
    connected: bool,
    last_bitrate: f32,
    last_level_db: f32,
    status_text: String,
    /// 需要直接展示给用户的提示（错误 / 警告 / 断开原因）
    notice: String,
    /// 提示级别，决定配色
    notice_level: NoticeLevel,
    vb_cable_installed: bool,
    last_toggle_instant: Instant,
    /// 上次写音频诊断日志的时间（30s 一次，避免刷盘）
    last_stats_log: Instant,
    session_id: u32,
    reverse_audio: bool,
    reverse_enabled: bool,
}

fn main() -> Result<(), iced::Error> {
    // 尽早初始化落盘日志：后续所有失败路径都要能留下证据
    logger::init();
    // 任何线程 panic 都要落盘：GUI 子系统下 stderr 会被丢弃
    logger::install_panic_hook();
    logf!("[App] 启动 v{}", env!("CARGO_PKG_VERSION"));
    if let Some(p) = logger::path() {
        logf!("[App] 日志文件: {}", p.display());
    }

    // SAFETY: 三处 `PCWSTR::from_raw` 的缓冲都是本作用域内的 `Vec<u16>`，
    // 以 `\0` 结尾且生命周期覆盖调用；`CreateMutexW` 句柄故意泄漏到进程结束
    // （单实例锁整个进程生命周期有效），不对其调用 `CloseHandle`。
    unsafe {
        let name: Vec<u16> = "UDP2CAL_SingleInstance_Mutex\0".encode_utf16().collect();
        let _ = CreateMutexW(None, true, PCWSTR::from_raw(name.as_ptr()));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // 先尝试把已运行实例的窗口拉到前台；再提示，避免"双击没反应"
            focus_existing_window("UDP2CAL");
            let title: Vec<u16> = "UDP2CAL\0".encode_utf16().collect();
            let msg: Vec<u16> = "程序已在运行中（已尝试切换到已有窗口）\0"
                .encode_utf16()
                .collect();
            MessageBoxW(
                None,
                PCWSTR::from_raw(msg.as_ptr()),
                PCWSTR::from_raw(title.as_ptr()),
                MB_OK | MB_ICONINFORMATION,
            );
            return Ok(());
        }
    }

    let startup_cfg = config::Config::load();
    LISTEN_PORT.store(startup_cfg.listen_port, Ordering::Release);

    // 防火墙放行：限定到本程序 + 具体端口。失败必须可被用户看到，
    // 因此同步执行并把错误作为首条提示传入界面。
    let firewall_warning = firewall::add_firewall_rule(startup_cfg.listen_port as u16).err();
    if let Some(err) = &firewall_warning {
        logf!("[Firewall] {err}");
    }

    // P2P 系统初始化
    start_broadcast_listener();
    start_broadcast_state_machine();
    init_audio_worker();

    // 初始化图标
    let (icon_rgba, w, h) = load_icon_rgba();

    // 系统托盘（失败不致命：降级为"关窗即退出"）
    let tray_icon = install_tray(icon_rgba.clone(), w, h);
    let tray_ok = tray_icon.is_some();
    if !tray_ok {
        logf!("[Tray] 托盘不可用，降级为关闭窗口即退出");
    }

    // 首条提示：防火墙失败 / 托盘降级，二者可能同时出现
    let startup_notice: String = match (&firewall_warning, tray_ok) {
        (Some(err), false) => format!("{err}；托盘不可用，关闭窗口将直接退出"),
        (Some(err), true) => err.clone(),
        (None, false) => "托盘不可用，关闭窗口将直接退出".to_string(),
        (None, true) => String::new(),
    };

    // 主窗体
    let result = iced::application(
        move || {
            let cfg = config::Config::load();
            REVERSE_ENABLED.store(
                if cfg.reverse_enabled != 0 { 1 } else { 0 },
                Ordering::Release,
            );
            let now = Instant::now();
            let state = AppState {
                ip_input: cfg.listen_ip.clone(),
                port_input: cfg.listen_port.to_string(),
                port_valid: true,
                is_running: false,
                connected: false,
                last_bitrate: 0.0,
                last_level_db: -60.0,
                status_text: String::new(),
                notice: startup_notice.clone(),
                notice_level: if firewall_warning.is_some() || !tray_ok {
                    NoticeLevel::Error
                } else {
                    NoticeLevel::Info
                },
                vb_cable_installed: audio::detect_vb_cable().is_some(),
                reverse_audio: false,
                reverse_enabled: cfg.reverse_enabled != 0,
                config: cfg,
                last_toggle_instant: now,
                last_stats_log: now,
                session_id: 0,
            };
            (state, Task::none())
        },
        AppState::update,
        AppState::view,
    )
    .title("UDP2CAL")
    .subscription(AppState::subscription)
    .default_font(iced::Font::with_name("Microsoft YaHei"))
    .theme(Theme::Dark)
    .window(iced::window::Settings {
        // ⚠ 此尺寸是精心设计的视觉尺寸，请勿修改。
        // 320×290 客户区 + 1px 边框 + 标题栏，成品窗口恰好是 322×322 的完美正方形；
        // 任何改动都会破坏这个正方形比例。
        size: iced::Size::new(320.0, 290.0),
        position: iced::window::Position::Centered,
        resizable: false,
        // 托盘不可用时必须让 X 真正退出：否则窗口隐藏后再也唤不回来，
        // 进程会变成只能从任务管理器结束的幽灵进程。
        exit_on_close_request: !tray_ok,
        icon: iced::window::icon::from_rgba(icon_rgba, w, h).ok(),
        ..Default::default()
    })
    .run();

    drop(tray_icon);
    result
}

/// 诊断数值缩写：12345 → "12.3k"，2_000_000 → "2.0M"
fn short_num(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

impl AppState {
    /// 保存配置，失败必须留证据。
    ///
    /// 注册表写入可能被杀毒软件/组策略拦截。用 `let _ =` 吞掉错误会让用户
    /// 以为设置已持久化：重启后配置静默回退，而日志里没有任何线索。为此把
    /// 错误写进日志并提示用户（回滚语义由各调用点自己决定，这里不改配置值）。
    fn persist_config(&mut self) {
        if let Err(e) = self.config.save() {
            logf!("[Config] 保存配置失败: {e}");
            self.notice = format!("配置保存失败：{e}");
            self.notice_level = NoticeLevel::Error;
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::IpChanged(s) => {
                self.ip_input = s;
                Task::none()
            }
            Message::PortChanged(s) => {
                self.port_input = s;
                self.port_valid =
                    self.port_input.parse::<u16>().is_ok() || self.port_input.is_empty();
                Task::none()
            }
            Message::ToggleAutoStart(enabled) => {
                // 写入失败必须回滚开关并告知用户，避免"开关假亮"
                match self.config.set_auto_start(enabled) {
                    Ok(()) => {
                        self.persist_config();
                        self.notice.clear();
                    }
                    Err(e) => {
                        self.notice = e;
                        self.notice_level = NoticeLevel::Error;
                    }
                }
                Task::none()
            }
            Message::ToggleReverse(enabled) => {
                // 运行中不可更改
                if self.is_running {
                    return Task::none();
                }
                self.reverse_enabled = enabled;
                REVERSE_ENABLED.store(if enabled { 1 } else { 0 }, Ordering::Release);
                self.config.set_reverse_enabled(enabled);
                self.persist_config();
                Task::none()
            }
            Message::ToggleRunning => {
                if self.last_toggle_instant.elapsed() < Duration::from_millis(200) {
                    return Task::none();
                }
                self.last_toggle_instant = Instant::now();
                self.session_id = self.session_id.wrapping_add(1);

                if self.is_running {
                    APP_RUNNING.store(0, Ordering::Release);
                    REVERSE_ENABLED.store(0, Ordering::Release);
                    reset_to_ready();
                    self.is_running = false;
                    self.connected = false;
                    self.reverse_audio = false;
                    self.last_bitrate = 0.0;
                    self.last_level_db = -60.0;
                    self.status_text.clear();
                    self.notice.clear();
                } else {
                    let port: u16 = match self.port_input.parse() {
                        Ok(p) => p,
                        Err(_) => {
                            self.notice = "端口号无效，请输入 1-65535".into();
                            self.notice_level = NoticeLevel::Error;
                            return Task::none();
                        }
                    };
                    if port == 0 {
                        self.notice = "端口号不能为 0".into();
                        self.notice_level = NoticeLevel::Error;
                        return Task::none();
                    }
                    // 监听地址必须是本机地址；空/非法值会让 bind 直接失败
                    let ip = self.ip_input.trim().to_string();
                    if ip.is_empty() {
                        self.notice = "监听地址不能为空".into();
                        self.notice_level = NoticeLevel::Error;
                        return Task::none();
                    }
                    if ip.parse::<std::net::IpAddr>().is_err() && ip != "localhost" {
                        self.notice = format!("监听地址「{ip}」不是合法的 IP");
                        self.notice_level = NoticeLevel::Error;
                        return Task::none();
                    }

                    self.config.listen_ip = ip;
                    self.config.listen_port = port as u32;
                    self.persist_config();
                    LISTEN_PORT.store(port as u32, Ordering::Release);

                    // 上次音频初始化失败过则重试一次（例如用户刚装好 VB-Cable）
                    if matches!(
                        AudioInitState::load(),
                        AudioInitState::DecoderFailed | AudioInitState::DeviceFailed
                    ) {
                        init_audio_worker();
                    }
                    // 端口可能在本次会话中改过，补齐防火墙规则
                    std::thread::spawn(move || {
                        let _ = firewall::add_firewall_rule(port);
                    });

                    APP_RUNNING.store(1, Ordering::Release);
                    reset_to_ready();
                    self.is_running = true;
                    self.connected = false;
                    self.last_bitrate = 0.0;
                    self.last_level_db = -60.0;
                    self.status_text = "等待连接...".into();
                    self.notice.clear();
                    self.vb_cable_installed = audio::detect_vb_cable().is_some();
                    // 应用反向串流设置到全局
                    REVERSE_ENABLED
                        .store(if self.reverse_enabled { 1 } else { 0 }, Ordering::Release);
                }
                Task::none()
            }
            Message::StatusUpdate(info) => {
                // 「已连接」= 控制面绑定存在。规范 6.4 明确保活只看控制面，
                // 不得用"是否收到 RTP"判断存活；历史实现只在 bitrate>0 时置真、
                // 从不置假，导致 RTP 停止后码率归零而界面仍显示已连接。
                let bound = p2p_snapshot().is_busy();
                if bound != self.connected {
                    self.connected = bound;
                    if bound {
                        // 音频真正流动起来后清掉历史错误与断开标记
                        self.notice.clear();
                        self.status_text.clear();
                    } else if self.is_running {
                        self.last_bitrate = 0.0;
                        self.last_level_db = -60.0;
                        self.status_text = "已断开".into();
                    }
                }
                self.last_bitrate = info.bitrate_kbps;
                self.last_level_db = info.level_db;
                // 诊断：每 30s 落一次盘（音频计数器 + RTP/抖动缓冲计数器）
                if self.last_stats_log.elapsed() >= Duration::from_secs(30) {
                    self.last_stats_log = Instant::now();
                    let s = audio_stats().snapshot();
                    let (j, water) = {
                        let jb = jitter().lock().unwrap_or_else(|e| e.into_inner());
                        (jb.stats, jb.water_ms())
                    };
                    let peer = p2p_snapshot();
                    logf!(
                        "[Audio] 统计 块={} 重采样={} 丢样={} 满丢旧数据={} 欠载={}",
                        s.writes,
                        s.resampled,
                        s.dropped,
                        s.flush_on_full,
                        s.underrun
                    );
                    logf!(
                        "[Audio] 延迟≈{}ms（含声卡 {}ms）比率微调 {:+.3}%",
                        LATENCY_MS.load(Ordering::Relaxed),
                        DEVICE_LATENCY_MS.load(Ordering::Relaxed),
                        f32::from_bits(DRIFT_ADJUST_BITS.load(Ordering::Relaxed)) * 100.0
                    );
                    logf!(
                        "[RTP] 收={} 字节={} 迟到={} 重复={} 溢出={} 重同步={} PLC={}(峰值{}) FEC={} 水位={}ms SSRC={:#010x}",
                        j.received, j.bytes, j.late, j.duplicate, j.overflow, j.resyncs,
                        j.plc_frames, j.max_plc_run,
                        FEC_FRAMES.swap(0, Ordering::Relaxed), water, peer.forward_ssrc
                    );
                    logf!(
                        "[RTP] 丢包/非法包={} 累计接收={}",
                        RTP_RX_DROPPED.load(Ordering::Relaxed),
                        RTP_RX_PACKETS.load(Ordering::Relaxed)
                    );
                    // 反向链路统计由 UI 线程汇总：发送线程内**不得**做文件 I/O，
                    // 否则一次慢写盘就会打断 RTP（实测每 30 s 一次破音）
                    let r = capture::take_reverse_stats();
                    logf!(
                        "[Reverse] 30s: 包={} 字节={} 编码错误={} 超限={} 丢样={} 发送错误={} 发送中断={} 帧={} 均值us(wait/read/encode/send)={:.0}/{:.0}/{:.0}/{:.0} 积压峰值={}ms",
                        r.packets, r.bytes, r.encode_errors, r.oversize, r.dropped_samples,
                        r.send_errors, r.gaps, r.frames,
                        r.avg_us(r.wait_us),
                        r.avg_us(r.read_us),
                        r.avg_us(r.encode_us),
                        r.avg_us(r.send_us),
                        r.max_backlog / 2 * 10 / 480
                    );
                    // 节拍迟到量（借鉴 Sunshine 的 "Sleep overshoot"）：
                    // 只报 min/max/avg，不做 EWMA——偶发的百毫秒级卡顿才是最该看见的。
                    if r.late_n > 0 || r.reanchors > 0 {
                        logf!(
                            "[Pacer] 30s: 节拍迟到 min/avg/max={}/{}/{}us 帧={} 重锚={}",
                            r.late_min_us,
                            r.late_avg_us(),
                            r.late_max_us,
                            r.late_n,
                            r.reanchors
                        );
                    }
                }
                Task::none()
            }
            Message::Disconnected => {
                if self.is_running && self.connected {
                    self.connected = false;
                    self.last_bitrate = 0.0;
                    self.last_level_db = -60.0;
                    // 只更新顶部状态点（● 已断开），底部提示行不再重复同一信息
                    self.status_text = "已断开".into();
                }
                Task::none()
            }
            Message::EngineError(msg) => {
                // 引擎级失败：必须回到停止态，否则界面会一直"假运行"
                APP_RUNNING.store(0, Ordering::Release);
                reset_to_ready();
                self.is_running = false;
                self.connected = false;
                self.reverse_audio = false;
                self.last_bitrate = 0.0;
                self.last_level_db = -60.0;
                self.status_text.clear();
                self.notice = msg;
                self.notice_level = NoticeLevel::Error;
                Task::none()
            }
            Message::PeerStateChanged => Task::none(),
            Message::HideWindow => {
                latest_window_task(|id| iced::window::set_mode(id, iced::window::Mode::Hidden))
            }
            Message::ShowWindow => latest_window_task(|id| {
                Task::batch([
                    iced::window::set_mode(id, iced::window::Mode::Windowed),
                    iced::window::gain_focus(id),
                ])
            }),
            Message::Quit => iced::exit(),
        }
    }

    /// 状态点文字：靠 `status_text` 区分"已断开"与"等待连接"。
    /// 这里只影响显示，不再参与配色判定。
    fn status_is_disconnected(&self) -> bool {
        self.status_text.contains("断开")
    }

    fn view(&self) -> Element<'_, Message> {
        let bg = Color::from_rgb(0.08, 0.08, 0.10);
        let card_bg = Color::from_rgb(0.13, 0.13, 0.16);
        let border_color = Color::from_rgb(0.20, 0.20, 0.25);
        let accent = Color::from_rgb(0.0, 0.80, 0.45);
        let red = Color::from_rgb(0.95, 0.25, 0.30);
        let grey = Color::from_rgb(0.40, 0.40, 0.45);
        let dim = Color::from_rgb(0.55, 0.55, 0.60);

        let (sc, st) = if !self.is_running {
            (grey, "● 已停止")
        } else if self.connected {
            (accent, "● 已连接")
        } else {
            (
                red,
                if self.status_is_disconnected() {
                    "● 已断开"
                } else {
                    "● 等待连接"
                },
            )
        };

        // P2P 状态: TYPE_CONNECT 独占绑定 / 空闲
        // 已占用后不再拼接 8 位设备识别码：对用户没有意义，还会把顶部一行挤满。
        let p2p = p2p_snapshot();
        let (p2p_state, p2p_color) = if !self.is_running {
            (String::new(), dim)
        } else if p2p.is_busy() {
            ("● 已占用".to_string(), Color::from_rgb(0.95, 0.55, 0.15))
        } else {
            ("● 空闲".to_string(), accent)
        };

        let vb_str = if self.vb_cable_installed {
            "● VB-Cable"
        } else {
            "● VB-Cable 未安装"
        };
        let vb_color = if self.vb_cable_installed {
            Color::from_rgb(0.25, 0.80, 0.40)
        } else {
            Color::from_rgb(0.90, 0.55, 0.15)
        };

        // ── 底部状态行（永远只占一行，三种状态互斥）──
        //   未启动        → 广播地址
        //   已启动未连上  → 广播地址 + 广播状态
        //   已连接        → 音频诊断 + 反向串流状态（地址与广播状态对用户已无意义）
        let footer_left = if self.connected {
            let s = audio_stats().snapshot();
            // 延迟放在最前：它是用户唯一能直接感知的指标，
            // 重采样计数已失去诊断价值（比率每帧都在动），只在日志里保留
            format!(
                "延迟 {}ms · 丢样 {} · 欠载 {}",
                LATENCY_MS.load(Ordering::Relaxed),
                short_num(s.dropped),
                short_num(s.underrun)
            )
        } else {
            "广播地址: 255.255.255.255:44043".to_string()
        };
        let (footer_right, footer_right_color) = if !self.is_running {
            (String::new(), dim)
        } else if self.connected {
            if self.reverse_enabled {
                (
                    "● 反向串流已启用".to_string(),
                    Color::from_rgb(0.25, 0.80, 0.40),
                )
            } else {
                ("○ 反向串流未启用".to_string(), dim)
            }
        } else {
            let bc_on = APP_RUNNING.load(Ordering::Acquire) != 0 && !p2p.is_busy();
            if bc_on {
                ("● 广播中".to_string(), accent)
            } else {
                ("● 已静音".to_string(), dim)
            }
        };

        let mk = |label: &'static str, color: Color, msg: Message| {
            button(text(label).size(16))
                .on_press(msg)
                .width(Length::Fill)
                .height(34)
                .style(move |_, status| {
                    let mut bg_color = color;
                    match status {
                        iced::widget::button::Status::Hovered => {
                            bg_color = Color::from_rgb(
                                (color.r + 0.06).min(1.0),
                                (color.g + 0.06).min(1.0),
                                (color.b + 0.06).min(1.0),
                            );
                        }
                        iced::widget::button::Status::Pressed => {
                            bg_color = Color::from_rgb(
                                (color.r - 0.06).max(0.0),
                                (color.g - 0.06).max(0.0),
                                (color.b - 0.06).max(0.0),
                            );
                        }
                        _ => {}
                    }
                    iced::widget::button::Style {
                        background: Some(iced::Background::Color(bg_color)),
                        text_color: Color::WHITE,
                        border: iced::Border {
                            radius: 6.0.into(),
                            ..Default::default()
                        },
                        shadow: Default::default(),
                        snap: true,
                    }
                })
        };

        let btn_c = if self.is_running { red } else { accent };
        let auto_c = if self.config.is_auto_start() {
            accent
        } else {
            grey
        };

        let db = self.last_level_db.clamp(-60.0, 0.0);
        let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);

        // 提示行：错误红色、普通信息蓝灰（级别由写入方显式声明）。
        // 无提示时整块只有 6px 间隔：320×290 的方形窗口容不下常驻空行。
        let notice_block: Element<'_, Message> = if self.notice.is_empty() {
            Space::new().height(6).into()
        } else {
            column![
                Space::new().height(6),
                text(self.notice.clone()).size(11).color(
                    if self.notice_level == NoticeLevel::Error {
                        red
                    } else {
                        Color::from_rgb(0.62, 0.70, 0.78)
                    }
                ),
                Space::new().height(6),
            ]
            .into()
        };

        container(
            column![
                row![
                    text("ENJOY ME").size(20),
                    Space::new().width(Length::Fill),
                    text(st).color(sc).size(11),
                    Space::new().width(8),
                    text(vb_str).size(11).color(vb_color),
                    Space::new().width(8),
                    text(p2p_state).size(11).color(p2p_color),
                ]
                .align_y(Alignment::Center),
                Space::new().height(8.0),
                container(column![
                    text(if self.is_running {
                        "监听端口"
                    } else {
                        "目标地址"
                    })
                    .size(14)
                    .color(dim),
                    Space::new().height(4),
                    if self.is_running {
                        let addr = format!("{}:{}", self.config.listen_ip, self.config.listen_port);
                        row![text(addr).size(14).color(Color::WHITE)]
                            .height(32)
                            .align_y(Alignment::Center)
                    } else {
                        // 与运行态分支的 .height(32) 对齐：
                        // text_input 的固有高度 = 字号16 × 行高1.3 + padding(5+5) = 30.8px，
                        // 不锁高的话启停切换时这张卡片会有 1.2px 的高度跳变，把下面整列推开。
                        row![
                            text_input("0.0.0.0", &self.ip_input)
                                .on_input(Message::IpChanged)
                                .width(148)
                                .padding([5, 8])
                                .style(move |_, status| {
                                    let base = iced::widget::text_input::default(
                                        &iced::theme::Theme::Dark,
                                        status,
                                    );
                                    iced::widget::text_input::Style {
                                        background: iced::Background::Color(Color::from_rgb(
                                            0.18, 0.18, 0.22,
                                        )),
                                        border: iced::Border {
                                            radius: 5.0.into(),
                                            color: Color::from_rgb(0.25, 0.25, 0.30),
                                            width: 1.0,
                                        },
                                        ..base
                                    }
                                }),
                            text(" : ").size(16).color(dim),
                            text_input("44044", &self.port_input)
                                .on_input(Message::PortChanged)
                                .width(68)
                                .padding([5, 8])
                                .style(move |_, status| {
                                    let base = iced::widget::text_input::default(
                                        &iced::theme::Theme::Dark,
                                        status,
                                    );
                                    iced::widget::text_input::Style {
                                        background: iced::Background::Color(Color::from_rgb(
                                            0.18, 0.18, 0.22,
                                        )),
                                        border: iced::Border {
                                            radius: 5.0.into(),
                                            color: if self.port_valid {
                                                Color::from_rgb(0.25, 0.25, 0.30)
                                            } else {
                                                red
                                            },
                                            width: 1.0,
                                        },
                                        ..base
                                    }
                                }),
                        ]
                        .height(32)
                        .align_y(Alignment::Center)
                    },
                ])
                .padding(10)
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(iced::Background::Color(card_bg)),
                    border: iced::Border {
                        radius: 8.0.into(),
                        color: border_color,
                        width: 1.0
                    },
                    ..Default::default()
                }),
                Space::new().height(8.0),
                container(column![
                    row![
                        column![
                            text("码率").size(12).color(dim),
                            Space::new().height(2),
                            row![
                                text(format!("{:.0}", self.last_bitrate as u32)).size(22),
                                Space::new().width(3),
                                text("kbps").size(11).color(dim)
                            ]
                            .align_y(Alignment::End)
                        ]
                        .width(Length::FillPortion(1)),
                        column![
                            text("电平").size(12).color(dim),
                            Space::new().height(2),
                            row![
                                text(format!("{:.0}", self.last_level_db as i32)).size(22),
                                Space::new().width(3),
                                text("dB").size(11).color(dim)
                            ]
                            .align_y(Alignment::End)
                        ]
                        .width(Length::FillPortion(1)),
                    ],
                    Space::new().height(6),
                    progress_bar(0.0..=1.0, frac).girth(3).style(move |_| {
                        let bar_color = if db > -10.0 {
                            red
                        } else if db > -25.0 {
                            Color::from_rgb(0.90, 0.55, 0.15)
                        } else {
                            accent
                        };
                        iced::widget::progress_bar::Style {
                            background: iced::Background::Color(Color::from_rgb(0.18, 0.18, 0.22)),
                            bar: iced::Background::Color(bar_color),
                            border: iced::Border {
                                radius: 2.0.into(),
                                ..Default::default()
                            },
                        }
                    }),
                ])
                .padding(10)
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(iced::Background::Color(card_bg)),
                    border: iced::Border {
                        radius: 8.0.into(),
                        color: border_color,
                        width: 1.0
                    },
                    ..Default::default()
                }),
                Space::new().height(15),
                row![
                    mk(
                        if self.is_running { "停止" } else { "启动" },
                        btn_c,
                        Message::ToggleRunning,
                    )
                    .width(Length::FillPortion(1)),
                    Space::new().width(8),
                    mk(
                        "开机自启",
                        auto_c,
                        Message::ToggleAutoStart(!self.config.is_auto_start()),
                    )
                    .width(Length::FillPortion(1)),
                    Space::new().width(8),
                    if self.is_running {
                        let rev_on = self.reverse_enabled;
                        button(text(if rev_on { "反向串流" } else { "未启用" }).size(16))
                            .width(Length::FillPortion(1))
                            .height(34)
                            .style(move |_, _| iced::widget::button::Style {
                                background: Some(iced::Background::Color(if rev_on {
                                    Color::from_rgb(0.05, 0.40, 0.25)
                                } else {
                                    Color::from_rgb(0.15, 0.15, 0.18)
                                })),
                                text_color: if rev_on { accent } else { dim },
                                border: iced::Border {
                                    radius: 6.0.into(),
                                    ..Default::default()
                                },
                                shadow: Default::default(),
                                snap: true,
                            })
                    } else {
                        mk(
                            "反向串流",
                            if self.reverse_enabled { accent } else { grey },
                            Message::ToggleReverse(!self.reverse_enabled),
                        )
                    },
                ],
                // ── 提示行：只承载错误与警告（连接状态由顶部状态点负责）──
                notice_block,
                // ── 底部状态行：单行，按连接状态切换内容 ──
                row![
                    text(footer_left).size(10).color(grey),
                    Space::new().width(Length::Fill),
                    text(footer_right).size(10).color(footer_right_color),
                ]
                .align_y(Alignment::Center),
            ]
            .padding(14),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(iced::Background::Color(bg)),
            ..Default::default()
        })
        .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let close_events = iced::event::listen_with(|event, _status, _id| {
            if let iced::Event::Window(window_event) = event {
                if matches!(window_event, iced::window::Event::CloseRequested) {
                    Some(Message::HideWindow)
                } else {
                    None
                }
            } else {
                None
            }
        });

        let mut subs = vec![
            close_events,
            Subscription::run_with((), |_| tray_event_stream()),
        ];

        if self.is_running {
            let ip = self.config.listen_ip.clone();
            let port = self.config.listen_port;
            subs.push(Subscription::run_with((ip, port), |data| {
                udp_receiver_stream(data.0.clone(), data.1)
            }));
        }

        Subscription::batch(subs)
    }
}

fn latest_window_task<F>(mut operation: F) -> Task<Message>
where
    F: FnMut(iced::window::Id) -> Task<Message> + Send + 'static,
{
    iced::window::latest().then(move |id| match id {
        Some(id) => operation(id),
        None => Task::none(),
    })
}

/// 创建系统托盘。失败（explorer 未就绪、远程会话、资源不足）返回 None，
/// 由调用方降级为"关窗即退出"，绝不能 panic：发布版没有控制台，
/// panic 的表现就是"双击图标毫无反应"。
fn install_tray(icon_rgba: Vec<u8>, width: u32, height: u32) -> Option<tray_icon::TrayIcon> {
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let _ = TRAY_COMMANDS.set(tokio::sync::Mutex::new(receiver));

    let restore_sender = sender.clone();
    tray_icon::TrayIconEvent::set_event_handler(Some(move |event| {
        if matches!(event, tray_icon::TrayIconEvent::DoubleClick { .. }) {
            let _ = restore_sender.try_send(TrayCommand::Restore);
        }
    }));
    tray_icon::menu::MenuEvent::set_event_handler(Some(
        move |event: tray_icon::menu::MenuEvent| {
            if event.id == "quit" {
                let _ = sender.try_send(TrayCommand::Quit);
            }
        },
    ));

    let tray_menu = tray_icon::menu::Menu::new();
    let quit_item = tray_icon::menu::MenuItemBuilder::new()
        .id("quit".into())
        .text("退出")
        .enabled(true)
        .build();
    let _ = tray_menu.append(&quit_item);

    let tray_icon_img = match tray_icon::Icon::from_rgba(icon_rgba, width, height) {
        Ok(icon) => icon,
        Err(e) => {
            logf!("[Tray] 图标构造失败: {e}");
            return None;
        }
    };
    match tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("UDP2CAL")
        .with_icon(tray_icon_img)
        .with_menu_on_left_click(false)
        .build()
    {
        Ok(tray) => Some(tray),
        Err(e) => {
            logf!("[Tray] 托盘创建失败: {e}");
            None
        }
    }
}

/// 单实例：按窗口标题激活已运行的实例
fn focus_existing_window(title: &str) {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` 以 0 结尾，指针在调用期间有效；FindWindowW 失败时返回
    // Err，不产生悬垂引用。
    unsafe {
        if let Ok(hwnd) = FindWindowW(PCWSTR::null(), PCWSTR::from_raw(wide.as_ptr())) {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

fn restore_native_window() {
    let mut search = WindowSearch {
        process_id: std::process::id(),
        window: HWND(std::ptr::null_mut()),
    };
    // SAFETY: `LPARAM` 携带 `&mut search` 的地址，`EnumWindows` 是同步调用，
    // 回调期间 `search` 保留独占可变借用且一直存活，不存在别名或悬垂。
    unsafe {
        let _ = EnumWindows(
            Some(find_window_for_process),
            LPARAM((&mut search as *mut WindowSearch) as isize),
        );
        if !search.window.0.is_null() {
            let _ = ShowWindow(search.window, SW_RESTORE);
            let _ = SetForegroundWindow(search.window);
        }
    }
}

struct WindowSearch {
    process_id: u32,
    window: HWND,
}

/// 只接受"可见 + 有标题"的顶层窗口。
/// 历史上命中枚举到的第一个同进程窗口即可，可能选到不可见的辅助窗口，
/// 导致托盘双击"没反应"。
///
/// # Safety
/// `data` 必须是由 [`restore_native_window`] 传入的 `&mut WindowSearch` 地址；
/// 回调是同步的，指针在该调用期间有效且不与其它借用冲突。
unsafe extern "system" fn find_window_for_process(hwnd: HWND, data: LPARAM) -> BOOL {
    // SAFETY: 见上，`data` 来自 restore_native_window 的 `&mut search`。
    let search = &mut *(data.0 as *mut WindowSearch);
    let mut process_id = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    if process_id != search.process_id {
        return BOOL(1);
    }
    if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) == 0 {
        return BOOL(1);
    }
    search.window = hwnd;
    BOOL(0)
}

fn tray_event_stream() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(
        10,
        |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            let Some(commands) = TRAY_COMMANDS.get() else {
                return;
            };
            let mut commands = commands.lock().await;
            while let Some(command) = commands.recv().await {
                let message = match command {
                    TrayCommand::Restore => {
                        restore_native_window();
                        Message::ShowWindow
                    }
                    TrayCommand::Quit => Message::Quit,
                };
                if output.send(message).await.is_err() {
                    break;
                }
            }
        },
    )
}

/// 发送 CONNECT_ACK（10 字节负载：reverse_ssrc + PT + frame_ms + forward_ssrc）。
/// 复用监听 socket 发送：ACK 源端口恒为服务端口，便于对端归属判断。
async fn send_connect_ack(
    socket: &tokio::net::UdpSocket,
    src_addr: &SocketAddr,
    reverse_ssrc: u32,
    forward_ssrc: u32,
) {
    let payload = protocol::ConnectAckPayload {
        reverse_ssrc,
        forward_ssrc,
        ..protocol::ConnectAckPayload::default()
    }
    .encode();
    let packet = protocol::build_control(protocol::TYPE_CONNECT_ACK, &my_device_id(), &payload);
    let _ = socket.send_to(&packet, src_addr).await;
}

/// 本次绑定应使用的反向 SSRC。
/// 反向关闭（用户开关关闭或 `reverse_port == 0`）时返回 0，表示不启动反向发送。
fn desired_reverse_ssrc(reverse_port: u16, previous: u32, regenerate: bool) -> u32 {
    if reverse_port == 0 || REVERSE_ENABLED.load(Ordering::Acquire) == 0 {
        return 0;
    }
    if previous != 0 && !regenerate {
        return previous;
    }
    random_u32_nonzero()
}

/// 启动/重启反向发送器：先停旧的，保证同一时刻只有一个线程持有环回端点。
fn restart_reverse_sender(addr: SocketAddr, ssrc: u32) {
    // 中毒恢复：槽里只有 Option<ReverseSender>，取回内部值继续操作；
    // 若因中毒直接跳过，旧发送线程会继续跑而日志一无所知。
    let serial = REVERSE_RESTART.lock().unwrap_or_else(|e| e.into_inner());
    stop_reverse_sender_under(&serial);
    if ssrc != 0 {
        let sender = capture::ReverseSender::start(addr, ssrc);
        *reverse_sender().lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
    }
}

/// 停止反向发送器（`stop()` 的 `join` 在槽位锁之外完成）。
fn stop_reverse_sender() {
    let serial = REVERSE_RESTART.lock().unwrap_or_else(|e| e.into_inner());
    stop_reverse_sender_under(&serial);
}

/// 取出槽位并 `stop()`。调用方必须已持有 `REVERSE_RESTART`
/// （维持“同一时刻只有一个发送线程”的不变量），但**不得**同时持有槽位锁：
/// `stop()` 会 `join` 发送线程，而该线程的启动阶段（`CoInitializeEx` +
/// 新建环回端点 + 新建 Opus 编码器）可长达数百毫秒。持有槽位锁做 join 会：
/// - 阻塞持有唯一 UDP socket 的 async 接收任务（期间不读数据报）；
/// - 阻塞 UI 线程（`reset_to_ready` 路径），整个窗口冻结；
/// - 把每次 keepalive 都要抢同一把锁的 `reverse_sender_alive()` 一起串住。
fn stop_reverse_sender_under(_serial: &std::sync::MutexGuard<'static, ()>) {
    // 临时守卫在语句结束即释放，`stop()` 在锁外执行。
    let old = reverse_sender()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(old) = old {
        old.stop();
    }
}

/// 处理一条控制消息。
///
/// 校验顺序（规范 5.7 第 5–7 步）：类型 → 负载布局 → 绑定关系。
/// 任何一步失败都静默丢弃：控制面不回显错误，也不修改绑定状态。
async fn handle_control(
    header: &protocol::ControlHeader,
    payload: &[u8],
    src_addr: &SocketAddr,
    socket: &tokio::net::UdpSocket,
    output: &mut iced::futures::channel::mpsc::Sender<Message>,
) {
    // 0（已废弃）与 5..=255（保留）都不是本端要处理的消息
    if !is_valid_control_header(header) || header.msg_type != protocol::TYPE_CONNECT {
        return;
    }

    let Some(cfg) = protocol::ConnectPayload::decode(payload) else {
        if should_log_control_reject() {
            logf!("[P2P] CONNECT 负载长度非法（{} 字节），丢弃", payload.len());
        }
        return;
    };
    if let Err(reason) = cfg.validate() {
        if should_log_control_reject() {
            logf!("[P2P] CONNECT 参数非法（{reason}），丢弃");
        }
        return;
    }
    let now = Instant::now();
    let peer = p2p_snapshot();
    let same_device = peer.is_busy() && peer.device_id == header.device_id;

    // v3 device_id 是公开标识，不是认证凭据。绑定期间固定完整源端点；
    // 未认证的地址/端口变化不能迁移绑定或刷新保活。合法网络切换需等待超时后重连。
    if same_device && !is_bound_peer_source(&peer, header.device_id, *src_addr) {
        if should_log_control_reject() {
            logf!("[P2P] 已绑定设备从新端点发起 CONNECT，拒绝未认证的端点迁移");
        }
        return;
    }

    // ── 已被其它设备占用：静默忽略，不回 ACK ──
    // 注意：反向编码器配置的更新必须放在这道门之后。否则同网段任意主机
    // 发一个参数合法的 CONNECT 就能改写当前会话的反向编码参数（码率/DTX/FEC），
    // 属于最小权限缺陷（规范 5.7 第 7 步要求先校验绑定关系）。
    if peer.is_busy() && !same_device {
        return;
    }

    // 反向编码器配置只描述 Windows → Android；内容相同则不下发（避免每秒重置编码器）
    capture::update_reverse_config(capture::OpusReverseConfig::from(&cfg));

    if !same_device {
        // ── 建立绑定 ──
        let reverse_ssrc = desired_reverse_ssrc(cfg.reverse_port, 0, true);
        {
            let mut st = p2p_lock();
            st.state = DEVICE_BUSY;
            st.device_id = header.device_id;
            st.addr = Some(*src_addr);
            st.forward_ssrc = cfg.forward_ssrc;
            st.reverse_ssrc = reverse_ssrc;
            st.reverse_port = cfg.reverse_port;
            st.last_connect = Some(now);
        }
        // 新会话：抖动缓冲与解码器状态全部重建
        jitter().lock().unwrap_or_else(|e| e.into_inner()).reset();
        wake_decoder();
        restart_reverse_sender(
            SocketAddr::new(src_addr.ip(), cfg.reverse_port.max(1)),
            reverse_ssrc,
        );
        if should_log_control_event() {
            logf!(
                "[P2P] 手机端 {} 已绑定（forward_ssrc={:#010x} reverse_ssrc={:#010x} reverse_port={}）",
                src_addr.ip(),
                cfg.forward_ssrc,
                reverse_ssrc,
                cfg.reverse_port
            );
        }
        let _ = output.try_send(Message::PeerStateChanged);
        send_connect_ack(socket, src_addr, reverse_ssrc, cfg.forward_ssrc).await;
        return;
    }

    // ── 同一设备的周期 CONNECT（保活 / 参数同步）──
    // forward_ssrc 变化表示新会话：必须重置抖动缓冲/解码器并换反向 SSRC
    let new_session = peer.forward_ssrc != cfg.forward_ssrc;
    let want_reverse = cfg.reverse_port != 0 && REVERSE_ENABLED.load(Ordering::Acquire) != 0;
    let reverse_stale = new_session
        || peer.reverse_port != cfg.reverse_port
        || (peer.reverse_ssrc != 0) != want_reverse;
    // 槽位里还有 ReverseSender 但线程已经结束：必须重启，否则保活一直显示健康。
    // 这种情况不换 SSRC（换号会让 Android 无谓地重建解码器/播放缓冲）。
    let sender_dead = want_reverse && peer.reverse_ssrc != 0 && !reverse_sender_alive();
    let need_restart = reverse_stale || sender_dead;

    let reverse_ssrc = if reverse_stale {
        desired_reverse_ssrc(cfg.reverse_port, peer.reverse_ssrc, true)
    } else {
        peer.reverse_ssrc
    };

    {
        let mut st = p2p_lock();
        st.addr = Some(*src_addr);
        st.forward_ssrc = cfg.forward_ssrc;
        st.reverse_ssrc = reverse_ssrc;
        st.reverse_port = cfg.reverse_port;
        st.last_connect = Some(now);
    }

    if new_session {
        jitter().lock().unwrap_or_else(|e| e.into_inner()).reset();
        wake_decoder();
        if should_log_control_event() {
            logf!("[P2P] forward_ssrc 变化 → 新会话，已重置抖动缓冲");
        }
    }
    if need_restart {
        restart_reverse_sender(
            SocketAddr::new(src_addr.ip(), cfg.reverse_port.max(1)),
            reverse_ssrc,
        );
        if should_log_control_event() {
            if reverse_stale {
                logf!(
                    "[P2P] 反向发送更新：ssrc={:#010x} port={}",
                    reverse_ssrc,
                    cfg.reverse_port
                );
            } else {
                logf!(
                    "[P2P] 反向发送线程已退出，已重启：ssrc={:#010x} port={}",
                    reverse_ssrc,
                    cfg.reverse_port
                );
            }
        }
    }

    send_connect_ack(socket, src_addr, reverse_ssrc, cfg.forward_ssrc).await;
}

/// 控制面超时：绑定设备 4.5 s 没有 CONNECT → 释放绑定、停止反向、回到 Ready。
/// 只依据控制面保活判断（DTX 静音期间可能长时间没有 RTP）。
fn check_peer_timeout() -> bool {
    let st = p2p_lock();
    if !st.is_busy() {
        return false;
    }
    let expired = st
        .last_connect
        .map(|t| t.elapsed() > Duration::from_millis(protocol::PEER_TIMEOUT_MS))
        .unwrap_or(true);
    if expired {
        drop(st);
        reset_to_ready();
        true
    } else {
        false
    }
}

fn udp_receiver_stream(
    listen_ip: String,
    listen_port: u32,
) -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(
        64,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            let bind_addr = format!("{}:{}", listen_ip, listen_port);
            // 用 `(host, port)` 元组解析，而不是把上面那个字符串拿去做 bind：
            // `format!("{}:{}")` 对 IPv6 会拼出 "::1:44044"（缺方括号，非法
            // socket 地址），而 UI 是接受 IPv6 监听地址的（main.rs 的校验
            // 只拦非法字符）。`ToSocketAddrs` 对 IPv4/IPv6/主机名三种形态
            // 都正确，`bind_addr` 只用于报错展示。
            // 端口已是 1..=65535（config::load 归一 + UI 校验），`as u16` 安全。
            let socket =
                match tokio::net::UdpSocket::bind((listen_ip.as_str(), listen_port as u16)).await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = output
                            .send(Message::EngineError(format!(
                                "监听 {bind_addr} 失败：{}（端口被占用或地址不可用）",
                                e.kind()
                            )))
                            .await;
                        return;
                    }
                };

            // 接收缓冲（借鉴 Sunshine 的 socket 调优）：把内核接收队列放大到
            // 1 MiB。Windows 默认只有 64 KiB 量级，Wi-Fi 一次漫游/重传退避就
            // 可能把突发直接丢掉；这里放大的是**内核队列**而不是应用层积压
            // ——循环每轮立即 recv，队列只用来吸收空口级突发，不引入延迟。
            const RECV_BUFFER_BYTES: usize = 1 << 20;
            if let Err(e) = socket2::SockRef::from(&socket).set_recv_buffer_size(RECV_BUFFER_BYTES)
            {
                logf!("[Net] 接收缓冲设置失败（{e}），继续使用系统默认值");
            }

            // 等待音频初始化完成（最多 3 秒；冷启动时音频服务可能较慢）
            let mut retries = 60;
            while AudioInitState::load() == AudioInitState::Pending && retries > 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                retries -= 1;
            }

            match AudioInitState::load() {
                AudioInitState::DecoderFailed => {
                    let _ = output
                        .send(Message::EngineError("Opus 解码器初始化失败".into()))
                        .await;
                    return;
                }
                AudioInitState::DeviceFailed => {
                    let _ = output
                        .send(Message::EngineError(
                            "音频设备初始化失败（请确认 VB-Cable 已安装后重启程序）".into(),
                        ))
                        .await;
                    return;
                }
                _ => {}
            }

            reset_to_ready();
            wake_decoder();

            let mut buf = vec![0u8; 2048];
            let mut last_stats = Instant::now();
            let mut last_liveness = Instant::now();
            let mut byte_count: u64 = 0;
            // 最近一次通过校验的 RTP 包到达时刻（空闲恢复判据，见 RTP_IDLE_RESET_MS）
            let mut last_rtp_at: Option<Instant> = None;

            loop {
                // 控制面保活检查必须**与流量无关**地周期性执行（规范 6.4：保活只看
                // 控制面，不得用"是否收到 RTP"判断连接存活）。历史实现把它放在
                // recv 超时分支里：只要 RTP 持续到达，检查就永不执行，绑定永不释放。
                if last_liveness.elapsed() >= Duration::from_millis(100) {
                    last_liveness = Instant::now();
                    if check_peer_timeout() {
                        let _ = output.send(Message::Disconnected).await;
                    }
                }

                let recv =
                    tokio::time::timeout(Duration::from_millis(200), socket.recv_from(&mut buf))
                        .await;

                let (len, src) = match recv {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => {
                        // Windows 上 UDP socket 会把上一次 send_to 收到的 ICMP 端口不可达
                        // 回灌成一次 WSAECONNRESET。这类错误不是致命的，但绝对不能
                        // 立刻重试——那会变成 100% CPU 的忙等。睡 10 ms 退避。
                        if should_log_control_reject() {
                            logf!("[UDP] recv_from 失败：{e}");
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    }
                    // 超时：下一轮继续做保活检查
                    Err(_) => continue,
                };

                match protocol::demux(&buf[..len]) {
                    protocol::Datagram::Control(data) => {
                        if let Some((hdr, payload)) = protocol::parse_control(data) {
                            handle_control(&hdr, payload, &src, &socket, &mut output).await;
                        }
                    }
                    protocol::Datagram::Rtp(data) => {
                        let peer = p2p_snapshot();
                        // 未绑定 / 源地址不符：一律丢弃（含绑定前后的迟到包）
                        if !peer.is_busy() || peer.addr != Some(src) {
                            RTP_RX_DROPPED.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let Ok(pkt) = protocol::rtp::parse(data) else {
                            RTP_RX_DROPPED.fetch_add(1, Ordering::Relaxed);
                            continue;
                        };
                        if pkt.ssrc != peer.forward_ssrc {
                            RTP_RX_DROPPED.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        // 网络线程只插入，不解码、不阻塞等待播放
                        // 空闲恢复：长时间无包后，本地已把播放头推到远超前于发送端，
                        // 必须先 reset 再按首包重新起播（否则恢复包全被当 late 丢弃）。
                        let idle_too_long = last_rtp_at
                            .map(|at| at.elapsed().as_millis() > RTP_IDLE_RESET_MS as u128)
                            .unwrap_or(false);
                        let accepted = {
                            let mut jb = jitter().lock().unwrap_or_else(|e| e.into_inner());
                            if idle_too_long {
                                jb.reset();
                            }
                            jb.insert(&pkt) != protocol::rtp::InsertResult::Dropped
                        };
                        if idle_too_long {
                            logf!(
                                "[RTP] 空闲超过 {}ms，已重置抖动缓冲并按首包重新起播",
                                RTP_IDLE_RESET_MS
                            );
                        }
                        last_rtp_at = Some(Instant::now());
                        if accepted {
                            byte_count += len as u64;
                            RTP_RX_PACKETS.fetch_add(1, Ordering::Relaxed);
                            RTP_RX_BYTES.fetch_add(len as u64, Ordering::Relaxed);
                        }
                        wake_decoder();
                    }
                    protocol::Datagram::Drop => {
                        RTP_RX_DROPPED.fetch_add(1, Ordering::Relaxed);
                    }
                }

                if last_stats.elapsed() > Duration::from_millis(500) {
                    let elapsed = last_stats.elapsed().as_secs_f32();
                    let kbps = if elapsed > 0.0 {
                        (byte_count * 8) as f32 / elapsed / 1000.0
                    } else {
                        0.0
                    };
                    let level_db = f32::from_bits(AUDIO_LEVEL_DB.load(Ordering::Relaxed));
                    // 半秒统计窗口结束：窗口字节数无条件归零（没有 RTP 时下个窗口
                    // 从 0 重新累计，DTX 静音也走这里；包计数不需要单独保存，
                    // 全局的 RTP_RX_PACKETS 已经在统计同一件事）。
                    byte_count = 0;
                    last_stats = Instant::now();
                    let _ = output
                        .send(Message::StatusUpdate(StatusInfo {
                            bitrate_kbps: kbps,
                            level_db,
                        }))
                        .await;
                }
            }
        },
    )
}
