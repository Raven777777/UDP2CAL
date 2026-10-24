// UDP2CAL Windows 接收端 - 局域网音频串流
// 双状态机架构 + P2P 独占通信系统
#![windows_subsystem = "windows"] 

mod audio; 
mod capture; 
mod config; 
mod decoder; 
mod firewall; 
mod logger; 
mod protocol; 

use iced::widget::{button, column, container, progress_bar, row, text, text_input, Space}; 
use iced::{Alignment, Element, Length, Subscription, Task, Theme, Color}; 
use iced::futures::SinkExt; 
use std::net::SocketAddr; 
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering}; 
use std::sync::mpsc::{sync_channel, SyncSender}; 
use std::sync::{Arc, Mutex, OnceLock}; 
use std::time::{Duration, Instant}; 
use windows::Win32::System::Threading::CreateMutexW; 
use windows::Win32::Foundation::{BOOL, GetLastError, HWND, LPARAM, ERROR_ALREADY_EXISTS};
use windows::Win32::UI::WindowsAndMessaging::{ 
    EnumWindows, FindWindowW, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
    MessageBoxW, SetForegroundWindow, ShowWindow, MB_OK,
    MB_ICONINFORMATION, SW_RESTORE,
}; 
use windows::core::PCWSTR;

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
// P2P 独占通信 - 全局共享状态
// ══════════════════════════════════════════════

const DEVICE_READY: u8 = 0;
const DEVICE_BUSY: u8 = 1;

/// P2P 独占状态。
/// 历史实现把"状态"和"绑定 ID"放在两个独立全局里分别用 Relaxed 访问，
/// 二者构成的复合不变式没有任何顺序保证，可能读到 "BUSY + 旧 ID"。
/// 现在合并进一把锁，状态与 ID 永远同时可见。
#[derive(Clone, Copy)]
struct P2pState {
    state: u8,
    device_id: [u8; protocol::DEVICE_ID_SIZE],
}

impl P2pState {
    const fn new() -> Self {
        Self { state: DEVICE_READY, device_id: [0u8; protocol::DEVICE_ID_SIZE] }
    }
    fn is_busy(&self) -> bool {
        self.state == DEVICE_BUSY
    }
}

static P2P_STATE: OnceLock<Mutex<P2pState>> = OnceLock::new();

fn p2p_lock() -> std::sync::MutexGuard<'static, P2pState> {
    P2P_STATE
        .get_or_init(|| Mutex::new(P2pState::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn p2p_snapshot() -> P2pState {
    *p2p_lock()
}

/// UI 是否已启动（停止时禁止广播）
static APP_RUNNING: AtomicU8 = AtomicU8::new(0);

/// Android 端地址和反向端口（用于反向音频发送）
static ANDROID_ADDR: OnceLock<Mutex<Option<(SocketAddr, u16)>>> = OnceLock::new();
fn android_addr() -> &'static Mutex<Option<(SocketAddr, u16)>> {
    ANDROID_ADDR.get_or_init(|| Mutex::new(None))
}

/// 反向串流用户开关（全局，供 handle_control 读取）
static REVERSE_ENABLED: AtomicU8 = AtomicU8::new(0);

/// 反向音频发送器句柄
static REVERSE_SENDER: OnceLock<Mutex<Option<crate::capture::ReverseSender>>> = OnceLock::new();
fn reverse_sender() -> &'static Mutex<Option<crate::capture::ReverseSender>> {
    REVERSE_SENDER.get_or_init(|| Mutex::new(None))
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
// 全局音频守护线程架构
// ══════════════════════════════════════════════

enum AudioMessage { 
    Packet { seq_num: u8, sample_rate: u8, payload: Vec<u8> }, 
    Reset, 
} 

static AUDIO_TX: OnceLock<Mutex<Option<SyncSender<AudioMessage>>>> = OnceLock::new();
static AUDIO_LEVEL_DB: AtomicU32 = AtomicU32::new((-60.0f32).to_bits()); 

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

/// 音频诊断计数器（进程级单例，UI 展示 + 落盘日志共用）
static AUDIO_STATS: OnceLock<Arc<audio::AudioStats>> = OnceLock::new();
fn audio_stats() -> &'static Arc<audio::AudioStats> {
    AUDIO_STATS.get_or_init(|| Arc::new(audio::AudioStats::default()))
}

fn audio_tx() -> &'static Mutex<Option<SyncSender<AudioMessage>>> {
    AUDIO_TX.get_or_init(|| Mutex::new(None))
}

