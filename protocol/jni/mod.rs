//! JNI 绑定：Android 端**直接复用**本 crate 的协议实现（单源化，v1.5.0）。
//!
//! ## 为什么存在
//!
//! v1.4.0 之前，`android/.../Udp2CalProtocol.kt` 是 `protocol/protocol.rs` +
//! `protocol/rtp.rs` 的手工移植副本（约 1000 行）。两份实现同步靠人眼比对，
//! 每次改协议都要改两处、跑两套测试——这正是 v1.4.0 里 `WATER_TARGET_MS`
//! 与阶跃吸收必须"成对修改"的根源。本模块把 Kotlin 从"第二份实现"降级为
//! **胶水**：常量与 DTO 留在 Kotlin（编译期需要），所有行为都由这里执行，
//! 与 Windows 端跑的是同一段 Rust 代码。
//!
//! ## 形态
//!
//! * `crate-type = ["rlib", "cdylib"]`：Android 得到 `libudp2cal_protocol.so`，
//!   host 得到 `udp2cal_protocol.dll`。**同一套符号**，所以现有 Kotlin 单测
//!   可以在 JVM 上原样跑，等于给 JNI 层本身做了一致性回归。
//! * 依赖只有 `jni-sys`（零传递依赖、纯函数表），不需要 `jni` crate 的封装层，
//!   也不引入 `GetJavaVM`——见下面的时钟桥。
//! * 句柄一律 `Box::into_raw` 成 `jlong`，Kotlin 侧显式 `close()`；
//!   `JitterBuffer` 会在接收线程（`insert`）与播放线程（`pull`）之间共享，
//!   故包一层 `Mutex`——纯 Kotlin 版本这里是数据竞争，锁上之后两端语义
//!   反而更严格。
//!
//! ## 时钟桥
//!
//! Kotlin 的 `System.nanoTime()` 与 Rust 的 `Instant` 在同一平台上来自同一个
//! 单调时钟（Android = `CLOCK_MONOTONIC`，Windows = QPC）。Kotlin 在加载本库
//! 后立刻调用一次 [`Java_com_udp2cal_app_native_ProtocolNative_clockSync`] 建立
//! `(Instant, java_nanos)` 基准，之后
//!
//! * `arrivalNanosAt` 把内部记录的 `Instant` 换算回 Java 纳秒（播放线程算
//!   `System.nanoTime() - arrival` 就是"这帧有多旧"）；
//!
//! 两侧速率完全同源，基准偏差只有一次读时钟的几微秒，长期不漂移。
//!
//! ## unsafe 契约
//!
//! 本文件的 `unsafe` 全部来自 JNI 的 C ABI。下列论证适用于本文件的**每一个**
//! `unsafe fn`，逐条写在这里而不是在 70 多个入口点上重复：
//!
//! 1. **`Env` 与引用只在本次调用内有效**：JVM 保证 native 方法执行期间 `env`、
//!    `this` 与参数引用合法。本模块不把 `env`/`jobject`/`jbyteArray` 存进任何
//!    全局或结构体，因此不存在跨调用悬垂。
//! 2. **函数表非空**：`JNINativeInterface_` 的函数指针由 JVM 在创建/附着线程时
//!    填满，JNI 规范要求非空，故 `(f.GetXxx)(...)` 不会 panic（见 [`fns`]）。
//! 3. **句柄生命周期归 Kotlin 侧**：`Box::into_raw` 只出现在 `*Create`，
//!    `Box::from_raw` 只出现在对应的 `*Destroy` 且均先判 `h != 0`。重复 destroy
//!    或使用已释放句柄属于 Kotlin 侧契约违反（见 `docs/PROTOCOL_V3_RTP.md` §12），
//!    Rust 侧无法在不引入全局句柄注册表的前提下防御——那属于 L3 设计变更，未做。
//! 4. **跨边界数组搬运先校验边界**：所有 `Get/SetXxxArrayRegion` 都先取
//!    `GetArrayLength`，再用 `i64` 相加比较（不用 `jint` 相加），越界直接返回失败。

// 这里全是 JNI 入口点，安全边界由 JVM 侧保证（参数由 `external fun` 声明约束，
// 句柄由 `Box::into_raw`/`Box::from_raw` 成对管理），不需要 Rust API 文档里的
// `# Safety` 段；行文见模块头部的「形态」与「时钟桥」与「unsafe 契约」。
//
// crate 根是 `#![deny(unsafe_code)]`，本模块是全 crate 唯一的例外。
#![allow(unsafe_code)]
// 整个模块就是一层 JNI unsafe 契约（见上），逐点再包 unsafe 块只是噪声
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use jni_sys::{
    JNIEnv, JNINativeInterface__1_1, jboolean, jbyte, jbyteArray, jchar, jdouble, jdoubleArray,
    jint, jintArray, jlong, jlongArray, jobject, jshortArray, jsize, jstring,
};

