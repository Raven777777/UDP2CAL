// UDP2Mic Windows 接收端 - 局域网麦克风 
#![windows_subsystem = "windows"] 

mod audio; 
mod config; 
mod decoder; 
mod firewall; 
mod protocol; 

use iced::widget::{button, column, container, progress_bar, row, text, text_input, Space}; 
use iced::{Alignment, Element, Length, Subscription, Task, Theme, Color}; 
use iced::futures::SinkExt; 
use std::sync::atomic::{AtomicU32, Ordering}; 
use std::sync::mpsc::{sync_channel, SyncSender}; 
use std::sync::OnceLock; 
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

// ========================================== 
// 全局音频守护线程架构 (Audio Worker Backend) 
// ========================================== 
enum AudioMessage { 
    Packet { seq_num: u8, sample_rate: u8, payload: Vec<u8> }, 
    Reset, 
} 

static AUDIO_TX: OnceLock<SyncSender<AudioMessage>> = OnceLock::new(); 
static AUDIO_LEVEL_DB: AtomicU32 = AtomicU32::new((-60.0f32).to_bits()); 
// 0=加载中, 1=解码器错误, 2=音频初始化错误, 3=初始化成功 
static AUDIO_INIT_STATE: AtomicU32 = AtomicU32::new(0); 

fn init_audio_worker() { 
    let (tx, rx) = sync_channel::<AudioMessage>(200); 
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

// ========================================== 
// UI 及主控逻辑 
// ========================================== 
#[derive(Debug, Clone)] 
enum Message { 
    IpChanged(String), 
    PortChanged(String), 
    ToggleRunning, 
    ToggleAutoStart(bool), 
    StatusUpdate(StatusInfo), 
    Disconnected, 
    HideWindow, 
    ShowWindow, 
    Quit, 
} 

// 窗口关闭请求跟踪，用于拦截关闭按钮
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
} 

/// 广播发现服务：监听手机搜索请求，回复当前实际监听端口
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
                    let msg = String::from_utf8_lossy(&buf[..amt]); 
                    if msg.trim() == "UDP2MIC_DISCOVER" { 
                        // 每次搜索请求从注册表读取最新端口
                        let port = config::Config::load().listen_port; 
                        let reply = format!("UDP2MIC_REPLY:{}", port); 
                        let _ = socket.send_to(reply.as_bytes(), src); 
                    } 
                } 
                Err(_) => {} 
            } 
        } 
    }); 
} 

