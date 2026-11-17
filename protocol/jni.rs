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

/// 建立 Java `System.nanoTime()` ↔ Rust `Instant` 基准。
///
/// Kotlin 侧在库加载完成后**立刻**调用一次（[`ProtocolNative`] 的 `init`）。
/// 重复调用是允许的：每次覆盖基准。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_clockSync(
    _env: Env,
    _this: jobject,
    java_nanos: jlong,
) {
    set_clock_base((Instant::now(), java_nanos));
}

// ═══════════════════════ 常量导出（防两端漂移） ═══════════════════════

/// 全部整数常量，顺序见 `Udp2CalProtocolTest.nativeConstantsMatchKotlinConstants`。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_constants(
    env: Env,
    _this: jobject,
) -> jlongArray {
    let v: [jlong; 52] = [
        CONTROL_VERSION as jlong,
        CONTROL_HEADER_SIZE as jlong,
        CONTROL_MAX_PAYLOAD as jlong,
        DEVICE_ID_SIZE as jlong,
        DISCOVER_PORT as jlong,
        DEFAULT_LISTEN_PORT as jlong,
        TYPE_DATA as jlong,
        TYPE_CONNECT as jlong,
        TYPE_DISCOVER_REQ as jlong,
        TYPE_DISCOVER_REPLY as jlong,
        TYPE_CONNECT_ACK as jlong,
        TYPE_MAX_KNOWN as jlong,
        CONNECT_PAYLOAD_SIZE as jlong,
        CONNECT_ACK_PAYLOAD_SIZE as jlong,
        DEVICE_NAME_MAX as jlong,
        RTP_HEADER_SIZE as jlong,
        OPUS_PAYLOAD_TYPE as jlong,
        OPUS_SAMPLE_RATE as jlong,
        OPUS_FRAME_MS as jlong,
        OPUS_FRAME_SAMPLES as jlong,
        OPUS_MAX_PAYLOAD as jlong,
        crate::rtp::RTP_MAX_PACKET as jlong,
        crate::rtp::UDP_IP_MAX_PACKET as jlong,
        crate::BITRATE_AUTO_KBPS as jlong,
        crate::BITRATE_MAX_KBPS as jlong,
        BITRATE_STEPS_KBPS[0] as jlong,
        BITRATE_STEPS_KBPS[1] as jlong,
        BITRATE_STEPS_KBPS[2] as jlong,
        BITRATE_STEPS_KBPS[3] as jlong,
        BITRATE_STEPS_KBPS[4] as jlong,
        crate::rtp::MAX_REORDER_PACKETS as jlong,
        ARRIVAL_TRACE as jlong,
        crate::rtp::JITTER_START_MS as jlong,
        water_target_ms() as jlong,
        crate::rtp::WATER_TARGET_MIN_MS as jlong,
        crate::rtp::WATER_TARGET_MAX_MS as jlong,
        crate::rtp::DECODE_LIMIT_MS as jlong,
        crate::rtp::PLC_LEAD_MS as jlong,
        crate::rtp::PLC_MAX_MS as jlong,
        crate::rtp::PLC_HOLE_MAX_MS as jlong,
        crate::rtp::PLC_HOLD_MS as jlong,
        crate::rtp::RESYNC_GAP_MS as jlong,
        crate::rtp::STEP_TRIM_GROWTH_MS as jlong,
        crate::rtp::STEP_TRIM_EXCESS_MS as jlong,
        crate::rtp::STEP_TRIM_HOLD_MS as jlong,
        crate::rtp::STEP_TRIM_MAX_PER_MIN as jlong,
        // CONNECT 线上语义：Kotlin 复刻一份，必须逐项比对，防两端漂移。
        crate::OPUS_FLAG_VBR as jlong,
        crate::OPUS_FLAG_DTX as jlong,
        crate::OPUS_FLAG_VBR_CONSTRAINT as jlong,
        crate::OPUS_FLAG_FEC as jlong,
        crate::OPUS_FLAGS_RESERVED_MASK as jlong,
        crate::OPUS_COMPLEXITY_MAX as jlong,
    ];
    new_longs(env, &v)
}

/// 全部浮点常量。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_constantsReal(
    env: Env,
    _this: jobject,
) -> jdoubleArray {
    let v: [jdouble; 7] = [
        crate::rtp::RESAMPLE_MAX_ADJUST,
        crate::rtp::RATIO_SLEW_STEP,
        crate::rtp::PI_KP_PER_MS,
        crate::rtp::PI_TI_S,
        crate::rtp::PI_INTEGRAL_MAX,
        crate::rtp::PI_MIN_UPDATE_MS,
        crate::rtp::WATER_SMOOTH_TAU_MS,
    ];
    new_doubles(env, &v)
}

// ═══════════════════════ 控制面（纯函数） ═══════════════════════

/// `[msg_type, payload_len, id0..id7]`，解析失败返回 null。
///
/// 要求数据报长度精确等于 `15 + payload_len`（只校验头部的入口已删除）。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_parseControl(
    env: Env,
    _this: jobject,
    buf: jbyteArray,
    len: jint,
) -> jlongArray {
    let mut data = Vec::new();
    if !read_bytes(env, buf, 0, len, &mut data) {
        return null();
    }
    let hdr = match parse_control(&data) {
        Some((h, _)) => h,
        None => return null(),
    };
    let mut out: Vec<jlong> = Vec::with_capacity(2 + DEVICE_ID_SIZE);
    out.push(hdr.msg_type as jlong);
    out.push(hdr.payload_len as jlong);
    for b in hdr.device_id {
        out.push(b as jlong);
    }
    new_longs(env, &out)
}

