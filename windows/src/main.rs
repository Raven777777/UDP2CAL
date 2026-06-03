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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK, MB_ICONINFORMATION};
use windows::core::PCWSTR;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UdpReceiverId;

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
    is_running: bool,
    connected: bool,
    last_bitrate: f32,
    last_level_db: f32,
    status_text: String,
    vb_cable_installed: bool, // 仅作为状态缓存，不再高频刷新
    float_window_enabled: bool,
    fw_level: Arc<AtomicU32>,
    fw_visible: Arc<AtomicBool>,
}

fn main() -> Result<(), iced::Error> {
    // 单实例检测
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
            let state = AppState {
                ip_input: cfg.listen_ip.clone(),
                port_input: cfg.listen_port.to_string(),
                is_running: false,
                connected: false,
                last_bitrate: 0.0,
                last_level_db: -60.0,
                status_text: String::new(),
                vb_cable_installed: audio::detect_vb_cable().is_some(), // 仅启动时检测一次
                float_window_enabled: cfg.float_window_enable != 0,
                fw_level,
                fw_visible,
                config: cfg,
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
                if self.is_running {
                    self.is_running = false;
                    self.connected = false;
                    self.last_bitrate = 0.0;
                    self.last_level_db = -60.0;
                    self.status_text.clear();
                } else {
                    self.config.listen_ip = self.ip_input.clone();
                    self.config.listen_port = self.port_input.parse().unwrap_or(8899);
                    let _ = self.config.save();
                    self.is_running = true;
                    self.connected = false;
                    self.last_bitrate = 0.0;
                    self.last_level_db = -60.0;
                    self.status_text = "等待连接...".into();
                    // 每次启动时顺便更新一次设备状态
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
        let bg = Color::from_rgb(0.10, 0.10, 0.12);
        let accent = Color::from_rgb(0.0, 0.80, 0.45);
        let red = Color::from_rgb(0.95, 0.25, 0.30);
        let grey = Color::from_rgb(0.45, 0.45, 0.50);
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
                text("●").color(Color::from_rgb(0.3, 0.85, 0.35)).size(10),
                text("VB-Cable").size(11).color(dim),
            ]
            .spacing(4)
            .into()
        } else {
            text("VB-Cable 未安装").size(11).color(Color::from_rgb(0.85, 0.6, 0.2)).into()
        };

        let mk = |label: &'static str, color: Color, msg: Message| {
            button(label)
                .on_press(msg)
                .width(Length::Fill)
                .height(32)
                .style(move |_, _| iced::widget::button::Style {
                    background: Some(iced::Background::Color(color)),
                    text_color: Color::WHITE,
                    border: iced::Border {
                        radius: 6.0.into(),
                        ..Default::default()
                    },
                    shadow: Default::default(),
                })
        };

        let btn_c = if self.is_running { red } else { accent };
        let auto_c = if self.config.auto_start != 0 { accent } else { grey };
        let flt_c = if self.float_window_enabled { accent } else { grey };

        let db = self.last_level_db.max(-60.0).min(0.0);
        let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);



        container(
            column![
                row![
                    text("UDP2Mic").size(22),
                    Space::new(Length::Fill, 0),
                    text(st).color(sc).size(13),
                    Space::new(12, 0),
                    vb,
                ]
                .align_y(Alignment::Center),
                Space::new(0, 12),
                column![
                    text("监听地址").size(11).color(dim),
                    row![
                        text_input("0.0.0.0", &self.ip_input)
                            .on_input(Message::IpChanged)
                            .width(140),
                        text(" : ").size(16).color(dim),
                        text_input("8899", &self.port_input)
                            .on_input(Message::PortChanged)
                            .width(64),
                    ]
                    .align_y(Alignment::Center),
                ]
                .spacing(4),
                Space::new(0, 16),
                column![
                    row![
                        column![text(format!("{:.0}", self.last_bitrate as u32)).size(28), text("kbps").size(11).color(dim)].width(90),
                        column![text(format!("{:.0}", self.last_level_db as i32)).size(28), text("dB").size(11).color(dim)].width(90),
                    ]
                    .spacing(20),
                    Space::new(0, 6),
                    progress_bar(0.0..=1.0, frac as f32).height(3),
                ]
                .spacing(2),
                Space::new(0, 20),
                row![
                    mk(
                        if self.is_running { "停止" } else { "启动" },
                        btn_c,
                        Message::ToggleRunning,
                    )
                    .width(Length::FillPortion(1)),
                    Space::new(8, 0),
                    mk(
                        "自启",
                        auto_c,
                        Message::ToggleAutoStart(!(self.config.auto_start != 0)),
                    )
                    .width(Length::FillPortion(1)),
                    Space::new(8, 0),
                    mk(
                        "浮窗",
                        flt_c,
                        Message::ToggleFloatWindow(!self.float_window_enabled),
                    )
                    .width(Length::FillPortion(1)),
                ],
            ]
            .padding(18),
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
            Subscription::batch([tick, Subscription::run_with_id(UdpReceiverId, udp_receiver_stream())])
        } else {
            tick
        }
    }
}

