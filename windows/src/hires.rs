//! 高精度等待（借鉴 Sunshine 的 `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`）。
//!
//! Windows 的 `Sleep` / `WaitForSingleObject` 超时精度受**系统定时器分辨率**限制，
//! 默认是 15.6 ms（一个 64 Hz tick）。反向音频的发送节拍是 10 ms 网格，用毫秒级
//! 等待就会出现"睡 10 ms 实际睡 15.6 ms"的量化误差：包成簇发出、中间空一段，
//! 接收端水位呈锯齿，阶跃吸收被动触发。
//!
//! 两种解法：
//! * `timeBeginPeriod(1)`：全局提高系统定时器分辨率——影响整机功耗，且是进程级
//!   全局状态，退出没配对还会残留；
//! * 可等待计时器 + `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`（Win10 1803+）：
//!   只让这一个句柄高精度，不动全局状态。**本项目采用后者。**
//!
//! 失败时返回 `None`，调用方退回原有的毫秒级等待（功能不受影响，只是精度变差）。

use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimerEx, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};

/// 一次等待最多睡多久（毫秒）。真实的 `due` 由 `SetWaitableTimerEx` 指定，
/// 这里只是给 `WaitForSingleObject` 一个上界：即使计时器句柄异常失效，
/// 线程也不会永久挂死。
const WAIT_CAP_MS: u32 = 50;

/// 高精度一次性等待定时器（自动复位语义：每次 `wait_until` 重新武装）。
pub struct HiResTimer {
    handle: HANDLE,
}

// SAFETY: `HANDLE` 是可等待计时器句柄，本身不绑定线程；Windows 允许任意线程
// 对该句柄调用 `SetWaitableTimerEx` / `WaitForSingleObject`。结构体不提供共享
// 引用下的内部可变状态（`wait_until` 对句柄的操作由内核串行化）。
unsafe impl Send for HiResTimer {}
unsafe impl Sync for HiResTimer {}

impl HiResTimer {
    /// 创建高精度定时器；系统不支持（Win10 1803 之前）时返回 `None`。
    pub fn new() -> Option<Self> {
        // SAFETY: 无指针参数（属性为 NULL），名称不关心（匿名定时器），
        // 句柄在 Drop 中配对 CloseHandle。
        let handle = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .ok()?;
        Some(Self { handle })
    }

    /// 睡到 `deadline`（`Instant` 语义，已是过去则立即返回）。
    ///
    /// 返回实际醒来时刻，便于上层统计迟到量（Sunshine 的 "Sleep overshoot"）。
    pub fn wait_until(&self, deadline: Instant) -> Instant {
        let now = Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            return now;
        };
        // `SetWaitableTimerEx` 的 due time 是 100 ns 单位，负数表示相对时间。
        // 向上取整到 100 ns：早醒会让 10 ms 网格重新积累漂移。
        let units = (remaining.as_nanos().saturating_add(99) / 100).min(i64::MAX as u128) as i64;
        let due = -units.max(1);
        // SAFETY: 句柄由 `new` 创建且在 `self` 生命周期内有效；`ptimerstate`
        // 指向栈上一个局部 `i64`，调用期间不会被移动；其余参数为 NULL/0。
        unsafe {
            if SetWaitableTimerEx(self.handle, &due, 0, None, None, None, 0).is_err() {
                // 武装失败：退回可被 tick 量化的粗睡，绝不空转
                std::thread::sleep(remaining.min(Duration::from_millis(WAIT_CAP_MS as u64)));
                return Instant::now();
            }
            let ms = remaining
                .as_millis()
                .saturating_add(2)
                .clamp(1, WAIT_CAP_MS as u128) as u32;
            WaitForSingleObject(self.handle, ms);
        }
        Instant::now()
    }
}

impl Drop for HiResTimer {
    fn drop(&mut self) {
        // SAFETY: 句柄由 CreateWaitableTimerExW 返回，此处是唯一一次释放，
        // 且 `self` 独占该句柄（未被复制/转移）。
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_until_past_deadline_returns_immediately() {
        let Some(timer) = HiResTimer::new() else {
            return; // 环境不支持高精度定时器时跳过（不在测试里误报失败）
        };
        let past = Instant::now() - Duration::from_millis(50);
        let start = Instant::now();
        timer.wait_until(past);
        assert!(
            start.elapsed() < Duration::from_millis(2),
            "过去时刻必须立即返回，实际 {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn wait_until_never_returns_early() {
        let Some(timer) = HiResTimer::new() else {
            return;
        };
        for _ in 0..10 {
            let start = Instant::now();
            let woke = timer.wait_until(start + Duration::from_millis(3));
            assert!(
                woke.duration_since(start) >= Duration::from_millis(2),
                "提前醒来：请求 3 ms，实际 {:?}",
                woke.duration_since(start)
            );
        }
    }

    #[test]
    fn wait_until_stays_within_tick_quantization_budget() {
        let Some(timer) = HiResTimer::new() else {
            return;
        };
        // 10 次 3 ms 的等待，若仍被 15.6 ms tick 量化，总耗时必然 > 150 ms。
        let start = Instant::now();
        for _ in 0..10 {
            timer.wait_until(Instant::now() + Duration::from_millis(3));
        }
        let total = start.elapsed();
        assert!(
            total < Duration::from_millis(120),
            "10×3 ms 等待耗时 {total:?}，疑似仍在用 15.6 ms tick 量化"
        );
    }
}
