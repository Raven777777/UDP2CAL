//! Windows 电源与线程调度接入（P1）
//!
//! 本项目是常驻托盘/后台运行的接收端，仅靠 `SetThreadPriority` 不够：
//!   * Windows 11 的 EcoQoS 会把后台进程降频，声卡欠载表现为周期性卡顿；
//!   * 显示器关闭/系统空闲后，默认电源计划仍可能让系统进入低功耗或睡眠；
//!   * 单纯提高优先级不等于音频调度，MMCSS 才能给出正确的实时配额。
//!
//! 这里把三类 API 包成成对获取/释放的守卫：[`ProcessPowerGuard`] 在进程
//! 存活期间保持系统唤醒并关闭 EcoQoS 降频，退出时恢复；[`MmcssGuard`] 在
//! 当前线程注册 "Pro Audio" 任务，线程结束时 `AvRevertMmThreadCharacteristics`。
//!
//! 所有失败路径只告警：策略/权限受限时回退到原有行为（默认优先级/默认电源
//! 策略），不能因为这些优化让程序起不来。

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Power::{
    ES_CONTINUOUS, ES_SYSTEM_REQUIRED, EXECUTION_STATE, SetThreadExecutionState,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentProcess,
    GetCurrentThread, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    ProcessPowerThrottling, SetProcessInformation, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows::core::w;

use crate::logf;

/// 进程级电源守卫：保持系统唤醒 + 关闭 EcoQoS 执行速度节流。
///
/// 必须在主线程创建并持有到进程退出：`SetThreadExecutionState` 的
/// `ES_CONTINUOUS` 状态绑定在调用线程上，线程退出即失效。
pub struct ProcessPowerGuard {
    execution_state_set: bool,
    throttling_disabled: bool,
}

impl ProcessPowerGuard {
    pub fn acquire() -> Self {
        // 只要求系统保持唤醒，不加 ES_DISPLAY_REQUIRED：屏幕仍可关闭省电，
        // 但不会因空闲进入睡眠导致流中断。
        // SAFETY: 无指针参数；返回 0 表示调用失败。
        let execution_state_set =
            unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) }
                != EXECUTION_STATE(0);
        if !execution_state_set {
            logf!("[Power] SetThreadExecutionState 失败，系统仍可能休眠");
        }

        // StateMask = 0 表示解除 EXECUTION_SPEED 节流；Win10 1709+ 才支持，
        // 旧系统/组策略拒绝时保持系统默认行为。
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        // SAFETY: GetCurrentProcess 返回伪句柄（无需关闭），结构体指针与
        // 声明长度严格对应。
        let throttling_disabled = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
            .is_ok()
        };
        if !throttling_disabled {
            logf!("[Power] 关闭 EcoQoS 降频失败，后台运行时可能被降频");
        } else {
            logf!("[Power] 已保持系统唤醒并关闭 EcoQoS 降频");
        }

        Self {
            execution_state_set,
            throttling_disabled,
        }
    }
}

impl Drop for ProcessPowerGuard {
    fn drop(&mut self) {
        if self.execution_state_set {
            // SAFETY: 无指针参数；ES_CONTINUOUS 单独调用即恢复默认电源行为。
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS);
            }
        }
        if self.throttling_disabled {
            // StateMask 置位 = 重新允许系统按默认策略节流。
            let state = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
                StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            };
            // SAFETY: 同 acquire；失败无需处理，进程即将退出。
            unsafe {
                let _ = SetProcessInformation(
                    GetCurrentProcess(),
                    ProcessPowerThrottling,
                    &state as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
                );
            }
        }
    }
}

/// 当前线程的 MMCSS "Pro Audio" 注册。句柄在 Drop 时成对归还。
pub struct MmcssGuard {
    handle: Option<HANDLE>,
}

impl MmcssGuard {
    pub fn acquire(label: &str) -> Self {
        let mut task_index = 0u32;
        // SAFETY: 任务名由 w! 生成静态 NUL 结尾 UTF-16；task_index 可写。
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
            Ok(handle) => {
                logf!("[{label}] 已注册 MMCSS Pro Audio（task index {task_index}）");
                Self {
                    handle: Some(handle),
                }
            }
            Err(e) => {
                logf!("[{label}] MMCSS 注册失败（{e}），回退到 THREAD_PRIORITY_HIGHEST");
                Self { handle: None }
            }
        }
    }

    /// MMCSS 是否生效；false 时调用方应回退到普通线程优先级。
    pub fn is_active(&self) -> bool {
        self.handle.is_some()
    }
}

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: handle 只由本守卫持有，且每次 acquire 对应一次 revert。
            let _ = unsafe { AvRevertMmThreadCharacteristics(handle) };
        }
    }
}

/// 提升当前线程的音频调度优先级。
///
/// 优先注册 MMCSS "Pro Audio"；失败（音频服务被裁剪/组策略限制）时回退到
/// `THREAD_PRIORITY_HIGHEST`，再失败只告警、不改变控制流。
///
/// 返回的守卫必须在线程存活期间持有：Drop 时成对归还 MMCSS 句柄。
pub(crate) fn boost_current_thread(label: &str, role: &str) -> MmcssGuard {
    let mmcss = MmcssGuard::acquire(label);
    if !mmcss.is_active() {
        // SAFETY: GetCurrentThread 返回伪句柄，不转移所有权、无需关闭。
        unsafe {
            if let Err(e) = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) {
                logf!("[{label}] {role}提权失败（{e}），继续用默认优先级");
            }
        }
    }
    mmcss
}
