// UDP2CAL Windows 接收端 - 局域网音频串流
// 双状态机架构 + P2P 独占通信系统
#![windows_subsystem = "windows"] 

mod audio; 
mod capture; 
mod config; 
mod decoder; 
mod firewall; 
mod protocol; 

use iced::widget::{button, column, container, progress_bar, row, text, text_input, Space}; 
use iced::{Alignment, Element, Length, Subscription, Task, Theme, Color}; 
use iced::futures::SinkExt; 
use std::net::SocketAddr; 
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering}; 
use std::sync::mpsc::{sync_channel, SyncSender}; 
use std::sync::{Mutex, OnceLock}; 
use std::time::{Duration, Instant}; 
use windows::Win32::System::Threading::CreateMutexW; 
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS}; 
use windows::Win32::UI::WindowsAndMessaging::{ 
    MessageBoxW, FindWindowW, ShowWindow, SetForegroundWindow, MB_OK, MB_ICONINFORMATION, SW_HIDE, SW_RESTORE 
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

// ══════════════════════════════════════════════
// P2P 独占通信 - 全局共享状态
// ══════════════════════════════════════════════

const DEVICE_READY: u8 = 0;
const DEVICE_BUSY: u8 = 1;

/// 全局设备状态 (DEVICE_READY / DEVICE_BUSY)
static GLOBAL_DEVICE_STATE: AtomicU8 = AtomicU8::new(DEVICE_READY);

/// UI 是否已启动（停止时禁止广播）
static APP_RUNNING: AtomicU8 = AtomicU8::new(0);

/// 当前绑定手机的唯一设备 ID (8字节)
static BOUND_DEVICE_ID: OnceLock<Mutex<[u8; protocol::DEVICE_ID_SIZE]>> = OnceLock::new();
fn bound_device_id() -> &'static Mutex<[u8; protocol::DEVICE_ID_SIZE]> {
    BOUND_DEVICE_ID.get_or_init(|| Mutex::new([0u8; protocol::DEVICE_ID_SIZE]))
}

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

/// 获取本机设备 ID 字节数组
fn my_device_id() -> [u8; protocol::DEVICE_ID_SIZE] {
    config::Config::load().get_device_id_bytes()
}

/// 获取本机设备名（系统主机名）
fn my_device_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "UDP2CAL".into())
}

// ══════════════════════════════════════════════
// 全局音频守护线程架构
// ══════════════════════════════════════════════

enum AudioMessage { 
    Packet { seq_num: u8, sample_rate: u8, payload: Vec<u8> }, 
    Reset, 
} 

static AUDIO_TX: OnceLock<SyncSender<AudioMessage>> = OnceLock::new(); 
static AUDIO_LEVEL_DB: AtomicU32 = AtomicU32::new((-60.0f32).to_bits()); 
// 0=加载中, 1=解码器错误, 2=音频初始化错误, 3=初始化成功 
static AUDIO_INIT_STATE: AtomicU32 = AtomicU32::new(0); 