/// 投递音频消息；通道不可用或已满时静默丢弃（低延迟优先）
fn send_audio(message: AudioMessage) {
    if let Ok(slot) = audio_tx().lock() {
        if let Some(tx) = slot.as_ref() {
            let _ = tx.try_send(message);
        }
    }
}

// ══════════════════════════════════════════════
// 接收路径载荷缓冲池
// 历史实现每包 `buf[..].to_vec()`，即 100 次/秒的小对象分配。这里复用一组
// 固定容量的缓冲：接收侧取出、音频线程用完归还。池有界（64 个），拿不到就新分配。
// 注意：锁只出现在"网络 → 音频线程"的交接处，cpal 回调路径仍然完全无锁。
// ══════════════════════════════════════════════

const PAYLOAD_POOL_MAX: usize = 64;

static PAYLOAD_POOL: OnceLock<Mutex<Vec<Vec<u8>>>> = OnceLock::new();
fn payload_pool() -> &'static Mutex<Vec<Vec<u8>>> {
    PAYLOAD_POOL.get_or_init(|| Mutex::new(Vec::with_capacity(PAYLOAD_POOL_MAX)))
}

fn take_payload() -> Vec<u8> {
    payload_pool()
        .lock()
        .ok()
        .and_then(|mut pool| pool.pop())
        .unwrap_or_else(|| Vec::with_capacity(protocol::MAX_PAYLOAD))
}

fn recycle_payload(mut buf: Vec<u8>) {
    if buf.capacity() < protocol::MAX_PAYLOAD {
        return; // 容量不足的缓冲不值得复用
    }
    if let Ok(mut pool) = payload_pool().lock() {
        if pool.len() < PAYLOAD_POOL_MAX {
            buf.clear();
            pool.push(buf);
        }
    }
}

