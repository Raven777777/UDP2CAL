//! iced 界面渲染。

use crate::app::{AppState, Message, NoticeLevel};
use crate::diag::LATENCY_MS;
use crate::i18n::{t, tf};
use iced::widget::{
    Space, button, column, container, opaque, progress_bar, row, stack, text, text_input,
};
use iced::{Alignment, Color, Element, Length, Task};
use std::sync::atomic::Ordering;

impl AppState {
    pub(crate) fn view(&self) -> Element<'_, Message> {
        let bg = Color::from_rgb(0.08, 0.08, 0.10);
        let card_bg = Color::from_rgb(0.13, 0.13, 0.16);
        let border_color = Color::from_rgb(0.20, 0.20, 0.25);
        let accent = Color::from_rgb(0.0, 0.80, 0.45);
        let red = Color::from_rgb(0.95, 0.25, 0.30);
        let grey = Color::from_rgb(0.40, 0.40, 0.45);
        let dim = Color::from_rgb(0.55, 0.55, 0.60);

        let (sc, st) = if !self.is_running {
            (grey, t("status.stopped"))
        } else if self.connected {
            (accent, t("status.connected"))
        } else {
            (
                red,
                if self.disconnected {
                    t("status.disconnected")
                } else {
                    t("status.waiting")
                },
            )
        };

        // P2P 状态: TYPE_CONNECT 独占绑定 / 空闲
        // 已占用后不再拼接 8 位设备识别码：对用户没有意义，还会把顶部一行挤满。
        let p2p = self.session.p2p();
        let (p2p_state, p2p_color) = if !self.is_running {
            (String::new(), dim)
        } else if p2p.is_busy() {
            (t("status.p2p_busy"), Color::from_rgb(0.95, 0.55, 0.15))
        } else {
            (t("status.p2p_idle"), accent)
        };

        let vb_str = if self.vb_cable_installed {
            t("status.vb_cable")
        } else {
            t("status.vb_cable_missing")
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
            let s = self.session.audio_stats().snapshot();
            // 延迟放在最前：它是用户唯一能直接感知的指标，
            // 重采样计数已失去诊断价值（比率每帧都在动），只在日志里保留
            tf(
                "footer.latency",
                &[
                    ("ms", &LATENCY_MS.load(Ordering::Relaxed)),
                    ("dropped", &short_num(s.dropped)),
                    ("underrun", &short_num(s.underrun)),
                ],
            )
        } else {
            t("footer.broadcast_addr")
        };
        let (footer_right, footer_right_color) = if !self.is_running {
            (String::new(), dim)
        } else if self.connected {
            if self.reverse_enabled {
                (t("footer.reverse_on"), Color::from_rgb(0.25, 0.80, 0.40))
            } else {
                (t("footer.reverse_off"), dim)
            }
        } else {
            let bc_on = self.session.running() && !p2p.is_busy();
            if bc_on {
                (t("footer.broadcasting"), accent)
            } else {
                (t("footer.muted"), dim)
            }
        };

        let mk = |label: String, color: Color, msg: Message| {
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

        let base = container(
            column![
                row![
                    text(t("app.tagline")).size(20),
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
                        t("ui.listen_port")
                    } else {
                        t("ui.target_address")
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
                            text(t("ui.bitrate")).size(12).color(dim),
                            Space::new().height(2),
                            row![
                                text(format!("{:.0}", self.last_bitrate as u32)).size(22),
                                Space::new().width(3),
                                text(t("unit.kbps")).size(11).color(dim)
                            ]
                            .align_y(Alignment::End)
                        ]
                        .width(Length::FillPortion(1)),
                        column![
                            text(t("ui.level")).size(12).color(dim),
                            Space::new().height(2),
                            row![
                                text(format!("{:.0}", self.last_level_db as i32)).size(22),
                                Space::new().width(3),
                                text(t("unit.db")).size(11).color(dim)
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
                        if self.is_running {
                            t("ui.stop")
                        } else {
                            t("ui.start")
                        },
                        btn_c,
                        Message::ToggleRunning,
                    )
                    .width(Length::FillPortion(1)),
                    Space::new().width(8),
                    mk(
                        t("ui.auto_start"),
                        auto_c,
                        Message::ToggleAutoStart(!self.config.is_auto_start()),
                    )
                    .width(Length::FillPortion(1)),
                    Space::new().width(8),
                    if self.is_running {
                        let rev_on = self.reverse_enabled;
                        button(
                            text(if rev_on {
                                t("ui.reverse_stream")
                            } else {
                                t("ui.not_enabled")
                            })
                            .size(16),
                        )
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
                            t("ui.reverse_stream"),
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
        });

        let base: Element<'_, Message> = base.into();
        let Some(req) = self.pending_pair else {
            return base;
        };
        let card = container(
            column![
                text(t("pair.title")).size(14).color(Color::WHITE),
                text(tf("pair.ip", &[("ip", &req.addr.ip())]))
                    .size(11)
                    .color(dim),
                text(tf(
                    "pair.device_id",
                    &[("id", &format!("{:016x}", req.device_id))],
                ))
                .size(10)
                .color(grey),
                text(t("pair.hint")).size(10).color(grey),
                Space::new().height(2),
                row![
                    mk(t("pair.allow"), accent, Message::AllowPairing)
                        .width(Length::FillPortion(1)),
                    Space::new().width(8),
                    mk(t("pair.deny"), red, Message::DenyPairing).width(Length::FillPortion(1)),
                ],
            ]
            .spacing(6),
        )
        .padding(14)
        .width(Length::Fixed(272.0))
        .style(move |_| container::Style {
            background: Some(iced::Background::Color(card_bg)),
            border: iced::Border {
                radius: 8.0.into(),
                color: border_color,
                width: 1.0,
            },
            ..Default::default()
        });
        let modal = opaque(
            container(card)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(iced::Background::Color(Color { a: 0.72, ..bg })),
                    ..Default::default()
                }),
        );
        stack(vec![base, modal]).into()
    }
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

pub(crate) fn latest_window_task<F>(mut operation: F) -> Task<Message>
where
    F: FnMut(iced::window::Id) -> Task<Message> + Send + 'static,
{
    iced::window::latest().then(move |id| match id {
        Some(id) => operation(id),
        None => Task::none(),
    })
}
