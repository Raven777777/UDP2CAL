// UDP2Mic Windows 接收端 - 局域网麦克风
#![windows_subsystem = "windows"]

mod audio;
mod config;
mod decoder;
mod firewall;
mod float;
mod protocol;

use iced::widget::{button, column, container, progress_bar, row, text, text_input, Space};
use iced::{Alignment, Element, Length, Subscription, Task, Theme, Color};
use iced::futures::SinkExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK, MB_ICONINFORMATION};
use windows::core::PCWSTR;

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
    let (tx, rx) = sync_channel::<AudioMessage>(200); // 缓冲队列，防止UI卡顿
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

        // 持续处理并播放音频，生命周期跟随主程序，杜绝反复启停导致的内存泄漏
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
    ToggleFloatWindow(bool),
    StatusUpdate(StatusInfo),
    Disconnected,
    Tick,
}

// 附带 session_id 防止 Iced 内部订阅状态滞留
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UdpReceiverId(u32);

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
    float_window_enabled: bool,
    fw_level: Arc<AtomicU32>,
    fw_visible: Arc<AtomicBool>,
    last_toggle_instant: Instant,
    session_id: u32,
}

pub fn start_broadcast_listener(audio_port: u16) {
    std::thread::spawn(move || {
        let socket = match std::net::UdpSocket::bind("0.0.0.0:44043") {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[Discovery] 绑定广播端口失败: {}", e);
                return;
            }
        };

        println!("[Discovery] 自动发现服务已启动，监听端口 44043...");

        let mut buf = [0u8; 1024];
        loop {
            match socket.recv_from(&mut buf) {
                Ok((amt, src)) => {
                    let msg = String::from_utf8_lossy(&buf[..amt]);

                    if msg.trim() == "UDP2MIC_DISCOVER" {
                        println!("[Discovery] 收到来自手机的搜索请求: {}", src);

                        let reply_msg = format!("UDP2MIC_REPLY:{}", audio_port);

                        if let Err(e) = socket.send_to(reply_msg.as_bytes(), src) {
                            eprintln!("[Discovery] 回复手机失败: {}", e);
                        } else {
                            println!("[Discovery] 已成功回复手机: {}", reply_msg);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[Discovery] 接收广播包错误: {}", e);
                }
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

    start_broadcast_listener(config::Config::load().listen_port as u16);

    // ✅ 在主程序启动前，仅初始化一次音频守护线程
    init_audio_worker();

    let fw = float::FloatWindow::new();
    let fw_level = fw.level.clone();
    let fw_visible = fw.visible.clone();

    iced::application("UDP2Mic", AppState::update, AppState::view)
        .subscription(AppState::subscription)
        .default_font(iced::Font::with_name("Microsoft YaHei"))
        .theme(|_| Theme::Dark)
        .window(iced::window::Settings {
            size: iced::Size::new(380.0, 300.0),
            position: iced::window::Position::Centered,
            resizable: false,
            ..Default::default()
        })
        .run_with(move || {
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
                float_window_enabled: cfg.float_window_enable != 0,
                fw_level,
                fw_visible,
                config: cfg,
                last_toggle_instant: now,
                session_id: 0,
            };
            (state, Task::none())
        })
}

impl AppState {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                if self.float_window_enabled {
                    self.fw_visible.store(true, Ordering::Relaxed);
                    let scaled = ((self.last_level_db + 60.0).max(0.0).min(60.0) * 100.0) as u32;
                    self.fw_level.store(scaled, Ordering::Relaxed);
                } else {
                    self.fw_visible.store(false, Ordering::Relaxed);
                }
                Task::none()
            }
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
            Message::ToggleFloatWindow(enabled) => {
                self.float_window_enabled = enabled;
                self.config.float_window_enable = if enabled { 1 } else { 0 };
                let _ = self.config.save();
                Task::none()
            }
            Message::ToggleRunning => {
                if self.last_toggle_instant.elapsed() < Duration::from_millis(200) {
                    return Task::none();
                }
                self.last_toggle_instant = Instant::now();
                self.session_id = self.session_id.wrapping_add(1); // 确保每次启停分配新ID，防止内部泄漏

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
                        border: iced::Border {
                            radius: 6.0.into(),
                            ..Default::default()
                        },
                        shadow: Default::default(),
                    }
                })
        };

        let btn_c = if self.is_running { red } else { accent };
        let auto_c = if self.config.is_auto_start() { accent } else { grey };
        let flt_c = if self.float_window_enabled { accent } else { grey };

        let db = self.last_level_db.max(-60.0).min(0.0);
        let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);

        container(
            column![
                row![
                    text("UDP2Mic").size(20),
                    Space::new(Length::Fill, 0),
                    text(st).color(sc).size(12),
                    Space::new(10, 0),
                    vb,
                ]
                .align_y(Alignment::Center),
                Space::new(0, 14),
                
                container(
                    column![
                        text(if self.is_running { "当前正在监听端口" } else { "配置局域网监听地址" }).size(11).color(dim),
                        Space::new(0, 4),
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
                                    .width(70)
                                    .padding([5, 8])
                                    .style(move |_, status| {
                                        let base = iced::widget::text_input::default(&iced::theme::Theme::Dark, status);
                                        iced::widget::text_input::Style {
                                            background: iced::Background::Color(Color::from_rgb(0.18, 0.18, 0.22)),
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
                
                Space::new(0, 12),
                
                container(
                    column![
                        row![
                            column![
                                text("传输码率").size(10).color(dim),
                                Space::new(0, 2),
                                row![
                                    text(format!("{:.0}", self.last_bitrate as u32)).size(24),
                                    Space::new(3, 0),
                                    text("kbps").size(11).color(dim)
                                ].align_y(Alignment::End)
                            ].width(Length::FillPortion(1)),
                            column![
                                text("声音电平").size(10).color(dim),
                                Space::new(0, 2),
                                row![
                                    text(format!("{:.0}", self.last_level_db as i32)).size(24),
                                    Space::new(3, 0),
                                    text("dB").size(11).color(dim)
                                ].align_y(Alignment::End)
                            ].width(Length::FillPortion(1)),
                        ],
                        Space::new(0, 8),
                        progress_bar(0.0..=1.0, frac as f32)
                            .height(4)
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
                
                Space::new(0, 14),
                
                row![
                    mk(
                        if self.is_running { "停止" } else { "启动" },
                        btn_c,
                        Message::ToggleRunning,
                    ).width(Length::FillPortion(1)),
                    Space::new(8, 0),
                    mk(
                        "开机自启",
                        auto_c,
                        Message::ToggleAutoStart(!self.config.is_auto_start()),
                    ).width(Length::FillPortion(1)),
                    Space::new(8, 0),
                    mk(
                        "桌面浮窗",
                        flt_c,
                        Message::ToggleFloatWindow(!self.float_window_enabled),
                    ).width(Length::FillPortion(1)),
                ],
                
                Space::new(0, 8),
                
                row![
                    text("广播地址: 255.255.255.255:44043").size(10).color(grey),
                    Space::new(Length::Fill, 0),
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
        let tick = iced::time::every(std::time::Duration::from_millis(500)).map(|_| Message::Tick);
        if self.is_running {
            let ip = self.config.listen_ip.clone();
            let port = self.config.listen_port;
            Subscription::batch([tick, Subscription::run_with_id(UdpReceiverId(self.session_id), udp_receiver_stream(ip, port))])
        } else {
            tick
        }
    }
}

fn udp_receiver_stream(listen_ip: String, listen_port: u32) -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(64, move |mut output| async move {

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

        // 获取音频后台初始化状态
        let mut retries = 10;
        while AUDIO_INIT_STATE.load(Ordering::Relaxed) == 0 && retries > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            retries -= 1;
        }

        let init_st = AUDIO_INIT_STATE.load(Ordering::Relaxed);
        if init_st == 1 {
            let _ = output.send(Message::StatusUpdate(StatusInfo { bitrate_kbps: 0.0, level_db: -60.0, error_msg: Some("Opus解码器初始化失败") })).await;
            return;
        } else if init_st == 2 {
            let _ = output.send(Message::StatusUpdate(StatusInfo { bitrate_kbps: 0.0, level_db: -60.0, error_msg: Some("音频初始化失败") })).await;
            return;
        }

        // 通知音频后台：清理状态、准备接入全新传输连接
        if let Some(tx) = AUDIO_TX.get() {
            let _ = tx.try_send(AudioMessage::Reset);
        }

        let mut buf = vec![0u8; protocol::MAX_PACKET];
        let mut last_stats = Instant::now();
        let mut byte_count: u64 = 0;
        let mut last_packet = Instant::now();
        let mut was_disconnected = false;

        loop {
            if output.is_closed() {
                break;
            }

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

            if len < protocol::HEADER_SIZE { continue; }

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

            // ✅ 将封包发送给后台 Audio Worker 线程
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

                // 从 Audio Worker 共享内存中提取最新的电平数据
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