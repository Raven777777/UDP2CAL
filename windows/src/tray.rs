//! 系统托盘、图标与原生窗口激活。

use crate::app::Message;
use crate::i18n;
use crate::logf;
use iced::futures::SinkExt;
use std::sync::OnceLock;
// 预期延迟档位（ms），直接派生自协议常量，与 Android 高级设置保持一致。
use udp2cal_protocol::rtp::WATER_TARGET_STEPS_MS;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE,
    SYNCHRONIZATION_ACCESS_RIGHTS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, EnumWindows, FindWindowW, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsWindowVisible, SW_RESTORE, SetForegroundWindow, ShowWindow,
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

#[derive(Debug, Clone)]
enum TrayCommand {
    Restore,
    Quit,
    SetWaterTarget(u32),
    SetLanguage(String),
}

// 勾选态需要菜单项句柄；muda 的 CheckMenuItem 内部是 Rc（非 Send/Sync），
// 不能放进 static。菜单事件在创建菜单的线程消息泵里派发，因此按线程保存。
thread_local! {
    static WATER_TARGET_ITEMS: std::cell::RefCell<Vec<(u32, tray_icon::menu::CheckMenuItem)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// 托盘对象同样只能在创建它的线程上操作；语言切换时用它重建菜单。
    static TRAY_ICON: std::cell::RefCell<Option<tray_icon::TrayIcon>> =
        const { std::cell::RefCell::new(None) };
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
    language: &str,
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
            } else if event.id.0 == "lang-system" {
                let _ = sender.try_send(TrayCommand::SetLanguage(String::new()));
            } else if let Some(code) = event.id.0.strip_prefix("lang-") {
                let _ = sender.try_send(TrayCommand::SetLanguage(code.to_string()));
            }
        },
    ));

    let menu = build_menu(water_target_ms, language);

    let tray_icon_img = match tray_icon::Icon::from_rgba(icon_rgba, width, height) {
        Ok(icon) => icon,
        Err(e) => {
            logf!("[Tray] 图标构造失败: {e}");
            return None;
        }
    };
    match tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("UDP2CAL")
        .with_icon(tray_icon_img)
        .with_menu_on_left_click(false)
        .build()
    {
        Ok(tray) => {
            TRAY_ICON.with(|slot| *slot.borrow_mut() = Some(tray.clone()));
            Some(tray)
        }
        Err(e) => {
            logf!("[Tray] 托盘创建失败: {e}");
            None
        }
    }
}

fn build_menu(water_target_ms: u32, language: &str) -> tray_icon::menu::Menu {
    let tray_menu = tray_icon::menu::Menu::new();
    let delay_menu = tray_icon::menu::Submenu::with_id(
        "water-target-menu",
        i18n::t("menu.expected_latency"),
        true,
    );
    let mut delay_items = Vec::new();
    for ms in WATER_TARGET_STEPS_MS {
        let item = tray_icon::menu::CheckMenuItem::with_id(
            format!("water-{ms}"),
            format!("{ms}{}", i18n::t("unit.ms")),
            true,
            ms == water_target_ms,
            None,
        );
        let _ = delay_menu.append(&item);
        delay_items.push((ms, item));
    }
    WATER_TARGET_ITEMS.with(|slot| *slot.borrow_mut() = delay_items);
    let _ = tray_menu.append(&delay_menu);

    let language_menu = tray_icon::menu::Submenu::with_id(
        "language-menu",
        i18n::t("menu.language"),
        true,
    );
    let system_item = tray_icon::menu::CheckMenuItem::with_id(
        "lang-system",
        i18n::t("menu.system"),
        true,
        language.is_empty(),
        None,
    );
    let _ = language_menu.append(&system_item);
    let active = i18n::active_code();
    let explicit = !language.is_empty();
    for (code, name) in i18n::available() {
        let item = tray_icon::menu::CheckMenuItem::with_id(
            format!("lang-{code}"),
            name,
            true,
            explicit && active == code,
            None,
        );
        let _ = language_menu.append(&item);
    }
    let _ = tray_menu.append(&language_menu);

    let quit_item = tray_icon::menu::MenuItemBuilder::new()
        .id("quit".into())
        .text(i18n::t("menu.quit"))
        .enabled(true)
        .build();
    let _ = tray_menu.append(&quit_item);
    tray_menu
}