/// `buildControl` 的入参合法性。返回 `false` 时导出函数必须返回 `null`。
///
/// 抽成独立函数是为了能在没有 `JNIEnv` 的情况下单测：这条防线的失效后果是
/// **进程 abort**（debug）或**静默截断的坏包**（release），不能只靠“读一遍
/// 代码看上去没问题”。
///
/// - `msg_type`：收窄成 `u8` 必须无损。Kotlin 会传 TYPE_DATA(0)/TYPE_CONNECT(1)/
///   TYPE_DISCOVER_REQ(2)，测试里还会传 9，所以只拦 `0..=255` 之外的值，
///   不做“已知类型”白名单。
/// - `payload_len`：`build_control` 对超过 [`CONTROL_MAX_PAYLOAD`] 的负载有
///   `debug_assert`，panic 从 `extern "system"` 逃出去 = abort；release 下则
///   静默截断成 512 B 并把“看着合法”的坏包交给 Kotlin（Kotlin 以为成功）。
fn build_control_args_ok(msg_type: jint, payload_len: jint) -> bool {
    (0..=u8::MAX as jint).contains(&msg_type)
        && (0..=CONTROL_MAX_PAYLOAD as jint).contains(&payload_len)
}

/// `jint → u8` 必须无损；越界返回 `None`（导出函数据此返回 `null`）。
fn u8_arg(v: jint) -> Option<u8> {
    (0..=u8::MAX as jint).contains(&v).then_some(v as u8)
}

