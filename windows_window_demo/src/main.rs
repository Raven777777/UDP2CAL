#![windows_subsystem = "windows"]

use iced::futures::SinkExt;
use iced::widget::{button, checkbox, column, container, row, text};
use iced::{window, Element, Length, Subscription, Task, Theme};
use std::sync::OnceLock;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

const TRAY_QUIT_ID: &str = "quit";
const WINDOW_TITLE: &str = "Windows 窗口行为 Demo";

#[derive(Debug, Clone, Copy)]
enum TrayCommand {
    Restore,
    Quit,
}

static TRAY_COMMANDS: OnceLock<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<TrayCommand>>> =
    OnceLock::new();

#[derive(Debug, Clone)]
enum Message {
    ToggleMaximize,
    ToggleFullscreen,
    Minimize,
    HideToTray,
    ToggleCloseToTray(bool),
    RestoreFromTray,
    Quit,
}

fn main() -> Result<(), iced::Error> {
    let tray = install_tray();

    let result = iced::application(Demo::default, Demo::update, Demo::view)
        .title(WINDOW_TITLE)
        .subscription(Demo::subscription)
        .theme(Theme::Dark)
        .window(window::Settings {
            size: iced::Size::new(620.0, 380.0),
            position: window::Position::Centered,
            resizable: true,
            exit_on_close_request: false,
            ..Default::default()
        })
        .run();

    drop(tray);
    result
}

#[derive(Default)]
struct Demo {
    maximized: bool,
    fullscreen: bool,
    close_to_tray: bool,
}

impl Demo {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ToggleMaximize => {
                self.maximized = !self.maximized;
                latest_window_task(window::toggle_maximize)
            }
            Message::ToggleFullscreen => {
                self.fullscreen = !self.fullscreen;
                let mode = if self.fullscreen {
                    window::Mode::Fullscreen
                } else {
                    window::Mode::Windowed
                };
                latest_window_task(move |id| window::set_mode(id, mode))
            }
            Message::Minimize => latest_window_task(|id| window::minimize(id, true)),
            Message::HideToTray => {
                latest_window_task(|id| window::set_mode(id, window::Mode::Hidden))
            }
            Message::ToggleCloseToTray(value) => {
                self.close_to_tray = value;
                Task::none()
            }
            Message::RestoreFromTray => {
                let fullscreen = self.fullscreen;
                let mode = if fullscreen {
                    window::Mode::Fullscreen
                } else {
                    window::Mode::Windowed
                };
                latest_window_task(move |id| {
                    Task::batch([window::set_mode(id, mode), window::gain_focus(id)])
                })
            }
            Message::Quit => iced::exit(),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let state = if self.fullscreen {
            "全屏"
        } else if self.maximized {
            "最大化"
        } else {
            "普通窗口"
        };

        let controls = row![
            button("最大化").on_press(Message::ToggleMaximize),
            button(if self.fullscreen {
                "退出全屏"
            } else {
                "全屏"
            })
            .on_press(Message::ToggleFullscreen),
            button("最小化到任务栏").on_press(Message::Minimize),
            button("隐藏到托盘").on_press(Message::HideToTray),
        ]
        .spacing(12);

        container(
            column![
                text("Windows 窗口行为 Demo").size(30),
                checkbox(self.close_to_tray)
                    .label("关闭时最小化到托盘")
                    .on_toggle(Message::ToggleCloseToTray),
                text("默认点击窗口 X 直接退出；勾选后点击 X 隐藏到托盘。"),
                text(format!("当前状态：{state}")),
                controls,
                text("托盘：双击图标恢复窗口；右键菜单 → 退出。"),
            ]
            .spacing(22),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .padding(32)
        .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            window::close_requests()
                .with(self.close_to_tray)
                .map(|(close_to_tray, _)| {
                    if close_to_tray {
                        Message::HideToTray
                    } else {
                        Message::Quit
                    }
                }),
            tray_event_stream(),
        ])
    }
}

fn latest_window_task<F>(mut operation: F) -> Task<Message>
where
    F: FnMut(window::Id) -> Task<Message> + Send + 'static,
{
    window::latest().then(move |id| match id {
        Some(id) => operation(id),
        None => Task::none(),
    })
}

fn install_tray() -> tray_icon::TrayIcon {
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
            if event.id == TRAY_QUIT_ID {
                let _ = sender.try_send(TrayCommand::Quit);
            }
        },
    ));

    let menu = tray_icon::menu::Menu::new();
    let quit = tray_icon::menu::MenuItemBuilder::new()
        .id(TRAY_QUIT_ID.into())
        .text("退出")
        .enabled(true)
        .build();
    let _ = menu.append(&quit);

    let icon =
        tray_icon::Icon::from_rgba(make_icon(), 32, 32).expect("generated tray icon must be valid");
    tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip(WINDOW_TITLE)
        .with_icon(icon)
        .build()
        .expect("tray icon must be created")
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

unsafe extern "system" fn find_window_for_process(hwnd: HWND, data: LPARAM) -> BOOL {
    let search = &mut *(data.0 as *mut WindowSearch);
    let mut process_id = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    if process_id == search.process_id {
        search.window = hwnd;
        BOOL(0)
    } else {
        BOOL(1)
    }
}

fn tray_event_stream() -> Subscription<Message> {
    Subscription::run(|| {
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
                            Message::RestoreFromTray
                        }
                        TrayCommand::Quit => Message::Quit,
                    };
                    if output.send(message).await.is_err() {
                        break;
                    }
                }
            },
        )
    })
}

fn make_icon() -> Vec<u8> {
    let mut rgba = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32 {
        for x in 0..32 {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            let distance = (dx * dx + dy * dy).sqrt();
            if distance < 14.0 {
                rgba.extend_from_slice(&[0, 204, 115, 255]);
            } else if distance < 15.0 {
                rgba.extend_from_slice(&[0, 204, 115, 128]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    rgba
}