fn main() -> Result<(), iced::Error> { 
    unsafe { 
        let name: Vec<u16> = "UDP2Mic_SingleInstance_Mutex\0".encode_utf16().collect(); 
        let _ = CreateMutexW(None, true, PCWSTR::from_raw(name.as_ptr())); 
        if GetLastError() == ERROR_ALREADY_EXISTS { 
            let title: Vec<u16> = "UDP2Mic\0".encode_utf16().collect(); 
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

    start_broadcast_listener(); 
    init_audio_worker(); 

    // 初始化图标 (加载 icon.png，失败时回退到程序绘制) 
    let (icon_rgba, w, h) = load_icon_rgba(); 

    // 1. 初始化系统托盘 (适配 tray-icon 0.24.0) 
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
        .with_tooltip("UDP2Mic") 
        .with_icon(tray_icon_img)
        .with_menu_on_left_click(false) 
        .build() 
        .unwrap(); 

    // 驻留托盘实例，防止生命周期结束导致托盘图标消失 
    Box::leak(Box::new(tray_icon)); 

    // 2. 启动主窗体
    iced::application(move || {
        let cfg = config::Config::load();
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
            config: cfg,
            last_toggle_instant: now,
            session_id: 0,
        };
        (state, Task::none())
    }, AppState::update, AppState::view)
        .title("UDP2Mic")
        .subscription(AppState::subscription)
        .default_font(iced::Font::with_name("Microsoft YaHei"))
        .theme(Theme::Dark)
        .window(iced::window::Settings {
            size: iced::Size::new(380.0, 300.0),
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
            Message::ToggleRunning => { 
                if self.last_toggle_instant.elapsed() < Duration::from_millis(200) { 
                    return Task::none(); 
                } 
                self.last_toggle_instant = Instant::now(); 
                self.session_id = self.session_id.wrapping_add(1); 

                if self.is_running { 
                    self.is_running = false; 
                    self.connected = false; 
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

                    self.is_running = true; 
                    self.connected = false; 
                    self.last_bitrate = 0.0; 
                    self.last_level_db = -60.0; 
                    self.status_text = "等待连接...".into(); 
                    self.vb_cable_installed = audio::detect_vb_cable().is_some(); 
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
                // 点击关闭按钮时隐藏窗口（不在任务栏显示） 
                unsafe { 
                    let title: Vec<u16> = "UDP2Mic\0".encode_utf16().collect(); 
                    if let Ok(hwnd) = FindWindowW(None, PCWSTR::from_raw(title.as_ptr())) { 
                        if !hwnd.is_invalid() { 
                            let _ = ShowWindow(hwnd, SW_HIDE); 
                        } 
                    } 
                } 
                Task::none() 
            }
            Message::ShowWindow => {
                // 双击托盘图标恢复并置顶窗口 
                unsafe { 
                    let title: Vec<u16> = "UDP2Mic\0".encode_utf16().collect(); 
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

        let vb: Element<_> = if self.vb_cable_installed { 
            row![ 
                text("●").color(Color::from_rgb(0.25, 0.80, 0.40)).size(9), 
                text("VB-Cable").size(11).color(dim), 
            ] 
            .spacing(4) 
            .align_y(Alignment::Center) 
            .into() 
        } else { 
            row![ 
                text("●").color(Color::from_rgb(0.90, 0.55, 0.15)).size(9), 
                text("VB-Cable 未安装").size(11).color(Color::from_rgb(0.90, 0.55, 0.15)), 
            ] 
            .spacing(4) 
            .align_y(Alignment::Center) 
            .into() 
        }; 

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
                    text("UDP2Mic").size(20), 
                    Space::new().width(Length::Fill), 
                    text(st).color(sc).size(12), 
                    Space::new().width(10.0), 
                    vb, 
                ] 
                .align_y(Alignment::Center), 
                Space::new().height(14.0), 
                container( 
                    column![ 
                        text(if self.is_running { "当前正在监听端口" } else { "配置局域网监听地址" }).size(11).color(dim), 
                        Space::new().height(4.0), 
                        if self.is_running { 
                            let addr = format!("{}:{}", self.config.listen_ip, self.config.listen_port); 
                            row![text(addr).size(14).color(Color::WHITE)] 
                                .align_y(Alignment::Center) 
                        } else { 
                            row![ 
                                text_input("0.0.0.0", &self.ip_input) 
                                    .on_input(Message::IpChanged) 
                                    .width(150) 
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
                                    .width(70) 
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
                            ] 
                            .align_y(Alignment::Center) 
                        }, 
                    ] 
                ) 
                .padding(12) 
                .width(Length::Fill) 
                .style(move |_| container::Style { 
                    background: Some(iced::Background::Color(card_bg)), 
                    border: iced::Border { radius: 8.0.into(), color: border_color, width: 1.0 }, 
                    ..Default::default() 
                }), 
                Space::new().height(12.0), 
                container( 
                    column![ 
                        row![ 
                            column![ 
                                text("传输码率").size(10).color(dim), 
                                Space::new().height(2.0), 
                                row![ 
                                    text(format!("{:.0}", self.last_bitrate as u32)).size(24), 
                                    Space::new().width(3.0), 
                                    text("kbps").size(11).color(dim) 
                                ].align_y(Alignment::End) 
                            ].width(Length::FillPortion(1)), 
                            column![ 
                                text("声音电平").size(10).color(dim), 
                                Space::new().height(2.0), 
                                row![ 
                                    text(format!("{:.0}", self.last_level_db as i32)).size(24), 
                                    Space::new().width(3.0), 
                                    text("dB").size(11).color(dim) 
                                ].align_y(Alignment::End) 
                            ].width(Length::FillPortion(1)), 
                        ], 
                        Space::new().height(8.0), 
                        progress_bar(0.0..=1.0, frac as f32) 
                            .girth(4) 
                            .style(move |_| { 
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
                                    border: iced::Border { radius: 2.0.into(), ..Default::default() }, 
                                } 
                            }), 
                    ] 
                ) 
                .padding(12) 
                .width(Length::Fill) 
                .style(move |_| container::Style { 
                    background: Some(iced::Background::Color(card_bg)), 
                    border: iced::Border { radius: 8.0.into(), color: border_color, width: 1.0 }, 
                    ..Default::default() 
                }), 
                Space::new().height(14.0), 
                row![ 
                    mk( 
                        if self.is_running { "停止" } else { "启动" }, 
                        btn_c, 
                        Message::ToggleRunning, 
                    ).width(Length::FillPortion(1)), 
                    Space::new().width(8.0), 
                    mk( 
                        "开机自启", 
                        auto_c, 
                        Message::ToggleAutoStart(!self.config.is_auto_start()), 
                    ).width(Length::FillPortion(1)), 
                ], 
                Space::new().height(8.0), 
                row![ 
                    text("广播地址: 255.255.255.255:44043").size(10).color(grey), 
                    Space::new().width(Length::Fill), 
                    if !self.status_text.is_empty() { 
                        text(&self.status_text).size(10).color(if self.status_text.contains("失败") { red } else { dim }) 
                    } else { 
                        text("") 
                    } 
                ].align_y(Alignment::Center) 
            ] 
            .padding(16), 
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
        // 捕获窗口关闭请求 
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

            let (len, _src) = match result { 
                Ok(Ok(v)) => v, 
                _ => { 
                    if !was_disconnected && last_packet.elapsed() > Duration::from_secs(3) { 
                        let _ = output.send(Message::Disconnected).await; 
                        was_disconnected = true; 
                    } 
                    continue; 
                } 
            }; 

            if len < protocol::HEADER_SIZE { 
                continue; 
            } 

            let hdr_array: [u8; protocol::HEADER_SIZE] = match buf[..protocol::HEADER_SIZE].try_into() { 
                Ok(b) => b, 
                Err(_) => continue, 
            }; 

            let h = match protocol::decode_header(&hdr_array) { 
                Some(h) => h, 
                None => continue, 
            }; 

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