/// `jint → u16` 必须无损；越界返回 `None`。
fn u16_arg(v: jint) -> Option<u16> {
    (0..=u16::MAX as jint).contains(&v).then_some(v as u16)
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_buildControl(
    env: Env,
    _this: jobject,
    msg_type: jint,
    device_id: jbyteArray,
    payload: jbyteArray,
) -> jbyteArray {
    let id = match read_device_id(env, device_id) {
        Some(id) => id,
        None => return null(),
    };
    // ART 对 null 数组调 GetArrayLength 会直接解引用空指针，必须先查
    if payload.is_null() {
        return null();
    }
    let payload_len = (fns(env).GetArrayLength)(env, payload);
    if !build_control_args_ok(msg_type, payload_len) {
        return null();
    }
    let mut body = Vec::new();
    if !read_bytes(env, payload, 0, payload_len, &mut body) {
        return null();
    }
    let out = build_control(msg_type as u8, &id, &body);
    new_bytes(env, &out)
}

/// `[reversePort, forwardSsrc, payloadType, frameMs, complexity, signal,
///   bandwidth, opusFlags, bitrateKbps, packetLoss]`
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_connectDecode(
    env: Env,
    _this: jobject,
    buf: jbyteArray,
    offset: jint,
    len: jint,
) -> jintArray {
    let mut data = Vec::new();
    if !read_bytes(env, buf, offset, len, &mut data) {
        return null();
    }
    let p = match ConnectPayload::decode(&data) {
        Some(p) => p,
        None => return null(),
    };
    let out: [jint; 10] = [
        p.reverse_port as jint,
        p.forward_ssrc as jint,
        p.payload_type as jint,
        p.frame_ms as jint,
        p.complexity as jint,
        p.signal as jint,
        p.bandwidth as jint,
        p.opus_flags as jint,
        p.bitrate_kbps as jint,
        p.packet_loss as jint,
    ];
    new_ints(env, &out)
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_connectEncode(
    env: Env,
    _this: jobject,
    reverse_port: jint,
    forward_ssrc: jint,
    payload_type: jint,
    frame_ms: jint,
    complexity: jint,
    signal: jint,
    bandwidth: jint,
    opus_flags: jint,
    bitrate_kbps: jint,
    packet_loss: jint,
) -> jbyteArray {
    let (
        Some(reverse_port),
        Some(payload_type),
        Some(frame_ms),
        Some(complexity),
        Some(signal),
        Some(bandwidth),
        Some(opus_flags),
        Some(bitrate_kbps),
        Some(packet_loss),
    ) = (
        u16_arg(reverse_port),
        u8_arg(payload_type),
        u8_arg(frame_ms),
        u8_arg(complexity),
        u8_arg(signal),
        u8_arg(bandwidth),
        u8_arg(opus_flags),
        u16_arg(bitrate_kbps),
        u8_arg(packet_loss),
    )
    else {
        return null();
    };
    let p = ConnectPayload {
        reverse_port,
        // jint → u32 是位模式保留的（Kotlin 用 Int 承载 SSRC），无需范围校验
        forward_ssrc: forward_ssrc as u32,
        payload_type,
        frame_ms,
        complexity,
        signal,
        bandwidth,
        opus_flags,
        bitrate_kbps,
        packet_loss,
    };
    new_bytes(env, &p.encode())
}

/// `[reverseSsrc, payloadType, frameMs, forwardSsrc]`
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_ackDecode(
    env: Env,
    _this: jobject,
    buf: jbyteArray,
    offset: jint,
    len: jint,
) -> jintArray {
    let mut data = Vec::new();
    if !read_bytes(env, buf, offset, len, &mut data) {
        return null();
    }
    let p = match ConnectAckPayload::decode(&data) {
        Some(p) => p,
        None => return null(),
    };
    let out: [jint; 4] = [
        p.reverse_ssrc as jint,
        p.payload_type as jint,
        p.frame_ms as jint,
        p.forward_ssrc as jint,
    ];
    new_ints(env, &out)
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_ackEncode(
    env: Env,
    _this: jobject,
    reverse_ssrc: jint,
    payload_type: jint,
    frame_ms: jint,
    forward_ssrc: jint,
) -> jbyteArray {
    let (Some(payload_type), Some(frame_ms)) = (u8_arg(payload_type), u8_arg(frame_ms)) else {
        return null();
    };
    let p = ConnectAckPayload {
        // jint → u32 是位模式保留的（Kotlin 用 Int 承载 SSRC），无需范围校验
        reverse_ssrc: reverse_ssrc as u32,
        payload_type,
        frame_ms,
        forward_ssrc: forward_ssrc as u32,
    };
    new_bytes(env, &p.encode())
}

/// DISCOVER_REPLY 的监听端口；非法返回 0。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_discoverPort(
    env: Env,
    _this: jobject,
    buf: jbyteArray,
    offset: jint,
    len: jint,
) -> jint {
    let mut data = Vec::new();
    if !read_bytes(env, buf, offset, len, &mut data) {
        return 0;
    }
    match DiscoverReply::decode(&data) {
        Some(r) => r.listen_port as jint,
        None => 0,
    }
}

/// DISCOVER_REPLY 的设备名；非法返回 null（空名已在 Rust 侧回落为 `"UDP2CAL PC"`）。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_discoverName(
    env: Env,
    _this: jobject,
    buf: jbyteArray,
    offset: jint,
    len: jint,
) -> jstring {
    let mut data = Vec::new();
    if !read_bytes(env, buf, offset, len, &mut data) {
        return null();
    }
    match DiscoverReply::decode(&data) {
        Some(r) => new_string(env, &r.device_name),
        None => null(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_clampBitrate(
    _env: Env,
    _this: jobject,
    kbps: jint,
) -> jint {
    if kbps <= 0 {
        return BITRATE_AUTO_KBPS as jint;
    }
    clamp_bitrate_kbps(kbps.min(BITRATE_MAX_KBPS as jint) as u16) as jint
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_snapBitrate(
    _env: Env,
    _this: jobject,
    kbps: jint,
) -> jint {
    if kbps <= 0 {
        return 0;
    }
    snap_bitrate_kbps(kbps.min(BITRATE_MAX_KBPS as jint) as u16) as jint
}

// ═══════════════════════ RTP ═══════════════════════

/// `[marker, seq, timestamp, ssrc, payloadOffset, payloadLength]`
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_parseRtp(
    env: Env,
    _this: jobject,
    buf: jbyteArray,
    len: jint,
) -> jlongArray {
    let mut data = Vec::new();
    if !read_bytes(env, buf, 0, len, &mut data) {
        return null();
    }
    let p = match crate::rtp::parse(&data) {
        Ok(p) => p,
        Err(_) => return null(),
    };
    let out: [jlong; 6] = [
        p.marker as jlong,
        p.seq as jlong,
        p.timestamp as jlong,
        p.ssrc as jlong,
        RTP_HEADER_SIZE as jlong,
        p.payload.len() as jlong,
    ];
    new_longs(env, &out)
}

/// 同 [`Java_com_udp2cal_app_native_ProtocolNative_parseRtp`]，但把字段写入
/// 调用方提供的数组（长度 ≥6），返回是否解析成功。
///
/// 反向接收线程每 10 ms 调一次。复用 Kotlin 侧 `LongArray` + 这里改用的
/// 栈缓冲后，热路径上不再有 `LongArray` / `RtpPacket` / `Vec` 分配。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_parseRtpInto(
    env: Env,
    _this: jobject,
    buf: jbyteArray,
    len: jint,
    out: jlongArray,
) -> jboolean {
    if len < 0 || len as usize > RTP_MAX_PACKET {
        return false;
    }
    let mut data = [0u8; RTP_MAX_PACKET];
    if !read_bytes_head(env, buf, len, &mut data) {
        return false;
    }
    let p = match crate::rtp::parse(&data[..len as usize]) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let fields: [jlong; 6] = [
        p.marker as jlong,
        p.seq as jlong,
        p.timestamp as jlong,
        p.ssrc as jlong,
        RTP_HEADER_SIZE as jlong,
        p.payload.len() as jlong,
    ];
    write_longs(env, out, &fields)
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_writeRtpHeader(
    env: Env,
    _this: jobject,
    dest: jbyteArray,
    offset: jint,
    marker: jboolean,
    seq: jint,
    timestamp: jint,
    ssrc: jint,
) -> jboolean {
    let hdr = RtpHeader::new(marker, seq as u16, timestamp as u32, ssrc as u32);
    let mut raw = [0u8; RTP_HEADER_SIZE];
    hdr.encode(&mut raw);
    // 目标数组太小/offset 非法时返回 false，上层可记录；不再静默发旧头
    write_bytes(env, dest, offset, &raw)
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_extendSeq(
    _env: Env,
    _this: jobject,
    highest: jlong,
    seq: jint,
) -> jlong {
    extend_seq(highest as u64, seq as u16) as jlong
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_extendTs(
    _env: Env,
    _this: jobject,
    highest: jlong,
    ts: jlong,
) -> jlong {
    extend_ts(highest as u64, ts as u32) as jlong
}

// ═══════════════════════ 句柄基础设施 ═══════════════════════

struct JbInner {
    jb: JitterBuffer,
    /// 复用的载荷暂存区：`insert` 收包、`pull` 取帧都借它，避免每帧分配
    scratch: Vec<u8>,
    /// 上一次 `pull` 写出的字节数（对应 Kotlin 的 `pulledLength`）
    pulled_len: jint,
}

struct JbHandle {
    inner: Mutex<JbInner>,
}

struct StepHandle {
    inner: Mutex<StepAbsorber>,
}

struct DriftHandle {
    inner: Mutex<DriftController>,
}

struct DeviceRateHandle {
    inner: Mutex<DeviceRateEstimator>,
}

struct RsHandle {
    inner: Mutex<RsInner>,
}

struct RsInner {
    rs: Resampler,
    /// 上送 PCM 的暂存：`short → f32` 转换前后各借一次，避免每帧分配
    in_i16: Vec<i16>,
    in_f32: Vec<f32>,
    /// 重采样输出（f32 交错）与回写 Java 数组前的 `short` 转换缓冲
    out_f32: Vec<f32>,
    out_i16: Vec<i16>,
}

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

unsafe fn jb_ref(h: Handle) -> Option<&'static JbHandle> {
    if h == 0 {
        return None;
    }
    (h as *const JbHandle).as_ref()
}

unsafe fn step_ref(h: Handle) -> Option<&'static StepHandle> {
    if h == 0 {
        return None;
    }
    (h as *const StepHandle).as_ref()
}

unsafe fn drift_ref(h: Handle) -> Option<&'static DriftHandle> {
    if h == 0 {
        return None;
    }
    (h as *const DriftHandle).as_ref()
}

unsafe fn device_rate_ref(h: Handle) -> Option<&'static DeviceRateHandle> {
    if h == 0 {
        return None;
    }
    (h as *const DeviceRateHandle).as_ref()
}

unsafe fn rs_ref(h: Handle) -> Option<&'static RsHandle> {
    if h == 0 {
        return None;
    }
    (h as *const RsHandle).as_ref()
}

// ═══════════════════════ JitterBuffer ═══════════════════════

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbCreate(
    _env: Env,
    _this: jobject,
) -> Handle {
    let handle = Box::new(JbHandle {
        inner: Mutex::new(JbInner {
            jb: JitterBuffer::new(),
            scratch: vec![0u8; OPUS_MAX_PAYLOAD],
            pulled_len: 0,
        }),
    });
    // SAFETY: `Box::into_raw` 把所有权交给 Kotlin 侧（见模块头部 unsafe
    // 契约第 3 条）；此后 Rust 侧不再持有该指针。
    Box::into_raw(handle) as Handle
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbDestroy(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if h != 0 {
        // SAFETY: `h` 必须来自 `jbCreate` 且未被释放过（Kotlin 侧 `close()` 保证
        // 只调用一次）；`jlong` → `*mut JbHandle` 还原的是同一个分配。
        drop(Box::from_raw(h as *mut JbHandle));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbReset(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if let Some(handle) = jb_ref(h) {
        let mut g = lock(&handle.inner);
        g.jb.reset();
        g.pulled_len = 0;
    }
}

/// 显式启动预缓冲：攒到本方向目标水位才起播（不再 30 ms 起播后慢慢爬）。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbSetStartPrebufferMs(
    _env: Env,
    _this: jobject,
    h: Handle,
    ms: jint,
) {
    if let Some(handle) = jb_ref(h) {
        lock(&handle.inner)
            .jb
            .set_start_prebuffer_ms(ms.max(0) as u32);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbInsert(
    env: Env,
    _this: jobject,
    h: Handle,
    seq: jint,
    ts: jlong,
    marker: jboolean,
    buf: jbyteArray,
    offset: jint,
    length: jint,
) -> jboolean {
    let handle = match jb_ref(h) {
        Some(x) => x,
        None => return false,
    };
    let mut g = lock(&handle.inner);
    // 暂存区借出：同一结构字段的不可变/可变借用无法通过 `g.` 路径共存
    let mut scratch = std::mem::take(&mut g.scratch);
    let mut ok = false;
    if read_bytes(env, buf, offset, length, &mut scratch) {
        let len = scratch.len();
        let pkt = RtpPacket {
            marker,
            payload_type: OPUS_PAYLOAD_TYPE,
            seq: seq as u16,
            timestamp: ts as u32,
            ssrc: 0,
            payload: &scratch[..len],
        };
        ok = !matches!(g.jb.insert(&pkt), InsertResult::Dropped);
    }
    // read_bytes 会精确缩容到本次载荷长度；放回去前恢复成整帧容量，
    // 否则下一次 pull 会因输出切片不够长而把可播帧当成 Wait（VBR 下必现）
    ensure_payload_capacity(&mut scratch);
    g.scratch = scratch;
    ok
}

/// 返回 `Pull` 的判别值（0=WAIT、1=PACKET、2=PLC、3=SILENCE）。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbPull(
    env: Env,
    _this: jobject,
    h: Handle,
    ring_ms: jint,
    pipe_ms: jint,
    out: jbyteArray,
) -> jint {
    let handle = match jb_ref(h) {
        Some(x) => x,
        None => return 0,
    };
    let mut g = lock(&handle.inner);
    g.pulled_len = 0;
    let mut scratch = std::mem::take(&mut g.scratch);
    ensure_payload_capacity(&mut scratch);
    let result = g.jb.pull(
        ring_ms.max(0) as u32,
        pipe_ms.max(0) as u32,
        &mut scratch[..],
    );
    let (code, len) = match result {
        Pull::Packet { len, .. } => (1, len),
        Pull::Plc { .. } => (2, 0),
        Pull::Silence { .. } => (3, 0),
        Pull::Wait => (0, 0),
    };
    if code != 0 {
        g.pulled_len = len as jint;
        if len > 0 && !write_bytes(env, out, 0, &scratch[..len]) {
            // 帧已经从缓冲里取走了（游标已前进）：不能对外报 Wait（那会让上层
            // 以为没东西可播，而时间轴其实已经跳了一帧）。降级成 PLC，
            // 时间轴与声音都不缺，代价只是这一帧的载荷丢了。
            g.pulled_len = 0;
            g.scratch = scratch;
            return 2;
        }
    }
    g.scratch = scratch;
    code
}

/// 前瞻播放位上的包而**不消费**它（供 Opus 带内 FEC 重建丢帧使用）。
///
/// 返回负载长度；`0` 表示播放位的包还没到（或句柄无效），调用方退回 PLC。
/// 与 `jbPull` 的 `WAIT`(0) 同值但语义不同：本函数**不推进**任何游标，
/// 也不会改变 `pulledLen`（它描述的是上一次 `jbPull` 的结果）。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbPeek(
    env: Env,
    _this: jobject,
    h: Handle,
    out: jbyteArray,
) -> jint {
    let handle = match jb_ref(h) {
        Some(x) => x,
        None => return 0,
    };
    let mut g = lock(&handle.inner);
    let mut scratch = std::mem::take(&mut g.scratch);
    ensure_payload_capacity(&mut scratch);
    let len = match g.jb.peek_current(&mut scratch[..]) {
        Some(n) => n,
        None => {
            g.scratch = scratch;
            return 0;
        }
    };
    let ok = write_bytes(env, out, 0, &scratch[..len]);
    g.scratch = scratch;
    if ok { len as jint } else { 0 }
}

/// 阶跃裁剪：按播放顺序取一帧，调用方必须**解码后丢弃 PCM**，
/// 让有状态 Opus 解码器跟住发送端。返回判别值同 `jbPull`。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbPullTrim(
    env: Env,
    _this: jobject,
    h: Handle,
    out: jbyteArray,
) -> jint {
    let handle = match jb_ref(h) {
        Some(x) => x,
        None => return 0,
    };
    let mut g = lock(&handle.inner);
    g.pulled_len = 0;
    let mut scratch = std::mem::take(&mut g.scratch);
    ensure_payload_capacity(&mut scratch);
    let result = g.jb.pull_trim(&mut scratch[..]);
    let (code, len) = match result {
        Pull::Packet { len, .. } => (1, len),
        Pull::Plc { .. } => (2, 0),
        Pull::Silence { .. } => (3, 0),
        Pull::Wait => (0, 0),
    };
    if code != 0 {
        g.pulled_len = len as jint;
        if len > 0 && !write_bytes(env, out, 0, &scratch[..len]) {
            g.pulled_len = 0;
            g.scratch = scratch;
            return 2;
        }
    }
    g.scratch = scratch;
    code
}

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

/// `JitterBuffer` 句柄的标量 getter（句柄无效时返回 `Default::default()`）。
macro_rules! jb_scalar {
    ($name:ident, $ret:ty, $body:expr) => {
        handle_scalar!($name, jb_ref, $ret, Default::default(), $body);
    };
}

jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbNextSeq,
    jlong,
    |g: &JbInner| g.jb.next_seq() as jlong
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbNextTs,
    jlong,
    |g: &JbInner| g.jb.next_ts() as jlong
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbHighestTs,
    jlong,
    |g: &JbInner| g.jb.highest_ts() as jlong
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbPlaying,
    jboolean,
    |g: &JbInner| g.jb.is_playing()
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbPlcRun,
    jint,
    |g: &JbInner| g.jb.plc_run() as jint
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbEpoch,
    jlong,
    |g: &JbInner| g.jb.epoch as jlong
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbPulledLength,
    jint,
    |g: &JbInner| g.pulled_len
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbWaterMs,
    jint,
    |g: &JbInner| g.jb.water_ms() as jint
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbOccupancyMs,
    jint,
    |g: &JbInner| g.jb.occupancy_ms() as jint
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbLate,
    jlong,
    |g: &JbInner| g.jb.stats.late as jlong
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbDuplicate,
    jlong,
    |g: &JbInner| g.jb.stats.duplicate as jlong
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbResyncs,
    jlong,
    |g: &JbInner| g.jb.stats.resyncs as jlong
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbSeqRealign,
    jlong,
    |g: &JbInner| g.jb.stats.seq_realign as jlong
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbOverflow,
    jlong,
    |g: &JbInner| g.jb.stats.overflow as jlong
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbPlcFrames,
    jlong,
    |g: &JbInner| g.jb.stats.plc_frames as jlong
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbMaxPlcRun,
    jint,
    |g: &JbInner| g.jb.stats.max_plc_run as jint
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbHoldFrames,
    jlong,
    |g: &JbInner| g.jb.stats.hold_frames as jlong
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbStepTrims,
    jlong,
    |g: &JbInner| g.jb.stats.step_trims as jlong
);

/// 到达时刻换算成 Java `System.nanoTime()` 口径；查不到返回 -1。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbArrivalNanosAt(
    _env: Env,
    _this: jobject,
    h: Handle,
    ts: jlong,
) -> jlong {
    let handle = match jb_ref(h) {
        Some(x) => x,
        None => return -1,
    };
    let g = lock(&handle.inner);
    // 时间戳由 32 位媒体时钟扩展而来，负数只可能是调用方笔误
    match g.jb.arrival_of(ts.max(0) as u64) {
        Some(at) => nanos_from_instant(at),
        None => -1,
    }
}

/// 最近一次重同步原因；从未重同步返回空串。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbLastRebaseReason(
    env: Env,
    _this: jobject,
    h: Handle,
) -> jstring {
    let handle = match jb_ref(h) {
        Some(x) => x,
        None => return null(),
    };
    let g = lock(&handle.inner);
    new_string(env, &g.jb.last_rebase_reason)
}