fn udp_receiver_stream() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(64, |mut output| async move {

        let cfg = config::Config::load();
        let bind_addr = format!("{}:{}", cfg.listen_ip, cfg.listen_port);

        let socket = match tokio::net::UdpSocket::bind(&bind_addr).await {
            Ok(s) => s,
            Err(_) => {
                let _ = output
                    .send(Message::StatusUpdate(StatusInfo {
                        bitrate_kbps: 0.0,
                        level_db: -60.0,
                        error_msg: Some("绑定失败"),
                    }))
                    .await;
                return;
            }
        };

        let mut dec = match decoder::OpusDecoder::new() {
            Ok(d) => d,
            Err(_) => {
                let _ = output
                    .send(Message::StatusUpdate(StatusInfo {
                        bitrate_kbps: 0.0,
                        level_db: -60.0,
                        error_msg: Some("Opus解码器初始化失败"),
                    }))
                    .await;
                return;
            }
        };

        let mut aw = match audio::start_audio() {
            Ok(w) => w,
            Err(_) => {
                let _ = output
                    .send(Message::StatusUpdate(StatusInfo {
                        bitrate_kbps: 0.0,
                        level_db: -60.0,
                        error_msg: Some("音频初始化失败"),
                    }))
                    .await;
                return;
            }
        };

        let mut rb = protocol::ReorderBuffer::new();
        let mut buf = vec![0u8; protocol::MAX_PACKET];
        let mut last_stats = Instant::now();
        let mut byte_count: u64 = 0;
        let mut rms_sum: f64 = 0.0;
        let mut rms_count: u64 = 0;
        let mut last_packet = Instant::now();
        let mut was_disconnected = false;

        loop {
            if output.is_closed() {
                break;
            }

            let result = tokio::time::timeout(
                std::time::Duration::from_millis(300),
                socket.recv_from(&mut buf),
            )
            .await;

            let (len, _src) = match result {
                Ok(Ok(v)) => v,
                _ => {
                    if !was_disconnected && last_packet.elapsed() > std::time::Duration::from_secs(3) {
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
            // Android 重连后自动恢复连接状态
            if was_disconnected {
                was_disconnected = false;
                let _ = output.send(Message::StatusUpdate(StatusInfo {
                    bitrate_kbps: 0.0,
                    level_db: -60.0,
                    error_msg: Some("已重新连接"),
                })).await;
            }
            byte_count += len as u64;

            rb.insert_and_drain(h.seq_num, h.sample_rate, &buf[protocol::HEADER_SIZE..len], |sr, payload| {
                if let Some((n, sr_hz)) = dec.decode(sr, payload) {
                    let pcm = dec.pcm_data();
                    let samples = &pcm[..n.min(pcm.len())];
                    rms_sum += samples.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>();
                    rms_count += samples.len() as u64;
                    aw.write(samples, sr_hz);
                }
            });

            if last_stats.elapsed() > std::time::Duration::from_millis(500) {
                let elapsed = last_stats.elapsed().as_secs_f32();
                let kbps = if elapsed > 0.0 {
                    (byte_count * 8) as f32 / elapsed / 1000.0
                } else {
                    0.0
                };
                let level_db = if rms_count > 0 {
                    let rms = (rms_sum / rms_count as f64).sqrt() as f32;
                    20.0 * (rms.max(1e-10)).log10()
                } else {
                    -60.0
                };

                byte_count = 0;
                rms_sum = 0.0;
                rms_count = 0;
                last_stats = Instant::now();
                
                // 【大刀阔斧删除】彻底移除 dec.shrink_pcm_buf()
                // Opus 解码器的 pcm_buf 在初始化时已经预分配了最大空间，其容量永远不会超过 MAX_FRAME_SAMPLES
                // 每秒调用 2 次 shrink_pcm_buf 是纯粹的性能浪费

                let _ = output
                    .send(Message::StatusUpdate(StatusInfo {
                        bitrate_kbps: kbps,
                        level_db,
                        error_msg: None,
                    }))
                    .await;
            }
        }
    })
}