/// 语言切换后重建托盘菜单。托盘对象绑定在创建线程上；若当前线程没有它
/// （理论上不会发生），主窗口仍会更新，菜单在下一次启动时使用新语言。
pub(crate) fn refresh_menu(water_target_ms: u32, language: &str) {
    let menu = build_menu(water_target_ms, language);
    TRAY_ICON.with(|slot| match slot.try_borrow() {
        Ok(slot) => match slot.as_ref() {
            Some(tray) => tray.set_menu(Some(Box::new(menu))),
            None => logf!("[Tray] 语言菜单刷新跳过：托盘尚未创建"),
        },
        Err(_) => logf!("[Tray] 语言菜单刷新跳过：托盘状态被占用"),
    });
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

// ==========================================
// 单实例唤醒：重复启动通过命名事件请求已运行实例弹出窗口
//
// 不能由第二个实例直接 ShowWindow 裸激活：winit 记录的可见性标志不会
// 同步，之后点标题栏"X"时 set_mode(Hidden) 因 diff 为空而不执行隐藏，
// 窗口关不掉（只能靠托盘双击把 iced 的状态同步回来）。正确的路径是
// 已运行实例在自己的事件循环里弹出窗口——与托盘双击完全一致。
// ==========================================

/// 唤醒事件名（本地命名空间，按登录会话隔离）
const SHOW_EVENT_NAME: &str = "UDP2CAL_ShowWindow_Event";

/// 事件对象的 SYNCHRONIZE(0x00100000) 位；windows crate 未在 Threading 模块
/// 暴露该常量（只在 Storage::FileSystem 有 FILE_ACCESS_RIGHTS 版本），本地定义。
const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;

/// `HANDLE`（内含裸指针）不实现 `Send/Sync`，但内核对象句柄本身可跨线程使用；
/// 这里的句柄只在创建后只读共享，包装以放进静态存储。
#[derive(Clone, Copy)]
struct SharedHandle(HANDLE);
unsafe impl Send for SharedHandle {}
unsafe impl Sync for SharedHandle {}

impl SharedHandle {
    /// 无限期等待事件触发。内核对象句柄跨线程等待是安全的（等待方只是
    /// 引用对象，不要求归属线程）；闭包经由此方法整体捕获，避免因
    /// 字段级捕获（RFC 2229）把非 Send 的 `HANDLE` 直接搬进线程。
    fn wait_forever(self) -> windows::Win32::Foundation::WAIT_EVENT {
        unsafe { WaitForSingleObject(self.0, INFINITE) }
    }
}

static SHOW_EVENT: OnceLock<SharedHandle> = OnceLock::new();

/// 创建供后续实例发信号的自动复位命名事件，句柄泄漏到进程结束
/// （与单实例锁同生命周期，无需关闭）。
///
/// 不手工构造 DACL：能检测到"已在运行"（打开互斥锁成功）的进程，必然也被
/// 事件的默认 DACL 放行——互斥锁与事件都由同一 token 创建，默认授权包含
/// 同一 user/logon SID；手工放行 Everyone 反而比默认更宽。
///
/// 创建失败只留日志：第二个实例将回退到旧的直接激活路径，功能不受影响。
pub(crate) fn create_show_request_event() {
    if SHOW_EVENT.get().is_some() {
        return;
    }
    match create_named_show_event(SHOW_EVENT_NAME) {
        Ok(handle) => {
            let _ = SHOW_EVENT.set(SharedHandle(handle));
        }
        Err(e) => logf!("[IPC] 唤醒事件创建失败: {e}"),
    }
}

fn create_named_show_event(name: &str) -> windows::core::Result<HANDLE> {
    // SAFETY: `wide` 以 0 结尾，指针在调用期间有效；返回的句柄归调用方。
    unsafe {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        CreateEventW(None, false, false, PCWSTR::from_raw(wide.as_ptr()))
    }
}

/// 第二个实例调用：请求运行中的实例弹出主窗口。
///
/// 顺序关键：先 `AllowSetForegroundWindow` 让渡前台权限（本实例刚被用户
/// 启动、持有该权限；运行中的实例是后台进程，没有让渡就抢不到前台），
/// 再 SetEvent。返回 false 表示信号通道不可用（运行中的是旧版本、
/// 或事件未创建），调用方应回退到旧的直接激活路径。
pub(crate) fn signal_show_request() -> bool {
    unsafe {
        let wide: Vec<u16> = "UDP2CAL".encode_utf16().chain(std::iter::once(0)).collect();
        if let Ok(hwnd) = FindWindowW(PCWSTR::null(), PCWSTR::from_raw(wide.as_ptr())) {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != 0 {
                let _ = AllowSetForegroundWindow(pid);
            }
        }
        let name: Vec<u16> = SHOW_EVENT_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let Ok(event) = OpenEventW(
            SYNCHRONIZATION_ACCESS_RIGHTS(EVENT_MODIFY_STATE.0 | SYNCHRONIZE_ACCESS),
            false,
            PCWSTR::from_raw(name.as_ptr()),
        ) else {
            return false;
        };
        let signaled = SetEvent(event).is_ok();
        let _ = CloseHandle(event);
        signaled
    }
}

/// 订阅唤醒事件：第二个实例要求弹出窗口时，经 iced 事件循环处理
/// （解除最小化 + 同步可见性 + 聚焦，见 `Message::ShowWindow` 处理器）。
///
/// ⚠ 等待必须放在普通 `std::thread` 上，不能用 `tokio::task::spawn_blocking`：
/// iced 退出时 drop 内部的 tokio Runtime，而 Runtime 的 drop 会无限期等待
/// 所有 blocking 池任务结束——一个永远等在 `WaitForSingleObject(INFINITE)`
/// 上的任务会让整个进程退出卡死（表现为托盘"退出"无效、程序无响应）。
/// 普通线程不参与 tokio 生命周期，进程结束时随之终结。
///
/// ⚠ 两级通道都用**背压式**发送（`blocking_send` / `Sink::send`），绝不能
/// 用 `try_send` 遇错即退：iced 的订阅追踪器不会重建已结束的同 id 流，
/// 通道满（UI 忙时连续多次双开）就会让唤起功能对本次运行永久静默失效。
/// 这里堵一下只会阻塞等事件的线程，事件本身是自动复位的，不丢信号。
pub(crate) fn show_request_stream() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(
        4,
        |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            let Some(handle) = SHOW_EVENT.get().copied() else {
                return;
            };
            let (sender, mut receiver) = tokio::sync::mpsc::channel::<()>(4);
            if let Err(e) = std::thread::Builder::new()
                .name("udp2cal-show-request".into())
                .spawn(move || {
                    loop {
                        handle.wait_forever();
                        // iced 退出、接收端断开后线程在此退出；否则阻塞到
                        // 订阅端腾出槽位（背压），不丢信号。
                        if sender.blocking_send(()).is_err() {
                            break;
                        }
                    }
                })
            {
                logf!("[IPC] 唤醒等待线程启动失败: {e}");
                return;
            }
            while let Some(()) = receiver.recv().await {
                if output.send(Message::ShowWindow).await.is_err() {
                    break;
                }
            }
        },
    )
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
                    TrayCommand::SetLanguage(code) => Message::SetLanguage(code),
                };
                // 托盘事件是可重复的用户点击：UI 卡住时丢一次点击比阻塞托盘
                // 线程好；仅接收端已退出才退出转发循环。
                if let Err(e) = output.try_send(message)
                    && e.is_disconnected() {
                        break;
                    }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单实例唤醒事件必须可被后续实例打开并触发（默认 DACL 即可，无需手工 ACL）。
    /// 用独立测试名，避免与正在运行的本程序事件冲突。
    #[test]
    fn show_event_is_openable_and_signalable() {
        let name = format!("UDP2CAL_ShowWindow_Event_test_{}", std::process::id());
        let handle = create_named_show_event(&name).expect("创建唤醒事件失败");
        unsafe {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let opened = OpenEventW(
                SYNCHRONIZATION_ACCESS_RIGHTS(EVENT_MODIFY_STATE.0 | SYNCHRONIZE_ACCESS),
                false,
                PCWSTR::from_raw(wide.as_ptr()),
            )
            .expect("打开唤醒事件失败（访问权限过窄）");
            SetEvent(opened).expect("触发唤醒事件失败");
            let _ = CloseHandle(opened);
            // 测试自建自管的句柄，用完关闭（真实路径的句柄故意泄漏）。
            let _ = CloseHandle(handle);
        }
    }
}