// ═══════════════════════ StepAbsorber ═══════════════════════

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_saCreate(
    _env: Env,
    _this: jobject,
) -> Handle {
    Box::into_raw(Box::new(StepHandle {
        inner: Mutex::new(StepAbsorber::new()),
    })) as Handle
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_saDestroy(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if h != 0 {
        drop(Box::from_raw(h as *mut StepHandle));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_saReset(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if let Some(handle) = step_ref(h) {
        lock(&handle.inner).reset();
    }
}

/// 返回值打包：`frames << 32 | limited`。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_saDecide(
    _env: Env,
    _this: jobject,
    h: Handle,
    jb_water_ms: jint,
    pipe_ms: jint,
    can_trim: jboolean,
    target_ms: jint,
) -> jlong {
    let handle = match step_ref(h) {
        Some(x) => x,
        None => return 0,
    };
    let mut g = lock(&handle.inner);
    let d = g.decide(
        jb_water_ms.max(0) as u32,
        pipe_ms.max(0) as u32,
        can_trim,
        target_ms.max(0) as u32,
    );
    // 高 32 位帧数、低 1 位限额标志。隐式契约：`frames <= PLC_MAX_MS / FRAME_MS`（=12），
    // 若将来把上限放大到 2^31 以上，这里必须改成两个导出。
    ((d.frames as jlong) << 32) | (d.limited as jlong)
}

/// 与 [`..._saDecide`] 相同，但显式传入 `System.nanoTime()` 基准时刻，
/// 供单测推进虚拟时间（生产路径仍用实时时钟）。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_saDecideAt(
    _env: Env,
    _this: jobject,
    h: Handle,
    jb_water_ms: jint,
    pipe_ms: jint,
    can_trim: jboolean,
    target_ms: jint,
    now_nanos: jlong,
) -> jlong {
    let handle = match step_ref(h) {
        Some(x) => x,
        None => return 0,
    };
    let mut g = lock(&handle.inner);
    let d = g.decide_at(
        jb_water_ms.max(0) as u32,
        pipe_ms.max(0) as u32,
        can_trim,
        target_ms.max(0) as u32,
        instant_from_nanos(now_nanos),
    );
    ((d.frames as jlong) << 32) | (d.limited as jlong)
}

handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_saHits,
    step_ref,
    jlong,
    0,
    |g: &mut StepAbsorber| g.hits as jlong
);

handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_saTrimsInWindow,
    step_ref,
    jint,
    0,
    |g: &mut StepAbsorber| g.trims_in_window() as jint
);

handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_saTakeLimitedLog,
    step_ref,
    jboolean,
    false,
    |g: &mut StepAbsorber| g.take_limited_log()
);

// ═══════════════════ DeviceRateEstimator（设备时钟前馈） ═══════════════════

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_deCreate(
    _env: Env,
    _this: jobject,
) -> Handle {
    Box::into_raw(Box::new(DeviceRateHandle {
        inner: Mutex::new(DeviceRateEstimator::new()),
    })) as Handle
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_deDestroy(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if h != 0 {
        drop(Box::from_raw(h as *mut DeviceRateHandle));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_deReset(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if let Some(handle) = device_rate_ref(h) {
        lock(&handle.inner).reset();
    }
}

/// `played_nanos` 用 Java `System.nanoTime()`，与 `Instant` 同源。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_deObserve(
    _env: Env,
    _this: jobject,
    h: Handle,
    device_frames: jlong,
    played_nanos: jlong,
) {
    if let Some(handle) = device_rate_ref(h) {
        lock(&handle.inner).observe(
            device_frames.max(0) as u64,
            instant_from_nanos(played_nanos),
        );
    }
}

// 无估计时返回 NaN。正值表示设备时钟偏慢。
handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_deFeedforward,
    device_rate_ref,
    jdouble,
    f64::NAN,
    |g: &mut DeviceRateEstimator| g.feedforward().unwrap_or(f64::NAN)
);

handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_deSigmaPpm,
    device_rate_ref,
    jdouble,
    0.0,
    |g: &mut DeviceRateEstimator| g.sigma_ppm()
);

handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_deBuckets,
    device_rate_ref,
    jint,
    0,
    |g: &mut DeviceRateEstimator| g.buckets() as jint
);

// ═══════════════════════ DriftController ═══════════════════════

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcCreate(
    _env: Env,
    _this: jobject,
    device_rate: jint,
    target_ms: jint,
) -> Handle {
    let rate = if device_rate > 0 {
        device_rate as u32
    } else {
        OPUS_SAMPLE_RATE
    };
    let target = if target_ms > 0 {
        target_ms as u32
    } else {
        water_target_ms()
    };
    Box::into_raw(Box::new(DriftHandle {
        inner: Mutex::new(DriftController::with_target(rate, target)),
    })) as Handle
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcDestroy(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if h != 0 {
        drop(Box::from_raw(h as *mut DriftHandle));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcReset(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if let Some(handle) = drift_ref(h) {
        lock(&handle.inner).reset();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcUpdate(
    _env: Env,
    _this: jobject,
    h: Handle,
    water_ms: jint,
    device_frames: jlong,
) -> jdouble {
    let handle = match drift_ref(h) {
        Some(x) => x,
        None => return 0.0,
    };
    lock(&handle.inner).update(water_ms.max(0) as u32, device_frames.max(0) as u64)
}

handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_dcAdjust,
    drift_ref,
    jdouble,
    0.0,
    |g: &mut DriftController| g.adjust()
);

handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_dcWater,
    drift_ref,
    jdouble,
    0.0,
    |g: &mut DriftController| g.water()
);

handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_dcIntegral,
    drift_ref,
    jdouble,
    0.0,
    |g: &mut DriftController| g.integral()
);

handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_dcTargetMs,
    drift_ref,
    jint,
    0,
    |g: &mut DriftController| g.target_ms() as jint
);

handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_dcRatioUpdates,
    drift_ref,
    jlong,
    0,
    |g: &mut DriftController| g.ratio_updates as jlong
);