use crate::resampler::{MAX_INPUT_FRAMES, MAX_OUTPUT_FRAMES, Resampler};

use crate::rtp::{
    ARRIVAL_TRACE, DeviceRateEstimator, DriftController, InsertResult, JitterBuffer,
    OPUS_FRAME_SAMPLES, OPUS_MAX_PAYLOAD, OPUS_SAMPLE_RATE, Pull, RTP_HEADER_SIZE, RTP_MAX_PACKET,
    RtpHeader, RtpPacket, StepAbsorber, extend_seq, extend_ts, water_target_ms,
};
use crate::{
    BITRATE_AUTO_KBPS, BITRATE_MAX_KBPS, BITRATE_STEPS_KBPS, CONNECT_ACK_PAYLOAD_SIZE,
    CONNECT_PAYLOAD_SIZE, CONTROL_HEADER_SIZE, CONTROL_MAX_PAYLOAD, CONTROL_VERSION,
    ConnectAckPayload, ConnectPayload, DEFAULT_LISTEN_PORT, DEVICE_ID_SIZE,
    DEVICE_NAME_MAX, DISCOVER_PORT, DiscoverReply, OPUS_FRAME_MS, OPUS_PAYLOAD_TYPE, TYPE_CONNECT,
    TYPE_CONNECT_ACK, TYPE_DATA, TYPE_DISCOVER_REPLY, TYPE_DISCOVER_REQ, TYPE_MAX_KNOWN,
    build_control, clamp_bitrate_kbps, parse_control, snap_bitrate_kbps,
};

/// 原生方法收到的 `JNIEnv`：与 `jni-sys` 函数表字段同型，直接透传。
type Env = *mut JNIEnv;
type Handle = jlong;

/// 本模块只用到 JNI 1.1 就有的函数，故固定取 1.1 版函数表。
#[inline]
unsafe fn fns(env: Env) -> &'static JNINativeInterface__1_1 {
    // SAFETY: `env` 由 JVM 在本次 native 调用期间保证有效且非空；它指向的
    // 函数表在 JVM 生命周期内不可变且一直存活，因此返回 `'static` 引用是
    // 成立的（实际只在本模块内按本次调用的作用域使用）。
    &(**env).v1_1
}

#[inline]
fn null<T>() -> *mut T {
    std::ptr::null_mut()
}

// ═══════════════════════ 数组/字符串搬运 ═══════════════════════

/// 把 `arr[offset .. offset+len)` 拷进 `out`；越界返回 false。
///
/// 边界校验全部用 `i64` 相加，避开 `offset + len` 的 `jint` 溢出。
unsafe fn read_bytes(
    env: Env,
    arr: jbyteArray,
    offset: jint,
    len: jint,
    out: &mut Vec<u8>,
) -> bool {
    if arr.is_null() || offset < 0 || len < 0 {
        return false;
    }
    let f = fns(env);
    let total = (f.GetArrayLength)(env, arr);
    if total < 0 || (offset as i64 + len as i64) > total as i64 {
        return false;
    }
    out.clear();
    out.resize(len as usize, 0);
    if len > 0 {
        (f.GetByteArrayRegion)(env, arr, offset, len, out.as_mut_ptr() as *mut jbyte);
    }
    true
}

/// 把 `arr[0 .. len)` 拷进调用方的定长缓冲；越界返回 false。
///
/// 与 [`read_bytes`] 的区别是不在原生堆上分配：RTP 解析只需要包长和
/// 12 字节头，调用方给一块栈上数组即可，每包省掉一次 `Vec` 分配。
unsafe fn read_bytes_head(env: Env, arr: jbyteArray, len: jint, out: &mut [u8]) -> bool {
    if arr.is_null() || len < 0 || len as usize > out.len() {
        return false;
    }
    let f = fns(env);
    let total = (f.GetArrayLength)(env, arr);
    if total < 0 || len as i64 > total as i64 {
        return false;
    }
    if len > 0 {
        (f.GetByteArrayRegion)(env, arr, 0, len, out.as_mut_ptr() as *mut jbyte);
    }
    true
}

/// 解析 8 字节设备 ID；长度不符返回 None。
unsafe fn read_device_id(env: Env, arr: jbyteArray) -> Option<[u8; DEVICE_ID_SIZE]> {
    let mut buf = Vec::new();
    if !read_bytes(env, arr, 0, DEVICE_ID_SIZE as jint, &mut buf) {
        return None;
    }
    let mut id = [0u8; DEVICE_ID_SIZE];
    id.copy_from_slice(&buf);
    Some(id)
}

