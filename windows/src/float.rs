use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM, COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, GetDC, GetStockObject, ReleaseDC, 
    SetBkMode, SetTextColor, TextOutW, GRAY_BRUSH, TRANSPARENT, HBRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, LoadCursorW, 
    PeekMessageW, PostQuitMessage, RegisterClassW, SetLayeredWindowAttributes, ShowWindow, 
    TranslateMessage, DispatchMessageW, CS_HREDRAW, CS_VREDRAW, IDC_ARROW, LWA_ALPHA, 
    MSG, PM_REMOVE, SM_CXSCREEN, SW_HIDE, SW_SHOW, WM_DESTROY, WM_NCHITTEST, 
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE, WNDCLASSW, 
    HTCLIENT, HTCAPTION,
};

pub struct FloatWindow {
    running: Arc<AtomicBool>,
    pub level: Arc<AtomicU32>,
    pub visible: Arc<AtomicBool>,
}

impl FloatWindow {
    pub fn new() -> Self {
        let l = Arc::new(AtomicU32::new(0));
        let v = Arc::new(AtomicBool::new(false));
        let r = Arc::new(AtomicBool::new(true));
        
        let r2 = r.clone();
        let l2 = l.clone();
        let v2 = v.clone();
        
        std::thread::Builder::new()
            .name("udp2mic-fw".into())
            .spawn(move || fw_main(r2, l2, v2))
            .ok();

        Self {
            running: r,
            level: l,
            visible: v,
        }
    }
}

impl Drop for FloatWindow {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

// 【大刀阔斧改进】纯栈分配格式化，杜绝 UI 渲染中的堆内存分配
fn format_db_stack(db_val: f32) -> [u16; 16] {
    let mut buf = [0u16; 16];
    let mut pos = 0;
    let int_val = db_val as i32;

    if int_val < 0 {
        buf[pos] = '-' as u16;
        pos += 1;
    }

    let abs_val = int_val.unsigned_abs();
    if abs_val == 0 {
        buf[pos] = '0' as u16;
        pos += 1;
    } else {
        let mut digits = [0u8; 10];
        let mut d_pos = 0;
        let mut v = abs_val;

        while v > 0 {
            digits[d_pos] = (v % 10) as u8;
            v /= 10;
            d_pos += 1;
        }

        for i in (0..d_pos).rev() {
            buf[pos] = (digits[i] + b'0') as u16;
            pos += 1;
        }
    }

    // 追加 " dB"
    for c in " dB".chars() {
        if pos < 15 {
            buf[pos] = c as u16;
            pos += 1;
        }
    }
    
    buf
}

fn fw_main(running: Arc<AtomicBool>, level: Arc<AtomicU32>, visible: Arc<AtomicBool>) {
    std::thread::sleep(std::time::Duration::from_millis(200));
    
    unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(fw_proc),
            hInstance: inst.into(),
            lpszClassName: windows::core::w!("LmFlo\0"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(GetStockObject(GRAY_BRUSH).0),
            ..Default::default()
        };

        if RegisterClassW(&wc) == 0 {
            return;
        }

        let sw = GetSystemMetrics(SM_CXSCREEN);
        let (ww, wh) = (260i32, 28i32);
        let margin = 12i32;

        let hwnd = match CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            windows::core::w!("LmFlo\0"),
            windows::core::w!(""),
            WS_POPUP | WS_VISIBLE,
            sw - ww - margin,
            margin,
            ww,
            wh,
            None,
            None,
            inst,
            None,
        ) {
            Ok(h) => h,
            _ => return,
        };

        SetLayeredWindowAttributes(hwnd, COLORREF(0u32), 200, LWA_ALPHA).ok();
        let _ = ShowWindow(hwnd, SW_HIDE);

        let mut last_lvl = u32::MAX;
        let mut last_vis = false;

        loop {
            if !running.load(Ordering::Relaxed) {
                break;
            }

            let vis = visible.load(Ordering::Relaxed);
            let lvl = level.load(Ordering::Relaxed);

            // 显隐状态更新
            if vis != last_vis {
                let _ = ShowWindow(hwnd, if vis { SW_SHOW } else { SW_HIDE });
                last_vis = vis;
            }

            // 渲染层：当窗口可见且音量电平发生变化时重新绘制
            if vis && lvl != last_lvl {
                last_lvl = lvl;
                let dc = GetDC(hwnd);

                if !dc.is_invalid() {
                    // 1. 绘制背景
                    let bg_brush = CreateSolidBrush(COLORREF(0u32));
                    FillRect(dc, &RECT { left: 0, top: 0, right: ww, bottom: wh }, bg_brush);
                    let _ = DeleteObject(bg_brush);

                    // 2. 计算并绘制音量条 (绿 -> 黄 -> 红 动态变色)
                    let frac = (lvl as f32 / 6000.0).min(1.0);
                    let bw = ((frac * (ww - 2) as f32) as i32).max(0);
                    
                    let color = if frac < 0.3 {
                        0x00FF00u32 // 绿色
                    } else if frac < 0.7 {
                        0x00CCFFu32 // 黄色/橙色 (Windows BGR 格式)
                    } else {
                        0x0000FFu32 // 红色
                    };

                    let bar_brush = CreateSolidBrush(COLORREF(color));
                    FillRect(dc, &RECT { left: 1, top: 2, right: 1 + bw, bottom: wh - 2 }, bar_brush);
                    let _ = DeleteObject(bar_brush);

                    // 3. 【核心修复】纯栈格式化文本并绘制，零堆分配
                    let db_val = (lvl as f32 / 100.0) - 60.0;
                    let w = format_db_stack(db_val);

                    SetBkMode(dc, TRANSPARENT);
                    SetTextColor(dc, COLORREF(0xFFFFFFu32));
                    let _ = TextOutW(dc, 6, 5, &w);

                    ReleaseDC(hwnd, dc);
                }
            }

            // Windows 消息循环处理（防止窗口卡死）
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_DESTROY {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        let _ = DestroyWindow(hwnd);
    }
}

unsafe extern "system" fn fw_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        // 实现点击窗口任意位置均可拖动窗口 (将 Client 区域伪装成 Caption 标题栏)
        WM_NCHITTEST => {
            let r = DefWindowProcW(hwnd, msg, wp, lp);
            if r.0 as u32 == HTCLIENT {
                LRESULT(HTCAPTION as isize)
            } else {
                r
            }
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}