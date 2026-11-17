//! 系统托盘、图标与原生窗口激活。

use crate::app::Message;
use crate::logf;
use std::sync::OnceLock;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
    SW_RESTORE, SetForegroundWindow, ShowWindow,
};
use windows::core::{BOOL, PCWSTR};

// ==========================================
// 图标加载 (编译期嵌入 icon.png)
// ==========================================
pub(crate) fn load_icon_rgba() -> (Vec<u8>, u32, u32) {
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
    SetWaterTarget(u32),
}

/// 预期延迟档位（ms），与 Android 高级设置保持一致。
const WATER_TARGET_STEPS_MS: [u32; 5] = [80, 120, 160, 200, 300];

// 勾选态需要菜单项句柄；muda 的 CheckMenuItem 内部是 Rc（非 Send/Sync），
// 不能放进 static。菜单事件在创建菜单的线程消息泵里派发，因此按线程保存。
thread_local! {
    static WATER_TARGET_ITEMS: std::cell::RefCell<Vec<(u32, tray_icon::menu::CheckMenuItem)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

static TRAY_COMMANDS: OnceLock<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<TrayCommand>>> =
    OnceLock::new();

/// 创建系统托盘。失败（explorer 未就绪、远程会话、资源不足）返回 None，
/// 由调用方降级为"关窗即退出"，绝不能 panic：发布版没有控制台，
/// panic 的表现就是"双击图标毫无反应"。
pub(crate) fn install_tray(
    icon_rgba: Vec<u8>,
    width: u32,
    height: u32,
    water_target_ms: u32,
) -> Option<tray_icon::TrayIcon> {
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
            } else if let Some(ms) = event
                .id
                .0
                .strip_prefix("water-")
                .and_then(|s| s.parse::<u32>().ok())
            {
                WATER_TARGET_ITEMS.with(|slot| {
                    for (value, item) in slot.borrow().iter() {
                        item.set_checked(*value == ms);
                    }
                });
                let _ = sender.try_send(TrayCommand::SetWaterTarget(ms));
            }
        },
    ));

    let tray_menu = tray_icon::menu::Menu::new();
    let delay_menu =
        tray_icon::menu::Submenu::with_id("water-target-menu", "预期延迟", true);
    let mut delay_items = Vec::new();
    for ms in WATER_TARGET_STEPS_MS {
        let item = tray_icon::menu::CheckMenuItem::with_id(
            format!("water-{ms}"),
            format!("{ms}ms"),
            true,
            ms == water_target_ms,
            None,
        );
        let _ = delay_menu.append(&item);
        delay_items.push((ms, item));
    }
    // 二次安装（理论上不会发生）时覆盖旧句柄，保持勾选态更新指向当前菜单
    WATER_TARGET_ITEMS.with(|slot| *slot.borrow_mut() = delay_items);
    let _ = tray_menu.append(&delay_menu);
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
pub(crate) fn focus_existing_window(title: &str) {
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

pub(crate) fn restore_native_window() {
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
    let search = unsafe { &mut *(data.0 as *mut WindowSearch) };
    let mut process_id = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if process_id != search.process_id {
        return BOOL(1);
    }
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() || unsafe { GetWindowTextLengthW(hwnd) } == 0 {
        return BOOL(1);
    }
    search.window = hwnd;
    BOOL(0)
}

pub(crate) fn tray_event_stream() -> impl iced::futures::Stream<Item = Message> {
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
                    TrayCommand::SetWaterTarget(ms) => Message::SetWaterTarget(ms),
                };
                // 托盘事件是可重复的用户点击：UI 卡住时丢一次点击比阻塞托盘
                // 线程好；仅接收端已退出才退出转发循环。
                if let Err(e) = output.try_send(message) {
                    if e.is_disconnected() {
                        break;
                    }
                }
            }
        },
    )
}