/// 确保复用缓冲恰好 `OPUS_MAX_PAYLOAD` 字节、且不超额占用容量。
///
/// `read_bytes` 只取需要的长度（精确缩容），而 `JitterBuffer::pull` 要求
/// 输出切片容得下队首帧；VBR 下包长在 40–100 字节之间跳，缩过头的缓冲会让
/// 一个本来可播的帧被当成 `Wait`（时间轴不前进、播放线程空转）。
///
/// 反过来，长度与容量也要有上界：`read_bytes` 按调用方传入的 `length` 分配，
/// 而真实包最大只有 [`OPUS_MAX_PAYLOAD`]（超长包会被 `JitterBuffer::insert`
/// 丢弃）。一次超大 `jbInsert` 会让这个句柄永久占着那块内存——它不释放，也
/// 不再被任何合法包用到，属于没有收益的常驻分配。
fn ensure_payload_capacity(buf: &mut Vec<u8>) {
    if buf.len() != OPUS_MAX_PAYLOAD {
        buf.resize(OPUS_MAX_PAYLOAD, 0);
    }
    if buf.capacity() > OPUS_MAX_PAYLOAD {
        buf.shrink_to(OPUS_MAX_PAYLOAD);
    }
}

/// 把 `data` 写进 `arr[offset ..]`；容量不足返回 false。
unsafe fn write_bytes(env: Env, arr: jbyteArray, offset: jint, data: &[u8]) -> bool {
    if arr.is_null() || offset < 0 {
        return false;
    }
    let f = fns(env);
    let total = (f.GetArrayLength)(env, arr);
    if total < 0 || (offset as i64 + data.len() as i64) > total as i64 {
        return false;
    }
    if !data.is_empty() {
        (f.SetByteArrayRegion)(
            env,
            arr,
            offset,
            data.len() as jsize,
            data.as_ptr() as *const jbyte,
        );
    }
    true
}

unsafe fn new_bytes(env: Env, data: &[u8]) -> jbyteArray {
    let f = fns(env);
    let arr = (f.NewByteArray)(env, data.len() as jsize);
    if arr.is_null() {
        return null();
    }
    if !data.is_empty() {
        (f.SetByteArrayRegion)(
            env,
            arr,
            0,
            data.len() as jsize,
            data.as_ptr() as *const jbyte,
        );
    }
    arr
}

/// 把 `arr[offset .. offset+len)` 拷进 `out`；越界返回 false。
/// 与 [`read_bytes`] 同构，供重采样器搬运 `short` PCM。
unsafe fn read_shorts(
    env: Env,
    arr: jshortArray,
    offset: jint,
    len: jint,
    out: &mut Vec<i16>,
) -> bool {
    if arr.is_null() || offset < 0 || len < 0 {
        return false;
    }
    let f = fns(env);
    let total = (f.GetArrayLength)(env, arr);
    if total < 0 || (offset as i64 + len as i64) > total as i64 {
        return false;
    }
    out.clear();
    out.resize(len as usize, 0);
    if len > 0 {
        (f.GetShortArrayRegion)(env, arr, offset, len, out.as_mut_ptr());
    }
    true
}

/// 把 `data` 写进 `arr[offset ..]`；容量不足返回 false。
unsafe fn write_shorts(env: Env, arr: jshortArray, offset: jint, data: &[i16]) -> bool {
    if arr.is_null() || offset < 0 {
        return false;
    }
    let f = fns(env);
    let total = (f.GetArrayLength)(env, arr);
    if total < 0 || (offset as i64 + data.len() as i64) > total as i64 {
        return false;
    }
    if !data.is_empty() {
        (f.SetShortArrayRegion)(env, arr, offset, data.len() as jsize, data.as_ptr());
    }
    true
}

unsafe fn new_longs(env: Env, data: &[jlong]) -> jlongArray {
    let f = fns(env);
    let arr = (f.NewLongArray)(env, data.len() as jsize);
    if arr.is_null() {
        return null();
    }
    (f.SetLongArrayRegion)(env, arr, 0, data.len() as jsize, data.as_ptr());
    arr
}

/// 把 `data` 写进调用方数组 `arr[0 .. data.len())`；容量不足返回 false。
unsafe fn write_longs(env: Env, arr: jlongArray, data: &[jlong]) -> bool {
    if arr.is_null() {
        return false;
    }
    let f = fns(env);
    let total = (f.GetArrayLength)(env, arr);
    if total < 0 || data.len() as jsize > total {
        return false;
    }
    if !data.is_empty() {
        (f.SetLongArrayRegion)(env, arr, 0, data.len() as jsize, data.as_ptr());
    }
    true
}

unsafe fn new_ints(env: Env, data: &[jint]) -> jintArray {
    let f = fns(env);
    let arr = (f.NewIntArray)(env, data.len() as jsize);
    if arr.is_null() {
        return null();
    }
    (f.SetIntArrayRegion)(env, arr, 0, data.len() as jsize, data.as_ptr());
    arr
}