// ═══════════════════════ 重采样器（三端唯一实现） ═══════════════════════

/// Kotlin `StreamResampler` 的 PCM 是 `short`，重采样核心统一用归一化 f32。
/// 除以 32768（而非 32767）与解码器输出满量程一致，回写时再乘回并饱和。
fn shorts_to_floats(src: &[i16], dst: &mut Vec<f32>) {
    dst.clear();
    dst.reserve(src.len());
    dst.extend(src.iter().map(|&s| s as f32 / 32768.0));
}

fn floats_to_shorts(src: &[f32], dst: &mut Vec<i16>) {
    dst.clear();
    dst.reserve(src.len());
    dst.extend(
        src.iter()
            .map(|&v| (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16),
    );
}

/// `channels` 必须是 1..=32；非法返回 0（Kotlin 侧构造时检查）。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_rsCreate(
    _env: Env,
    _this: jobject,
    channels: jint,
) -> Handle {
    if !(1..=32).contains(&channels) {
        return 0;
    }
    Box::into_raw(Box::new(RsHandle {
        inner: Mutex::new(RsInner {
            rs: Resampler::new(channels as usize),
            in_i16: Vec::new(),
            in_f32: Vec::new(),
            out_f32: Vec::new(),
            out_i16: Vec::new(),
        }),
    })) as Handle
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_rsDestroy(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if h != 0 {
        drop(Box::from_raw(h as *mut RsHandle));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_rsReset(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if let Some(handle) = rs_ref(h) {
        lock(&handle.inner).rs.reset();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_rsSetSourceRate(
    _env: Env,
    _this: jobject,
    h: Handle,
    source_rate: jint,
    device_rate: jint,
) {
    if let Some(handle) = rs_ref(h) {
        lock(&handle.inner)
            .rs
            .set_source_rate(source_rate.max(0) as u32, device_rate.max(0) as u32);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_rsSetAdjust(
    _env: Env,
    _this: jobject,
    h: Handle,
    adjust: jdouble,
) {
    if let Some(handle) = rs_ref(h) {
        lock(&handle.inner).rs.set_ratio_adjust(adjust);
    }
}

/// 单次 `rsProcess` 允许的最大输入帧数（Kotlin `StreamResampler.CAPACITY_FRAMES`）。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_rsMaxInputFrames(
    _env: Env,
    _this: jobject,
) -> jint {
    MAX_INPUT_FRAMES as jint
}

handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_rsRatio,
    rs_ref,
    jdouble,
    0.0,
    |g: &mut RsInner| g.rs.ratio()
);

// 输出数组一次必须能装下的 `short` 数（最坏比率 + 最负相位 + 满块）。
handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_rsMaxOutputShorts,
    rs_ref,
    jint,
    0,
    |g: &mut RsInner| (MAX_OUTPUT_FRAMES * g.rs.channels()) as jint
);

/// 重采样 `src[offset .. offset + frames*channels)`，结果写入 `out`。
///
/// 返回写入 `out` 的 `short` 数；句柄无效、参数越界或 `out` 装不下最坏情况
/// 时返回 `-1`（此时**不推进**任何状态，调用方必须按上界分配 `out`）。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_rsProcess(
    env: Env,
    _this: jobject,
    h: Handle,
    src: jshortArray,
    offset: jint,
    frames: jint,
    out: jshortArray,
) -> jint {
    let handle = match rs_ref(h) {
        Some(x) => x,
        None => return -1,
    };
    if frames < 0 || frames as usize > MAX_INPUT_FRAMES {
        return -1;
    }
    let mut g = lock(&handle.inner);
    let channels = g.rs.channels();
    let count = frames as usize * channels;
    // 先确认输出容量，再动状态：处理完才发现装不下就无法回滚相位与历史
    let needed = MAX_OUTPUT_FRAMES * channels;
    let f = fns(env);
    if out.is_null() || (f.GetArrayLength)(env, out) < needed as jint {
        return -1;
    }
    if !read_shorts(env, src, offset, count as jint, &mut g.in_i16) {
        return -1;
    }
    let inner = &mut *g;
    shorts_to_floats(&inner.in_i16, &mut inner.in_f32);
    inner.rs.process_into(&inner.in_f32, &mut inner.out_f32);
    floats_to_shorts(&inner.out_f32, &mut inner.out_i16);
    if !write_shorts(env, out, 0, &inner.out_i16) {
        return -1;
    }
    inner.out_i16.len() as jint
}

/// 环境变量给出的目标水位（Android 无环境变量，恒为 [`WATER_TARGET_MS`]）。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_waterTargetMs(
    _env: Env,
    _this: jobject,
) -> jint {
    water_target_ms() as jint
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `buildControl` 的入参守卫：非法值必须在进 `build_control` 之前被拦下。
    /// 漏拦的后果是 debug 构建 panic 逃出 `extern "system"` = 进程 abort。
    #[test]
    fn build_control_args_reject_out_of_range() {
        // 合法：Kotlin 实际会传的三种类型（含 TYPE_DATA = 0）与测试里的 9
        assert!(build_control_args_ok(TYPE_DATA as jint, 0));
        assert!(build_control_args_ok(
            TYPE_CONNECT as jint,
            CONNECT_PAYLOAD_SIZE as jint
        ));
        assert!(build_control_args_ok(TYPE_DISCOVER_REQ as jint, 0));
        assert!(build_control_args_ok(9, 0));
        // 边界：u8::MAX 类型、恰好 512 字节负载都放行
        assert!(build_control_args_ok(
            u8::MAX as jint,
            CONTROL_MAX_PAYLOAD as jint
        ));

        // msg_type 越过 u8（收窄会静默截断）
        assert!(!build_control_args_ok(256, 0));
        assert!(!build_control_args_ok(0x1_0000, 0));
        assert!(!build_control_args_ok(-1, 0));
        assert!(!build_control_args_ok(jint::MIN, 0));

        // 负载超上限（debug_assert panic / release 静默截断）
        assert!(!build_control_args_ok(TYPE_CONNECT as jint, 513));
        assert!(!build_control_args_ok(TYPE_CONNECT as jint, jint::MAX));
        // 负长度（GetArrayLength 正常不会给，但 JNI 边界不当假设）
        assert!(!build_control_args_ok(TYPE_CONNECT as jint, -1));
    }

    /// 越界字段必须在 `new_bytes` 之前返回 null，而不是静默截断（356 → 100）。
    /// 只走拒绝路径，故可用空 `Env`：这些分支不碰 JNI 函数表。
    #[test]
    fn connect_and_ack_encode_reject_out_of_range_fields() {
        let env: Env = std::ptr::null_mut();
        let this: jobject = std::ptr::null_mut();
        let connect = |payload_type: jint, frame_ms: jint, packet_loss: jint| unsafe {
            Java_com_udp2cal_app_native_ProtocolNative_connectEncode(
                env,
                this,
                0,
                1,
                payload_type,
                frame_ms,
                0,
                0,
                4,
                crate::OPUS_FLAG_VBR as jint,
                0,
                packet_loss,
            )
        };
        assert!(connect(OPUS_PAYLOAD_TYPE as jint, OPUS_FRAME_MS as jint, 356).is_null());
        assert!(connect(256, OPUS_FRAME_MS as jint, 0).is_null());
        assert!(connect(OPUS_PAYLOAD_TYPE as jint, 300, 0).is_null());

        let ack = |frame_ms: jint| unsafe {
            Java_com_udp2cal_app_native_ProtocolNative_ackEncode(
                env,
                this,
                0,
                OPUS_PAYLOAD_TYPE as jint,
                frame_ms,
                0,
            )
        };
        assert!(ack(300).is_null());
    }

    /// 复用缓冲的尺寸必须与 `OPUS_MAX_PAYLOAD` 恰好一致：偏小会把可播帧当
    /// `Wait`，偏大会让一次超大 insert 永久抬高该句柄的常驻内存。
    #[test]
    fn payload_scratch_is_normalized_both_ways() {
        // 偏小（VBR 短包之后）→ 撑到整帧容量，保证 pull 的切片够长
        let mut small = vec![0u8; 40];
        ensure_payload_capacity(&mut small);
        assert_eq!(small.len(), OPUS_MAX_PAYLOAD);

        // 偏大（一次超大 insert 之后）→ 截回整帧，且不保留多余容量
        let mut big = vec![0u8; 8192];
        ensure_payload_capacity(&mut big);
        assert_eq!(big.len(), OPUS_MAX_PAYLOAD);
        assert!(
            big.capacity() <= OPUS_MAX_PAYLOAD,
            "超额容量未释放: {}",
            big.capacity()
        );

        // 已经正好：长度与容量都不动
        let mut exact = vec![0u8; OPUS_MAX_PAYLOAD];
        let cap = exact.capacity();
        ensure_payload_capacity(&mut exact);
        assert_eq!(exact.len(), OPUS_MAX_PAYLOAD);
        assert_eq!(exact.capacity(), cap);
    }

    /// PCM 量化必须无损往返，并显式收敛溢出/NaN（Rust 浮点转整数的饱和语义）。
    #[test]
    fn short_float_conversion_roundtrips_and_saturates() {
        let input = [i16::MIN, -32767, -1, 0, 1, 32767];
        let mut floats = Vec::new();
        let mut shorts = Vec::new();
        shorts_to_floats(&input, &mut floats);
        assert_eq!(floats[0], -1.0);
        floats_to_shorts(&floats, &mut shorts);
        assert_eq!(shorts, input.to_vec());

        floats_to_shorts(&[2.0, -2.0, f32::NAN], &mut shorts);
        assert_eq!(shorts, vec![32767, -32768, 0], "溢出必须饱和，NaN 归零");
    }
}