/// 启动（或重启）音频守护线程。
/// 允许在初始化失败后再次调用：旧线程已退出，通道被替换后其接收端会关闭。
fn init_audio_worker() { 
    let (tx, rx) = sync_channel::<AudioMessage>(50); 
    if let Ok(mut slot) = audio_tx().lock() {
        *slot = Some(tx);
    }
    AudioInitState::Pending.store();

    let stats = audio_stats().clone();
    let _ = std::thread::Builder::new()
        .name("udp2cal-worker".into())
        .spawn(move || { 
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
        AudioInitState::Ready.store(); 
        logf!("[Audio] 就绪：输出设备 {} Hz", aw.device_rate);

        let mut rb = protocol::ReorderBuffer::new();
        let mut rms_sum: f64 = 0.0;
        let mut rms_count: u64 = 0;
        let mut last_stats = Instant::now();
        // ── 丢包隐藏（PLC）状态 ──
        // 协议帧长 10ms；连续 2 帧未收到包视为出现缺口，按缺口帧数请求 PLC 补齐。
        // 上限 5 帧：长时间静默（发送端 DTX / 断流）不做无限补偿，2 秒后彻底静音。
        const FRAME_MS: u64 = 10;
        const PLC_MAX_FRAMES: u32 = 5;
        const PLC_STREAM_IDLE_MS: u64 = 2000;
        let mut last_audio: Option<Instant> = None;

        loop {
            match rx.recv_timeout(Duration::from_millis(30)) {
                Ok(AudioMessage::Reset) => {
                    rb = protocol::ReorderBuffer::new();
                    rms_sum = 0.0;
                    rms_count = 0;
                    last_audio = None;
                    AUDIO_LEVEL_DB.store((-60.0f32).to_bits(), Ordering::Relaxed);
                }
                Ok(AudioMessage::Packet { seq_num, sample_rate, payload }) => {
                    last_audio = Some(Instant::now());
                    rb.insert_and_drain(seq_num, sample_rate, &payload, |sr, p| {
                        if let Some((n, sr_hz)) = dec.decode(sr, p) {
                            let pcm = dec.pcm_data();
                            let samples = &pcm[..n.min(pcm.len())];
                            rms_sum += samples.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>();
                            rms_count += samples.len() as u64;
                            aw.write(samples, sr_hz);
                        }
                    });
                    recycle_payload(payload);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let Some(t) = last_audio else { continue };
                    let elapsed = t.elapsed();
                    // 仍在活跃流内但出现帧缺口 → 逐帧 PLC（Deep PLC 在此生效）
                    if elapsed > Duration::from_millis(FRAME_MS * 2)
                        && elapsed < Duration::from_millis(PLC_STREAM_IDLE_MS)
                    {
                        let missed = ((elapsed.as_millis() as u64 / FRAME_MS) as u32)
                            .min(PLC_MAX_FRAMES);
                        for _ in 0..missed {
                            if let Some((n, sr_hz)) = dec.conceal() {
                                let pcm = dec.pcm_data();
                                let samples = &pcm[..n.min(pcm.len())];
                                rms_sum += samples.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>();
                                rms_count += samples.len() as u64;
                                aw.write(samples, sr_hz);
                            }
                        }
                        last_audio = Some(Instant::now());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
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
        } 
    }); 
} 

// ══════════════════════════════════════════════
// 广播状态机 (独立线程)
// ══════════════════════════════════════════════
// Ready 模式: 每1秒广播 TYPE_DISCOVER_REPLY 到 LAN
// Silent 模式: APP 停止或 DEVICE_BUSY 时停止广播

/// 当前监听端口（缓存，避免广播线程每秒读注册表）
static LISTEN_PORT: AtomicU32 = AtomicU32::new(44044);

fn start_broadcast_state_machine() {
    std::thread::spawn(move || {
        let socket = match std::net::UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = socket.set_broadcast(true);
        let device_id = my_device_id();
        let my_name = my_device_name();
        loop {
            if APP_RUNNING.load(Ordering::Acquire) != 0 && !p2p_snapshot().is_busy() {
                let port = LISTEN_PORT.load(Ordering::Acquire) as u16;
                // 独立 payload 缓冲区避免与 pkt_buf 可变借用冲突
                let mut payload_buf = [0u8; 256];
                let name_bytes = my_name.as_bytes();
                let payload_len = 2 + name_bytes.len();
                if payload_len + protocol::HEADER_SIZE <= 1500 && payload_len <= payload_buf.len() {
                    let mut pkt_buf = [0u8; 1500];
                    payload_buf[..2].copy_from_slice(&port.to_be_bytes());
                    payload_buf[2..payload_len].copy_from_slice(name_bytes);
                    if let Some(total) = protocol::build_packet_to(
                        &mut pkt_buf, 0, false, protocol::TYPE_DISCOVER_REPLY, 0, 0,
                        &device_id, &payload_buf[..payload_len], 0,
                    ) {
                        let _ = socket.send_to(&pkt_buf[..total], "255.255.255.255:44043");
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// 重置全局状态到 DEVICE_READY
fn reset_to_ready() {
    {
        let mut st = p2p_lock();
        st.state = DEVICE_READY;
        st.device_id = [0u8; protocol::DEVICE_ID_SIZE];
    }
    // 清除 Android 地址并停止反向音频
    if let Ok(mut addr) = android_addr().lock() {
        *addr = None;
    }
    if let Ok(mut sender) = reverse_sender().lock() {
        if let Some(s) = sender.take() {
            s.stop();
        }
    }
}

// ══════════════════════════════════════════════
// v2 发现服务 (独立线程)
// ══════════════════════════════════════════════
pub fn start_broadcast_listener() { 
    std::thread::spawn(move || { 
        let socket = match std::net::UdpSocket::bind("0.0.0.0:44043") { 
            Ok(s) => s, 
            Err(e) => { 
                logf!("[Discovery] 绑定广播端口失败: {}", e); 
                return; 
            } 
        }; 
        let mut buf = [0u8; 1024]; 
        let my_id = my_device_id();
        let my_name = my_device_name();
        // 简单令牌桶：同一源 IP 每秒最多应答 MAX_PER_SEC 次。
        // 发现请求 15B、应答 ~15B+主机名，无限制时可被用于 UDP 反射放大。
        const MAX_PER_SEC: u32 = 5;
        let mut bucket: std::collections::HashMap<std::net::IpAddr, (u32, Instant)> =
            std::collections::HashMap::new();
        let mut last_cleanup = Instant::now();
        loop { 
            if let Ok((amt, src)) = socket.recv_from(&mut buf) { 
                    if last_cleanup.elapsed() > Duration::from_secs(30) {
                        bucket.retain(|_, (_, t)| t.elapsed() < Duration::from_secs(5));
                        last_cleanup = Instant::now();
                    }
                    {
                        let entry = bucket.entry(src.ip()).or_insert((0, Instant::now()));
                        if entry.1.elapsed() >= Duration::from_secs(1) {
                            *entry = (0, Instant::now());
                        }
                        if entry.0 >= MAX_PER_SEC {
                            continue;
                        }
                        entry.0 += 1;
                    }
                    let data = &buf[..amt];
                    if amt >= protocol::HEADER_SIZE {
                        let mut hdr_buf = [0u8; protocol::HEADER_SIZE];
                        hdr_buf.copy_from_slice(&data[..protocol::HEADER_SIZE]);
                        if let Some(hdr) = protocol::decode_header(&hdr_buf) {
                            if APP_RUNNING.load(Ordering::Acquire) != 0
                                && !p2p_snapshot().is_busy()
                                && hdr.msg_type == protocol::TYPE_DISCOVER_REQ {
                                let port = LISTEN_PORT.load(Ordering::Acquire) as u16;
                                // 独立 payload 缓冲区避免借用冲突
                                let mut payload_buf = [0u8; 256];
                                let name_bytes = my_name.as_bytes();
                                let payload_len = 2 + name_bytes.len();
                                if payload_len + protocol::HEADER_SIZE <= 1500 && payload_len <= payload_buf.len() {
                                    let mut pkt_buf = [0u8; 1500];
                                    payload_buf[..2].copy_from_slice(&port.to_be_bytes());
                                    payload_buf[2..payload_len].copy_from_slice(name_bytes);
                                    if let Some(total) = protocol::build_packet_to(
                                        &mut pkt_buf, 0, false, protocol::TYPE_DISCOVER_REPLY, 0, 0,
                                        &my_id, &payload_buf[..payload_len], 0,
                                    ) {
                                        let _ = socket.send_to(&pkt_buf[..total], src);
                                    }
                                }
                                continue;
                            }
                        }
                    }

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
    logf!("[App] 启动 v{}", env!("CARGO_PKG_VERSION"));
    if let Some(p) = logger::path() {
        logf!("[App] 日志文件: {}", p.display());
    }

    unsafe { 
        let name: Vec<u16> = "UDP2CAL_SingleInstance_Mutex\0".encode_utf16().collect(); 
        let _ = CreateMutexW(None, true, PCWSTR::from_raw(name.as_ptr())); 
        if GetLastError() == ERROR_ALREADY_EXISTS { 
            // 先尝试把已运行实例的窗口拉到前台；再提示，避免"双击没反应"
            focus_existing_window("UDP2CAL");
            let title: Vec<u16> = "UDP2CAL\0".encode_utf16().collect();
            let msg: Vec<u16> = "程序已在运行中（已尝试切换到已有窗口）\0".encode_utf16().collect();
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
    let result = iced::application(move || {
        let cfg = config::Config::load();
        REVERSE_ENABLED.store(if cfg.reverse_enabled != 0 { 1 } else { 0 }, Ordering::Release);
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
    }, AppState::update, AppState::view)
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
    fn update(&mut self, message: Message) -> Task<Message> { 
        match message { 
            Message::IpChanged(s) => { 
                self.ip_input = s; 
                Task::none() 
            } 
            Message::PortChanged(s) => { 
                self.port_input = s; 
                self.port_valid = self.port_input.parse::<u16>().is_ok() || self.port_input.is_empty(); 
                Task::none() 
            } 
            Message::ToggleAutoStart(enabled) => { 
                // 写入失败必须回滚开关并告知用户，避免"开关假亮"
                match self.config.set_auto_start(enabled) { 
                    Ok(()) => { 
                        let _ = self.config.save(); 
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
                if self.is_running { return Task::none(); }
                self.reverse_enabled = enabled;
                REVERSE_ENABLED.store(if enabled { 1 } else { 0 }, Ordering::Release);
                self.config.set_reverse_enabled(enabled);
                let _ = self.config.save();
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
                    let _ = self.config.save(); 
                    LISTEN_PORT.store(port as u32, Ordering::Release);

                    // 上次音频初始化失败过则重试一次（例如用户刚装好 VB-Cable）
                    if matches!(AudioInitState::load(), AudioInitState::DecoderFailed | AudioInitState::DeviceFailed) { 
                        init_audio_worker(); 
                    } 
                    // 端口可能在本次会话中改过，补齐防火墙规则
                    std::thread::spawn(move || { let _ = firewall::add_firewall_rule(port); });

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
                    REVERSE_ENABLED.store(if self.reverse_enabled { 1 } else { 0 }, Ordering::Release);
                } 
                Task::none() 
            } 
            Message::StatusUpdate(info) => { 
                // 只在「无流 → 有流」的那一帧做状态迁移，避免每 500ms 重复清除用户提示
                if info.bitrate_kbps > 0.0 && !self.connected { 
                    self.connected = true; 
                    // 音频真正流动起来后清掉历史错误与断开标记
                    self.notice.clear(); 
                    self.status_text.clear(); 
                } 
                self.last_bitrate = info.bitrate_kbps; 
                self.last_level_db = info.level_db; 
                // 音频诊断：每 30s 落一次盘（计数器由 cpal 回调与音频线程维护）
                if self.last_stats_log.elapsed() >= Duration::from_secs(30) {
                    self.last_stats_log = Instant::now();
                    let s = audio_stats().snapshot();
                    logf!(
                        "[Audio] 统计 块={} 重采样={} 丢样={} 欠载={}",
                        s.writes, s.resampled, s.dropped, s.underrun
                    );
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
            Message::ShowWindow => {
                latest_window_task(|id| {
                    Task::batch([
                        iced::window::set_mode(id, iced::window::Mode::Windowed),
                        iced::window::gain_focus(id),
                    ])
                })
            }
            Message::Quit => { 
                iced::exit()
            } 
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
            (red, if self.status_is_disconnected() { "● 已断开" } else { "● 等待连接" }) 
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

        let vb_str = if self.vb_cable_installed { "● VB-Cable" } else { "● VB-Cable 未安装" };
        let vb_color = if self.vb_cable_installed { Color::from_rgb(0.25, 0.80, 0.40) } else { Color::from_rgb(0.90, 0.55, 0.15) };

        // ── 底部状态行（永远只占一行，三种状态互斥）──
        //   未启动        → 广播地址
        //   已启动未连上  → 广播地址 + 广播状态
        //   已连接        → 音频诊断 + 反向串流状态（地址与广播状态对用户已无意义）
        let footer_left = if self.connected {
            let s = audio_stats().snapshot();
            format!(
                "重采样 {} · 丢样 {} · 欠载 {}",
                short_num(s.resampled), short_num(s.dropped), short_num(s.underrun)
            )
        } else {
            "广播地址: 255.255.255.255:44043".to_string()
        };
        let (footer_right, footer_right_color) = if !self.is_running {
            (String::new(), dim)
        } else if self.connected {
            if self.reverse_enabled {
                ("● 反向串流已启用".to_string(), Color::from_rgb(0.25, 0.80, 0.40))
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
                        border: iced::Border { radius: 6.0.into(), ..Default::default() }, 
                        shadow: Default::default(), 
                        snap: true, 
                    } 
                }) 
        }; 

        let btn_c = if self.is_running { red } else { accent }; 
        let auto_c = if self.config.is_auto_start() { accent } else { grey }; 

        let db = self.last_level_db.clamp(-60.0, 0.0); 
        let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0); 

        // 提示行：错误红色、普通信息蓝灰（级别由写入方显式声明）。
        // 无提示时整块只有 6px 间隔：320×290 的方形窗口容不下常驻空行。
        let notice_block: Element<'_, Message> = if self.notice.is_empty() {
            Space::new().height(6).into()
        } else {
            column![
                Space::new().height(6),
                text(self.notice.clone())
                    .size(11)
                    .color(if self.notice_level == NoticeLevel::Error { red } else { Color::from_rgb(0.62, 0.70, 0.78) }),
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
                container(
                    column![
                        text(if self.is_running { "监听端口" } else { "目标地址" }).size(14).color(dim),
                        Space::new().height(4),
                        if self.is_running {
                            let addr = format!("{}:{}", self.config.listen_ip, self.config.listen_port);
                            row![text(addr).size(14).color(Color::WHITE)]
                                .height(32).align_y(Alignment::Center)
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
                                        let base = iced::widget::text_input::default(&iced::theme::Theme::Dark, status);
                                        iced::widget::text_input::Style {
                                            background: iced::Background::Color(Color::from_rgb(0.18, 0.18, 0.22)),
                                            border: iced::Border { radius: 5.0.into(), color: Color::from_rgb(0.25, 0.25, 0.30), width: 1.0 },
                                            ..base
                                        }
                                    }),
                                text(" : ").size(16).color(dim),
                                text_input("44044", &self.port_input)
                                    .on_input(Message::PortChanged)
                                    .width(68)
                                    .padding([5, 8])
                                    .style(move |_, status| {
                                        let base = iced::widget::text_input::default(&iced::theme::Theme::Dark, status);
                                        iced::widget::text_input::Style {
                                            background: iced::Background::Color(Color::from_rgb(0.18, 0.18, 0.22)),
                                            border: iced::Border {
                                                radius: 5.0.into(),
                                                color: if self.port_valid { Color::from_rgb(0.25, 0.25, 0.30) } else { red },
                                                width: 1.0
                                            },
                                            ..base
                                        }
                                    }),
                            ].height(32).align_y(Alignment::Center)
                        },
                    ]
                )
                .padding(10)
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(iced::Background::Color(card_bg)),
                    border: iced::Border { radius: 8.0.into(), color: border_color, width: 1.0 },
                    ..Default::default()
                }),
                Space::new().height(8.0),
                container(
                    column![
                        row![
                            column![
                                text("码率").size(12).color(dim),
                                Space::new().height(2),
                                row![
                                    text(format!("{:.0}", self.last_bitrate as u32)).size(22),
                                    Space::new().width(3),
                                    text("kbps").size(11).color(dim)
                                ].align_y(Alignment::End)
                            ].width(Length::FillPortion(1)),
                            column![
                                text("电平").size(12).color(dim),
                                Space::new().height(2),
                                row![
                                    text(format!("{:.0}", self.last_level_db as i32)).size(22),
                                    Space::new().width(3),
                                    text("dB").size(11).color(dim)
                                ].align_y(Alignment::End)
                            ].width(Length::FillPortion(1)),
                        ],
                        Space::new().height(6),
                        progress_bar(0.0..=1.0, frac)
                            .girth(3)
                            .style(move |_| {
                                let bar_color = if db > -10.0 { red }
                                else if db > -25.0 { Color::from_rgb(0.90, 0.55, 0.15) }
                                else { accent };
                                iced::widget::progress_bar::Style {
                                    background: iced::Background::Color(Color::from_rgb(0.18, 0.18, 0.22)),
                                    bar: iced::Background::Color(bar_color),
                                    border: iced::Border { radius: 2.0.into(), ..Default::default() },
                                }
                            }),
                    ]
                )
                .padding(10)
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(iced::Background::Color(card_bg)),
                    border: iced::Border { radius: 8.0.into(), color: border_color, width: 1.0 },
                    ..Default::default()
                }),
                Space::new().height(15),
                row![
                    mk(
                        if self.is_running { "停止" } else { "启动" },
                        btn_c,
                        Message::ToggleRunning,
                    ).width(Length::FillPortion(1)),
                    Space::new().width(8),
                    mk(
                        "开机自启",
                        auto_c,
                        Message::ToggleAutoStart(!self.config.is_auto_start()),
                    ).width(Length::FillPortion(1)),
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
                                border: iced::Border { radius: 6.0.into(), ..Default::default() },
                                shadow: Default::default(),
                                snap: true,
                            })
                    } else {
                        mk("反向串流",
                           if self.reverse_enabled { accent } else { grey },
                           Message::ToggleReverse(!self.reverse_enabled))
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
            subs.push(
                Subscription::run_with((ip, port), |data| udp_receiver_stream(data.0.clone(), data.1))
            );
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
unsafe extern "system" fn find_window_for_process(hwnd: HWND, data: LPARAM) -> BOOL {
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
        }
    )
}

/// 发送 CONNECT_ACK。
/// 复用监听 socket 发送：不再每次保活都新建 socket（1 个/秒、约 8.6 万个/天），
/// 且 ACK 源端口恒为服务端口，便于对端归属判断。
async fn send_connect_ack(socket: &tokio::net::UdpSocket, src_addr: &std::net::SocketAddr) {
    let my_id = my_device_id();
    let ack_packet = protocol::build_packet(
        false, protocol::TYPE_CONNECT_ACK, 0, 0, &my_id, &[], 0,
    );
    let _ = socket.send_to(&ack_packet, src_addr).await;
}

/// 处理控制消息（TYPE_CONNECT / TYPE_CONNECT_ACK / TYPE_DISCOVER_*）
async fn handle_control(
    header: &protocol::PacketHeader,
    payload: &[u8],
    src_addr: &std::net::SocketAddr,
    socket: &tokio::net::UdpSocket,
    output: &mut iced::futures::channel::mpsc::Sender<Message>,
) {
    /// 从 CONNECT payload 中解析 Opus 编码参数
    /// 布局: [0..2] 反向端口, [2] 标志位(bit1=携带 Opus 参数), [3..10] 编码参数
    fn parse_opus_cfg(payload: &[u8]) -> Option<capture::OpusReverseConfig> {
        if payload.len() < 10 || (payload[2] & 0x02) == 0 {
            return None; // 未携带扩展配置（首包仅含反向端口）
        }
        let flags = payload[6];
        Some(capture::OpusReverseConfig {
            bitrate_kbps: u16::from_be_bytes([payload[7], payload[8]]),
            bandwidth: payload[5],           // 0-4
            complexity: payload[3],           // 1-10
            signal: payload[4],               // 0=auto, 1=voice, 2=music
            vbr: (flags & 0x01) != 0,
            dtx: (flags & 0x02) != 0,
            vbr_constraint: (flags & 0x04) != 0,
            fec: (flags & 0x08) != 0,
            packet_loss: payload[9],          // 0-100
        })
    }

    if header.msg_type == protocol::TYPE_CONNECT {
            // 提取反向端口（CONNECT payload 前2字节，big-endian）
            let rev_port = if payload.len() >= 2 {
                ((payload[0] as u16) << 8) | (payload[1] as u16)
            } else {
                0
            };
            // 同步 Opus 编码设置
            if let Some(cfg) = parse_opus_cfg(payload) {
                capture::update_reverse_config(cfg);
            }
            let android_rev_addr = std::net::SocketAddr::new(
                src_addr.ip(),
                if rev_port > 0 { rev_port } else { src_addr.port() },
            );

            // 同设备保活重连
            let same_device = {
                let st = p2p_lock();
                st.is_busy() && st.device_id == header.device_id
            };
            if same_device {
                if rev_port > 0 {
                    // 仅端口真正变化时才重启发送器（避免每秒保活都重启）
                    let port_changed = android_addr().lock().ok()
                        .map(|mut ad| {
                            let changed = ad.as_ref()
                                .map(|(_, p)| *p != rev_port)
                                .unwrap_or(true);
                            *ad = Some((android_rev_addr, rev_port));
                            changed
                        })
                        .unwrap_or(true);
                    if port_changed && REVERSE_ENABLED.load(Ordering::Acquire) != 0 {
                        if let Ok(mut sender) = reverse_sender().lock() {
                            if let Some(s) = sender.take() { s.stop(); }
                            let my_id = my_device_id();
                            *sender = Some(capture::ReverseSender::start(android_rev_addr, my_id));
                            logf!("[P2P] 反向端口变化，已重启 -> {}:{}", src_addr.ip(), android_rev_addr.port());
                        }
                    }
                }
                send_connect_ack(socket, src_addr).await;
                return;
            }

            // 已被其他设备占用 → 静默拒绝；空闲 → 建立绑定
            {
                let mut st = p2p_lock();
                if st.is_busy() {
                    return;
                }
                st.device_id = header.device_id;
                st.state = DEVICE_BUSY;
            }
            logf!("[P2P] 手机端 {} 已绑定连接", src_addr.ip());
            // 只触发一次重绘（让顶部「● 已占用」立刻出现）。
            // 不再向底部提示行写"已连接手机端"：连接状态由顶部状态点统一表达；
            // 也不再用 bitrate=0 的 StatusUpdate 冒充重绘信号。
            let _ = output.try_send(Message::PeerStateChanged);
            send_connect_ack(socket, src_addr).await;

            // 存储 Android 地址+反向端口
            if let Ok(mut ad) = android_addr().lock() {
                *ad = Some((android_rev_addr, rev_port));
            }
            // 仅用户开启反向开关时才启动反向音频
            if REVERSE_ENABLED.load(Ordering::Acquire) != 0 {
                let my_id = my_device_id();
                if let Ok(mut sender) = reverse_sender().lock() {
                    if let Some(old) = sender.take() { old.stop(); }
                    *sender = Some(capture::ReverseSender::start(android_rev_addr, my_id));
                }
                logf!("[P2P] 反向音频发送到 {}:{}", src_addr.ip(), android_rev_addr.port());
            } else {
                logf!("[P2P] 反向音频开关未开启，跳过启动");
            }
        }
}

fn udp_receiver_stream(listen_ip: String, listen_port: u32) -> impl iced::futures::Stream<Item = Message> { 
    iced::stream::channel(64, move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move { 
        let bind_addr = format!("{}:{}", listen_ip, listen_port); 
        let socket = match tokio::net::UdpSocket::bind(&bind_addr).await { 
            Ok(s) => s, 
            Err(e) => { 
                let _ = output.send(Message::EngineError(format!(
                    "监听 {bind_addr} 失败：{}（端口可能被占用）", e.kind()
                ))).await; 
                return; 
            } 
        }; 

        // 等待音频初始化完成（最多 3 秒；冷启动时音频服务可能较慢）
        let mut retries = 60; 
        while AudioInitState::load() == AudioInitState::Pending && retries > 0 { 
            tokio::time::sleep(Duration::from_millis(50)).await; 
            retries -= 1; 
        } 

        match AudioInitState::load() { 
            AudioInitState::DecoderFailed => { 
                let _ = output.send(Message::EngineError(
                    "Opus 解码器初始化失败".into()
                )).await; 
                return; 
            } 
            AudioInitState::DeviceFailed => { 
                let _ = output.send(Message::EngineError(
                    "音频设备初始化失败（请确认 VB-Cable 已安装后重启程序）".into()
                )).await; 
                return; 
            } 
            _ => {} 
        } 

        send_audio(AudioMessage::Reset); 

        let mut buf = vec![0u8; protocol::MAX_PACKET]; 
        let mut last_stats = Instant::now(); 
        let mut byte_count: u64 = 0; 
        let mut last_packet = Instant::now(); 
        let mut was_disconnected = false; 

        loop { 
            let result = tokio::time::timeout( 
                Duration::from_millis(300), 
                socket.recv_from(&mut buf), 
            ).await; 

            let (len, src) = match result { 
                Ok(Ok(v)) => v, 
                _ => { 
                    if !was_disconnected && last_packet.elapsed() > Duration::from_secs(1) { 
                        reset_to_ready();
                        let _ = output.send(Message::Disconnected).await; 
                        was_disconnected = true; 
                    } 
                    continue; 
                } 
            }; 

            if len < protocol::HEADER_SIZE { 
                continue; 
            } 

            // 解析统一协议头
            let hdr_buf: [u8; protocol::HEADER_SIZE] = match buf[..protocol::HEADER_SIZE].try_into() {
                Ok(b) => b,
                Err(_) => continue,
            };
            let h = match protocol::decode_header(&hdr_buf) {
                Some(h) => h,
                None => continue,
            };

            // ═══ 控制消息 ═══
            if !h.is_audio {
                let payload_end = (protocol::HEADER_SIZE + h.payload_len as usize).min(len);
                let payload = &buf[protocol::HEADER_SIZE..payload_end];
                handle_control(&h, payload, &src, &socket, &mut output).await;
                continue;
            }

            // ═══ 音频数据 (TYPE_DATA) ═══
            // 安全准入: 仅 TYPE_CONNECT 可建连，READY 时拒绝所有音频
            // 注意：device_id 在发现回复中是明文，因此这只能防误连，不能防伪造
            let accepted = {
                let st = p2p_lock();
                st.is_busy() && st.device_id == h.device_id
            };
            if !accepted {
                continue;
            }

            last_packet = Instant::now(); 

            if was_disconnected { 
                was_disconnected = false; 
                // 重连同样只刷新状态，不再往底部写"已重新连接"
                let _ = output.send(Message::PeerStateChanged).await; 
            } 

            byte_count += len as u64; 
            let payload_end = (protocol::HEADER_SIZE + h.payload_len as usize).min(len);
            if payload_end > protocol::HEADER_SIZE {
                // 只投递声明长度内的负载，避免把尾部垃圾喂给解码器。
                // 缓冲从池中取，音频线程解码后归还，避免每包一次 malloc。
                let mut payload = take_payload();
                payload.extend_from_slice(&buf[protocol::HEADER_SIZE..payload_end]);
                send_audio(AudioMessage::Packet {
                    seq_num: h.seq_num,
                    sample_rate: h.sample_rate,
                    payload,
                });
            }

            if last_stats.elapsed() > Duration::from_millis(500) { 
                let elapsed = last_stats.elapsed().as_secs_f32(); 
                let kbps = if elapsed > 0.0 { 
                    (byte_count * 8) as f32 / elapsed / 1000.0 
                } else { 
                    0.0 
                }; 
                let level_db = f32::from_bits(AUDIO_LEVEL_DB.load(Ordering::Relaxed)); 
                byte_count = 0; 
                last_stats = Instant::now(); 
                let _ = output.send(Message::StatusUpdate(StatusInfo { 
                    bitrate_kbps: kbps, 
                    level_db, 
                })).await; 
            } 
        } 
    }) 
}