fn init_audio_worker() { 
    let (tx, rx) = sync_channel::<AudioMessage>(50); 
    let _ = AUDIO_TX.set(tx); 

    std::thread::spawn(move || { 
        let mut dec = match decoder::OpusDecoder::new() { 
            Ok(d) => d, 
            Err(_) => { 
                AUDIO_INIT_STATE.store(1, Ordering::Relaxed); 
                return; 
            } 
        }; 
        let mut aw = match audio::start_audio() { 
            Ok(a) => a, 
            Err(_) => { 
                AUDIO_INIT_STATE.store(2, Ordering::Relaxed); 
                return; 
            } 
        }; 
        AUDIO_INIT_STATE.store(3, Ordering::Relaxed); 

        let mut rb = protocol::ReorderBuffer::new(); 
        let mut rms_sum: f64 = 0.0; 
        let mut rms_count: u64 = 0; 
        let mut last_stats = Instant::now(); 

        for msg in rx { 
            match msg { 
                AudioMessage::Reset => { 
                    rb = protocol::ReorderBuffer::new(); 
                    rms_sum = 0.0; 
                    rms_count = 0; 
                    AUDIO_LEVEL_DB.store((-60.0f32).to_bits(), Ordering::Relaxed); 
                } 
                AudioMessage::Packet { seq_num, sample_rate, payload } => { 
                    rb.insert_and_drain(seq_num, sample_rate, &payload, |sr, p| { 
                        if let Some((n, sr_hz)) = dec.decode(sr, p) { 
                            let pcm = dec.pcm_data(); 
                            let samples = &pcm[..n.min(pcm.len())]; 
                            rms_sum += samples.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>(); 
                            rms_count += samples.len() as u64; 
                            aw.write(samples, sr_hz); 
                        } 
                    }); 
                } 
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
fn start_broadcast_state_machine() {
    std::thread::spawn(move || {
        let socket = match std::net::UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = socket.set_broadcast(true);
        loop {
            if APP_RUNNING.load(Ordering::Relaxed) != 0
                && GLOBAL_DEVICE_STATE.load(Ordering::Relaxed) == DEVICE_READY {
                let device_id = my_device_id();
                let my_name = my_device_name();
                let mut payload = Vec::with_capacity(2 + my_name.len());
                let port = config::Config::load().listen_port as u16;
                payload.extend_from_slice(&port.to_be_bytes());
                payload.extend_from_slice(my_name.as_bytes());
                let packet = protocol::build_packet(
                    false, protocol::TYPE_DISCOVER_REPLY, 0, 0,
                    &device_id, &payload, 0,
                );
                let _ = socket.send_to(&packet, "255.255.255.255:44043");
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// 重置全局状态到 DEVICE_READY
fn reset_to_ready() {
    GLOBAL_DEVICE_STATE.store(DEVICE_READY, Ordering::Relaxed);
    if let Ok(mut id) = bound_device_id().lock() {
        id.fill(0);
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
                eprintln!("[Discovery] 绑定广播端口失败: {}", e); 
                return; 
            } 
        }; 
        let mut buf = [0u8; 1024]; 
        loop { 
            match socket.recv_from(&mut buf) { 
                Ok((amt, src)) => { 
                    let data = &buf[..amt];
                    // 尝试统一 v2 协议
                    if amt >= protocol::HEADER_SIZE && protocol::is_v2_packet(data) {
                        let hdr_buf: [u8; protocol::HEADER_SIZE] = 
                            data[..protocol::HEADER_SIZE].try_into().unwrap_or_default();
                        if let Some(hdr) = protocol::decode_header(&hdr_buf) {
                            if APP_RUNNING.load(Ordering::Relaxed) != 0
                                && GLOBAL_DEVICE_STATE.load(Ordering::Relaxed) == DEVICE_READY
                                && hdr.msg_type == protocol::TYPE_DISCOVER_REQ {
                                let cfg = config::Config::load();
                                let port = cfg.listen_port as u16;
                                let my_id = cfg.get_device_id_bytes();
                                let my_name = my_device_name();
                                let mut payload = Vec::with_capacity(2 + my_name.len());
                                payload.extend_from_slice(&port.to_be_bytes());
                                payload.extend_from_slice(my_name.as_bytes());
                                let reply = protocol::build_packet(
                                    false, protocol::TYPE_DISCOVER_REPLY, 0, 0,
                                    &my_id, &payload, 0,
                                );
                                let _ = socket.send_to(&reply, src);
                                continue;
                            }
                        }
                    }

                } 
                Err(_) => {} 
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
    HideWindow, 
    ShowWindow, 
    Quit, 
} 

#[derive(Debug, Clone)] 
struct StatusInfo { 
    bitrate_kbps: f32, 
    level_db: f32, 
    error_msg: Option<&'static str>, 
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
    vb_cable_installed: bool, 
    last_toggle_instant: Instant, 
    session_id: u32, 
    reverse_audio: bool,
    reverse_enabled: bool,
} 

fn main() -> Result<(), iced::Error> { 
    unsafe { 
        let name: Vec<u16> = "UDP2CAL_SingleInstance_Mutex\0".encode_utf16().collect(); 
        let _ = CreateMutexW(None, true, PCWSTR::from_raw(name.as_ptr())); 
        if GetLastError() == ERROR_ALREADY_EXISTS { 
            let title: Vec<u16> = "UDP2CAL\0".encode_utf16().collect();
            let msg: Vec<u16> = "程序已在运行中\0".encode_utf16().collect();
            MessageBoxW( 
                None, 
                PCWSTR::from_raw(msg.as_ptr()), 
                PCWSTR::from_raw(title.as_ptr()), 
                MB_OK | MB_ICONINFORMATION, 
            ); 
            return Ok(()); 
        } 
    } 

    std::thread::spawn(|| { 
        let _ = firewall::add_firewall_rule(); 
    }); 

    // P2P 系统初始化
    start_broadcast_listener(); 
    start_broadcast_state_machine();
    init_audio_worker(); 

    // 初始化图标
    let (icon_rgba, w, h) = load_icon_rgba(); 

    // 系统托盘
    let tray_menu = tray_icon::menu::Menu::new(); 
    let quit_item = tray_icon::menu::MenuItemBuilder::new() 
        .id("quit".into()) 
        .text("退出") 
        .enabled(true) 
        .build(); 
    let _ = tray_menu.append(&quit_item); 

    let tray_icon_img = tray_icon::Icon::from_rgba(icon_rgba.clone(), w, h).unwrap(); 
    let tray_icon = tray_icon::TrayIconBuilder::new() 
        .with_menu(Box::new(tray_menu)) 
        .with_tooltip("UDP2CAL") 
        .with_icon(tray_icon_img)
        .with_menu_on_left_click(false) 
        .build() 
        .unwrap(); 
    Box::leak(Box::new(tray_icon)); 

    // 主窗体
    iced::application(move || {
        let cfg = config::Config::load();
        REVERSE_ENABLED.store(if cfg.reverse_enabled != 0 { 1 } else { 0 }, Ordering::Relaxed);
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
            vb_cable_installed: audio::detect_vb_cable().is_some(),
            reverse_audio: false,
            reverse_enabled: cfg.reverse_enabled != 0,
            config: cfg,
            last_toggle_instant: now,
            session_id: 0,
        };
        (state, Task::none())
    }, AppState::update, AppState::view)
        .title("UDP2CAL")
        .subscription(AppState::subscription)
        .default_font(iced::Font::with_name("Microsoft YaHei"))
        .theme(Theme::Dark)
        .window(iced::window::Settings {
            size: iced::Size::new(360.0, 300.0),
            position: iced::window::Position::Centered,
            resizable: false,
            exit_on_close_request: false,
            icon: Some(iced::window::icon::from_rgba(icon_rgba, w, h).unwrap()),
            ..Default::default()
        })
        .run() 
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
                self.config.set_auto_start(enabled); 
                let _ = self.config.save(); 
                Task::none() 
            } 
            Message::ToggleReverse(enabled) => {
                // 运行中不可更改
                if self.is_running { return Task::none(); }
                self.reverse_enabled = enabled;
                REVERSE_ENABLED.store(if enabled { 1 } else { 0 }, Ordering::Relaxed);
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
                    APP_RUNNING.store(0, Ordering::Relaxed);
                    REVERSE_ENABLED.store(0, Ordering::Relaxed);
                    reset_to_ready();
                    self.is_running = false; 
                    self.connected = false; 
                    self.reverse_audio = false; 
                    self.last_bitrate = 0.0; 
                    self.last_level_db = -60.0; 
                    self.status_text.clear(); 
                } else { 
                    let port: u16 = match self.port_input.parse() { 
                        Ok(p) => p, 
                        Err(_) => { 
                            self.status_text = "端口号无效，请输入 1-65535".into(); 
                            return Task::none(); 
                        } 
                    }; 
                    if port == 0 { 
                        self.status_text = "端口号不能为 0".into(); 
                        return Task::none(); 
                    } 
                    self.config.listen_ip = self.ip_input.clone(); 
                    self.config.listen_port = port as u32; 
                    let _ = self.config.save(); 

                    APP_RUNNING.store(1, Ordering::Relaxed);
                    reset_to_ready();
                    self.is_running = true; 
                    self.connected = false; 
                    self.last_bitrate = 0.0; 
                    self.last_level_db = -60.0; 
                    self.status_text = "等待连接...".into(); 
                    self.vb_cable_installed = audio::detect_vb_cable().is_some(); 
                    // 应用反向串流设置到全局
                    REVERSE_ENABLED.store(if self.reverse_enabled { 1 } else { 0 }, Ordering::Relaxed);
                } 
                Task::none() 
            } 
            Message::StatusUpdate(info) => { 
                if info.bitrate_kbps > 0.0 { 
                    self.connected = true; 
                } 
                self.last_bitrate = info.bitrate_kbps; 
                self.last_level_db = info.level_db; 
                self.status_text = match info.error_msg { 
                    Some(msg) => msg.to_string(), 
                    None if self.connected => String::new(), 
                    None => self.status_text.clone(), 
                }; 
                Task::none() 
            } 
            Message::Disconnected => { 
                if self.is_running && self.connected { 
                    self.connected = false; 
                    self.last_bitrate = 0.0; 
                    self.last_level_db = -60.0; 
                    self.status_text = "已断开".into(); 
                } 
                Task::none() 
            } 
            Message::HideWindow => {
                unsafe { 
                    let title: Vec<u16> = "UDP2CAL\0".encode_utf16().collect(); 
                    if let Ok(hwnd) = FindWindowW(None, PCWSTR::from_raw(title.as_ptr())) { 
                        if !hwnd.is_invalid() { 
                            let _ = ShowWindow(hwnd, SW_HIDE); 
                        } 
                    } 
                } 
                Task::none() 
            }
            Message::ShowWindow => {
                unsafe { 
                    let title: Vec<u16> = "UDP2CAL\0".encode_utf16().collect(); 
                    if let Ok(hwnd) = FindWindowW(None, PCWSTR::from_raw(title.as_ptr())) { 
                        if !hwnd.is_invalid() { 
                            let _ = ShowWindow(hwnd, SW_RESTORE); 
                            let _ = SetForegroundWindow(hwnd); 
                        } 
                    } 
                } 
                Task::none() 
            }
            Message::Quit => { 
                std::process::exit(0); 
            } 
        } 
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
            (red, if self.status_text.contains("断开") { "● 已断开" } else { "● 等待连接" }) 
        };

        // P2P 状态: 独占绑定(仅v2 TYPE_CONNECT触发) / 音频流连接(v1) / 空闲
        let gs = GLOBAL_DEVICE_STATE.load(Ordering::Relaxed);
        let (p2p_state, p2p_color) = if !self.is_running {
            (String::new(), dim)
        } else if gs == DEVICE_BUSY {
            // 独占绑定: 通过 v2 TYPE_CONNECT 建立, 显示设备 ID
            let id_str = bound_device_id().lock().ok()
                .map(|id| {
                    let hex: String = id.iter().map(|b| format!("{:02x}", b)).collect();
                    if hex.chars().all(|c| c == '0') { String::new() }
                    else if hex.len() > 8 { hex[..8].to_string() } else { hex }
                })
                .unwrap_or_default();
            if id_str.is_empty() {
                ("● 已占用".to_string(), Color::from_rgb(0.95, 0.55, 0.15))
            } else {
                (format!("● 已占用 {}", id_str), Color::from_rgb(0.95, 0.55, 0.15))
            }
        } else if self.connected {
            // v1 音频流连接: 有数据流但无独占绑定
            ("● 已连接".to_string(), accent)
        } else {
            ("● 空闲".to_string(), accent)
        };

        let vb_str = if self.vb_cable_installed { "● VB-Cable" } else { "● VB-Cable 未安装" };
        let vb_color = if self.vb_cable_installed { Color::from_rgb(0.25, 0.80, 0.40) } else { Color::from_rgb(0.90, 0.55, 0.15) };

        let mk = |label: &'static str, color: Color, msg: Message| { 
            button(text(label).size(13)) 
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

        let db = self.last_level_db.max(-60.0).min(0.0); 
        let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0); 

        container( 
            column![ 
                row![ 
                    text("UDP2CAL").size(20), 
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
                        text(if self.is_running { "监听端口" } else { "目标地址" }).size(11).color(dim),
                        Space::new().height(4),
                        if self.is_running {
                            let addr = format!("{}:{}", self.config.listen_ip, self.config.listen_port);
                            row![text(addr).size(14).color(Color::WHITE)]
                                .height(32).align_y(Alignment::Center)
                        } else {
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
                            ].align_y(Alignment::Center)
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
                                text("码率").size(10).color(dim),
                                Space::new().height(2),
                                row![
                                    text(format!("{:.0}", self.last_bitrate as u32)).size(22),
                                    Space::new().width(3),
                                    text("kbps").size(11).color(dim)
                                ].align_y(Alignment::End)
                            ].width(Length::FillPortion(1)),
                            column![
                                text("电平").size(10).color(dim),
                                Space::new().height(2),
                                row![
                                    text(format!("{:.0}", self.last_level_db as i32)).size(22),
                                    Space::new().width(3),
                                    text("dB").size(11).color(dim)
                                ].align_y(Alignment::End)
                            ].width(Length::FillPortion(1)),
                        ],
                        Space::new().height(6),
                        progress_bar(0.0..=1.0, frac as f32)
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
                Space::new().height(10),
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
                        let rev_c = if self.reverse_enabled { accent } else { dim };
                        button(text(if self.reverse_enabled { "反向串流" } else { "未启用" }).size(11))
                            .width(Length::FillPortion(1))
                            .height(34)
                            .style(move |_, _| iced::widget::button::Style {
                                background: Some(iced::Background::Color(if rev_c == accent {
                                    Color::from_rgb(0.05, 0.40, 0.25)
                                } else {
                                    Color::from_rgb(0.15, 0.15, 0.18)
                                })),
                                text_color: rev_c,
                                border: iced::Border { radius: 6.0.into(), ..Default::default() },
                                shadow: Default::default(),
                                snap: true,
                            })
                    } else {
                        mk(if self.reverse_enabled { "反向串流" } else { "反向串流" },
                           if self.reverse_enabled { accent } else { grey },
                           Message::ToggleReverse(!self.reverse_enabled))
                    },
                ],
                Space::new().height(6),
                row![
                    text("广播地址: 255.255.255.255:44043").size(10).color(grey),
                    Space::new().width(Length::Fill),
                    if self.is_running {
                        let bc_on = APP_RUNNING.load(Ordering::Relaxed) != 0
                            && GLOBAL_DEVICE_STATE.load(Ordering::Relaxed) == DEVICE_READY;
                        let bc_state = if bc_on { "● 广播中" } else { "● 已静音" };
                        let bc_color = if bc_on { accent } else { dim };
                        text(bc_state).size(10).color(bc_color)
                    } else {
                        text("")
                    },
                ].align_y(Alignment::Center),
                if self.is_running && self.reverse_enabled {
                    let rev_s = if self.connected { "● 反向串流已启用" } else { "○ 等待连接后启用" };
                    text(rev_s).size(10).color(Color::from_rgb(0.25, 0.80, 0.40))
                } else {
                    text("")
                },
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

fn tray_event_stream() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(10, move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
        let tray_rx = tray_icon::TrayIconEvent::receiver();
        let menu_rx = tray_icon::menu::MenuEvent::receiver();
        loop {
            if let Ok(event) = tray_rx.try_recv() {
                if matches!(event, tray_icon::TrayIconEvent::DoubleClick { .. }) {
                    let _ = output.send(Message::ShowWindow).await;
                }
            }
            if let Ok(event) = menu_rx.try_recv() {
                if event.id == "quit" {
                    let _ = output.send(Message::Quit).await;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
}

/// 发送 CONNECT_ACK 到 Android 的源地址（端口来自 CONNECT 包的 src_addr）
fn send_connect_ack(src_addr: &std::net::SocketAddr) {
    if let Ok(ack_sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        let my_id = my_device_id();
        let ack_packet = protocol::build_packet(
            false, protocol::TYPE_CONNECT_ACK, 0, 0, &my_id, &[], 0,
        );
        let _ = ack_sock.send_to(&ack_packet, src_addr);
    }
}

/// 处理控制消息（TYPE_CONNECT / TYPE_CONNECT_ACK / TYPE_DISCOVER_*）
fn handle_control(header: &protocol::PacketHeader, payload: &[u8], src_addr: &std::net::SocketAddr, output: &mut iced::futures::channel::mpsc::Sender<Message>) {
    /// 从扩展 CONNECT payload 中解析 Opus 编码参数（字节 3~9）
    fn parse_opus_cfg(payload: &[u8]) -> Option<capture::OpusReverseConfig> {
        if payload.len() < 10 || (payload[2] & 0x02) == 0 {
            return None; // 无扩展配置字段
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

    match header.msg_type {
        protocol::TYPE_CONNECT => {
            // 提取反向端口（CONNECT payload 前2字节，big-endian）
            let rev_port = if payload.len() >= 2 {
                ((payload[0] as u16) << 8) | (payload[1] as u16)
            } else {
                0
            };
            // 提取低性能模式标志（payload 第3字节 bit0，0=高品质/1=低性能）
            let low_perf = payload.len() >= 3 && (payload[2] & 0x01) != 0;
            // 同步 Opus 编码设置
            if let Some(cfg) = parse_opus_cfg(payload) {
                capture::update_reverse_config(cfg);
            }
            let android_rev_addr = std::net::SocketAddr::new(
                src_addr.ip(),
                if rev_port > 0 { rev_port } else { src_addr.port() },
            );

            let current_state = GLOBAL_DEVICE_STATE.load(Ordering::Relaxed);
            if current_state == DEVICE_BUSY {
                if let Ok(bound) = bound_device_id().lock() {
                    if *bound == header.device_id {
                        // 同设备保活重连
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
                            if port_changed && REVERSE_ENABLED.load(Ordering::Relaxed) != 0 {
                                if let Ok(mut sender) = reverse_sender().lock() {
                                    if let Some(s) = sender.take() { s.stop(); }
                                    let my_id = my_device_id();
                                    *sender = Some(capture::ReverseSender::start(android_rev_addr, my_id, low_perf));
                                    eprintln!("[P2P] 反向端口变化，已重启 -> {}:{} (low_perf={})", src_addr.ip(), android_rev_addr.port(), low_perf);
                                }
                            }
                        }
                        send_connect_ack(src_addr);
                        return;
                    }
                }
                // 异设备 CONNECT → 静默拒绝
                return;
            }
            if let Ok(mut bound) = bound_device_id().lock() {
                *bound = header.device_id;
            }
            GLOBAL_DEVICE_STATE.store(DEVICE_BUSY, Ordering::Relaxed);
            eprintln!("[P2P] 手机端 {} 已绑定连接", src_addr.ip());
            let _ = output.try_send(Message::StatusUpdate(StatusInfo {
                bitrate_kbps: 0.0, level_db: -60.0, error_msg: Some("已连接手机端"),
            }));
            send_connect_ack(src_addr);

            // 存储 Android 地址+反向端口
            if let Ok(mut ad) = android_addr().lock() {
                *ad = Some((android_rev_addr, rev_port));
            }
            // 仅用户开启反向开关时才启动反向音频
            if REVERSE_ENABLED.load(Ordering::Relaxed) != 0 {
                let my_id = my_device_id();
                if let Ok(mut sender) = reverse_sender().lock() {
                    *sender = Some(capture::ReverseSender::start(android_rev_addr, my_id, low_perf));
                }
                eprintln!("[P2P] 反向音频发送到 {}:{} (low_perf={})", src_addr.ip(), android_rev_addr.port(), low_perf);
            } else {
                eprintln!("[P2P] 反向音频开关未开启，跳过启动");
            }
        }
        _ => {}
    }
}

fn udp_receiver_stream(listen_ip: String, listen_port: u32) -> impl iced::futures::Stream<Item = Message> { 
    iced::stream::channel(64, move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move { 
        let bind_addr = format!("{}:{}", listen_ip, listen_port); 
        let socket = match tokio::net::UdpSocket::bind(&bind_addr).await { 
            Ok(s) => s, 
            Err(_) => { 
                let _ = output.send(Message::StatusUpdate(StatusInfo { 
                    bitrate_kbps: 0.0, 
                    level_db: -60.0, 
                    error_msg: Some("绑定失败"), 
                })).await; 
                return; 
            } 
        }; 

        let mut retries = 10; 
        while AUDIO_INIT_STATE.load(Ordering::Relaxed) == 0 && retries > 0 { 
            tokio::time::sleep(Duration::from_millis(50)).await; 
            retries -= 1; 
        } 

        let init_st = AUDIO_INIT_STATE.load(Ordering::Relaxed); 
        if init_st == 1 { 
            let _ = output.send(Message::StatusUpdate(StatusInfo { 
                bitrate_kbps: 0.0, 
                level_db: -60.0, 
                error_msg: Some("Opus解码器初始化失败") 
            })).await; 
            return; 
        } else if init_st == 2 { 
            let _ = output.send(Message::StatusUpdate(StatusInfo { 
                bitrate_kbps: 0.0, 
                level_db: -60.0, 
                error_msg: Some("音频初始化失败") 
            })).await; 
            return; 
        } 

        if let Some(tx) = AUDIO_TX.get() { 
            let _ = tx.try_send(AudioMessage::Reset); 
        } 

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

            if len < protocol::HEADER_SIZE || !protocol::is_v2_packet(&buf) { 
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
                handle_control(&h, payload, &src, &mut output);
                continue;
            }

            // ═══ 音频数据 (TYPE_DATA) ═══
            // 安全准入: 仅 TYPE_CONNECT 可建连，READY 时拒绝所有音频
            // 防止第三者恶意抢占——重启 Win 端即可清空非法绑定
            if GLOBAL_DEVICE_STATE.load(Ordering::Relaxed) == DEVICE_READY {
                continue; // 无合法 TYPE_CONNECT，拒绝所有音频
            }
            // 1对1 过滤: BUSY 时只接受绑定设备的音频
            let bound = bound_device_id().lock().ok()
                .map(|id| *id == h.device_id)
                .unwrap_or(false);
            if !bound {
                continue;
            }

            last_packet = Instant::now(); 

            if was_disconnected { 
                was_disconnected = false; 
                let _ = output.send(Message::StatusUpdate(StatusInfo { 
                    bitrate_kbps: 0.0, 
                    level_db: -60.0, 
                    error_msg: Some("已重新连接"), 
                })).await; 
            } 

            byte_count += len as u64; 
            if let Some(tx) = AUDIO_TX.get() { 
                let _ = tx.try_send(AudioMessage::Packet { 
                    seq_num: h.seq_num, 
                    sample_rate: h.sample_rate, 
                    payload: buf[protocol::HEADER_SIZE..len].to_vec(), 
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
                    error_msg: None, 
                })).await; 
            } 
        } 
    }) 
}