unsafe fn new_doubles(env: Env, data: &[jdouble]) -> jdoubleArray {
    let f = fns(env);
    let arr = (f.NewDoubleArray)(env, data.len() as jsize);
    if arr.is_null() {
        return null();
    }
    (f.SetDoubleArrayRegion)(env, arr, 0, data.len() as jsize, data.as_ptr());
    arr
}

/// 返回 `null` 当 `s` 含无法编码的内容（现代 JNI 用 UTF-16，不会再出现 MUTF-8 问题）。
unsafe fn new_string(env: Env, s: &str) -> jstring {
    let units: Vec<u16> = s.encode_utf16().collect();
    (fns(env).NewString)(env, units.as_ptr() as *const jchar, units.len() as jsize)
}

// ═══════════════════════ 时钟桥 ═══════════════════════

/// `(Rust Instant 基准, 同一时刻的 Java System.nanoTime())`。
///
/// 用 `Mutex<Option<_>>` 而非常量化的 `OnceLock`：`clockSync` 允许重复调用
/// （重载库、单元测试里换 JVM），每次都覆盖基准；未同步时读侧拿到 `None`，
/// `arrivalNanosAt` 返回 `-1`（上层 `if (at > 0)` 自然跳过），而不是把
/// “加载后经过的纳秒”当成 Java 纳秒去污染速率估计。
static CLOCK_BASE: Mutex<Option<(Instant, i64)>> = Mutex::new(None);

fn clock_base() -> Option<(Instant, i64)> {
    *CLOCK_BASE.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_clock_base(value: (Instant, i64)) {
    *CLOCK_BASE.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
}

fn instant_from_nanos(nanos: i64) -> Instant {
    let (base_instant, base_nanos) = match clock_base() {
        Some(base) => base,
        // 未同步：只能当作“刚刚”（速率估计会自然重置，不会得到离谱值）
        None => return Instant::now(),
    };
    let delta = nanos.saturating_sub(base_nanos);
    if delta >= 0 {
        base_instant
            .checked_add(Duration::from_nanos(delta as u64))
            .unwrap_or(base_instant)
    } else {
        base_instant
            .checked_sub(Duration::from_nanos(delta.unsigned_abs()))
            .unwrap_or(base_instant)
    }
}

/// 换算回 Java 纳秒；未同步时返回 `-1`（“查不到”）。
fn nanos_from_instant(at: Instant) -> i64 {
    let (base_instant, base_nanos) = match clock_base() {
        Some(base) => base,
        None => return -1,
    };
    let clamp = |d: Duration| d.as_nanos().min(i64::MAX as u128) as i64;
    match at.checked_duration_since(base_instant) {
        Some(d) => base_nanos.saturating_add(clamp(d)),
        None => base_nanos.saturating_sub(clamp(base_instant.duration_since(at))),
    }
}

// ═══════════════════════ 句柄基础设施 ═══════════════════════

#[inline]
unsafe fn lock<'a, T>(cell: &'a Mutex<T>) -> MutexGuard<'a, T> {
    // SAFETY: `cell` 是句柄内部的 `Mutex`，调用方已确保句柄有效（见模块头部
    // unsafe 契约第 3 条）；`MutexGuard` 的生命周期被约束为 `'a`，不会超出引用。
    // 中毒恢复：JNI 侧不允许把 panic 传染成永久不可用
    cell.lock().unwrap_or_else(|e| e.into_inner())
}

// 句柄解引用（`jb_ref`/`step_ref`/`drift_ref`/`device_rate_ref`）：
// 要求 `h` 来自对应的 `*Create` 且尚未被 `*Destroy` 释放。签名为方便标成
// `&'static`，实际有效期不超过本次 JNI 调用——调用点都只在本函数体内用完。

/// 为「句柄 → 内部状态」型标量 getter 生成 JNI 入口。
///
/// * `$handle_ref`：句柄解引用函数（[`jb_ref`] / [`step_ref`] / ...）；
/// * `$default`：句柄无效（0 或已释放）时的返回值；
/// * `$body`：接收 `&mut 内部状态` 的闭包。只读 getter 也统一拿 `&mut`，
///   这样 `take_limited_log` 之类有状态 getter 不必再手写一份入口。
macro_rules! handle_scalar {
    ($name:ident, $handle_ref:path, $ret:ty, $default:expr, $body:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn $name(_env: Env, _this: jobject, h: Handle) -> $ret {
            match $handle_ref(h) {
                Some(handle) => {
                    let mut g = lock(&handle.inner);
                    #[allow(clippy::redundant_closure_call)]
                    ($body)(&mut g)
                }
                None => $default,
            }
        }
    };
}

mod control;
mod jitter;
mod resampler;
mod rtp;
mod timing;
