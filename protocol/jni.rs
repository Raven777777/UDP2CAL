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
//! * `reObserve` 把 Java 纳秒换算回 `Instant`。
//!
//! 两侧速率完全同源，基准偏差只有一次读时钟的几微秒，长期不漂移。

// 这里全是 JNI 入口点，安全边界由 JVM 侧保证（参数由 `external fun` 声明约束，
// 句柄由 `Box::into_raw`/`Box::from_raw` 成对管理），不需要 Rust API 文档里的
// `# Safety` 段；行文见模块头部的「形态」与「时钟桥」。
#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use jni_sys::{
    jboolean, jbyte, jbyteArray, jchar, jdouble, jdoubleArray, jint, jintArray, jlong, jlongArray,
    jobject, jsize, jstring, JNIEnv, JNINativeInterface_,
};

use crate::rtp::{
    extend_seq, extend_ts, water_target_ms, DriftController, InsertResult, JitterBuffer, Pull,
    RateEstimator, RtpHeader, RtpPacket, StepAbsorber, ARRIVAL_TRACE, OPUS_FRAME_SAMPLES,
    OPUS_MAX_PAYLOAD, OPUS_SAMPLE_RATE, RTP_HEADER_SIZE,
};
use crate::{
    build_control, clamp_bitrate_kbps, parse_control, snap_bitrate_kbps, ConnectAckPayload,
    ConnectPayload, ControlHeader, DiscoverReply, BITRATE_AUTO_KBPS, BITRATE_MAX_KBPS,
    BITRATE_STEPS_KBPS, CONTROL_HEADER_SIZE, CONTROL_MAX_PAYLOAD, CONTROL_VERSION,
    CONNECT_ACK_PAYLOAD_SIZE, CONNECT_PAYLOAD_SIZE, DEFAULT_LISTEN_PORT, DEVICE_ID_SIZE,
    DEVICE_NAME_MAX, DISCOVER_PORT, OPUS_FRAME_MS, OPUS_PAYLOAD_TYPE, TYPE_CONNECT,
    TYPE_CONNECT_ACK, TYPE_DATA, TYPE_DISCOVER_REPLY, TYPE_DISCOVER_REQ, TYPE_MAX_KNOWN,
};

/// 原生方法收到的 `JNIEnv`：与 `jni-sys` 函数表字段同型，直接透传。
type Env = *mut JNIEnv;
type Handle = jlong;

#[inline]
unsafe fn fns(env: Env) -> &'static JNINativeInterface_ {
    &**env
}

#[inline]
fn null<T>() -> *mut T {
    std::ptr::null_mut()
}

// ═══════════════════════ 数组/字符串搬运 ═══════════════════════

/// 把 `arr[offset .. offset+len)` 拷进 `out`；越界返回 false。
unsafe fn read_bytes(env: Env, arr: jbyteArray, offset: jint, len: jint, out: &mut Vec<u8>) -> bool {
    if arr.is_null() || offset < 0 || len < 0 {
        return false;
    }
    let f = fns(env);
    let total = f.GetArrayLength.unwrap()(env, arr);
    if total < 0 || (offset as i64 + len as i64) > total as i64 {
        return false;
    }
    out.clear();
    out.resize(len as usize, 0);
    if len > 0 {
        f.GetByteArrayRegion.unwrap()(env, arr, offset, len, out.as_mut_ptr() as *mut jbyte);
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

/// 确保复用缓冲至少 `OPUS_MAX_PAYLOAD` 字节。
///
/// `read_bytes` 只取需要的长度（精确缩容），而 `JitterBuffer::pull` 要求
/// 输出切片容得下队首帧；VBR 下包长在 40–100 字节之间跳，缩过头的缓冲会让
/// 一个本来可播的帧被当成 `Wait`（时间轴不前进、播放线程空转）。
fn ensure_payload_capacity(buf: &mut Vec<u8>) {
    if buf.len() < OPUS_MAX_PAYLOAD {
        buf.resize(OPUS_MAX_PAYLOAD, 0);
    }
}

/// 把 `data` 写进 `arr[offset ..]`；容量不足返回 false。
unsafe fn write_bytes(env: Env, arr: jbyteArray, offset: jint, data: &[u8]) -> bool {
    if arr.is_null() || offset < 0 {
        return false;
    }
    let f = fns(env);
    let total = f.GetArrayLength.unwrap()(env, arr);
    if total < 0 || (offset as i64 + data.len() as i64) > total as i64 {
        return false;
    }
    if !data.is_empty() {
        f.SetByteArrayRegion.unwrap()(
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
    let arr = f.NewByteArray.unwrap()(env, data.len() as jsize);
    if arr.is_null() {
        return null();
    }
    if !data.is_empty() {
        f.SetByteArrayRegion.unwrap()(env, arr, 0, data.len() as jsize, data.as_ptr() as *const jbyte);
    }
    arr
}

unsafe fn new_longs(env: Env, data: &[jlong]) -> jlongArray {
    let f = fns(env);
    let arr = f.NewLongArray.unwrap()(env, data.len() as jsize);
    if arr.is_null() {
        return null();
    }
    f.SetLongArrayRegion.unwrap()(env, arr, 0, data.len() as jsize, data.as_ptr());
    arr
}

unsafe fn new_ints(env: Env, data: &[jint]) -> jintArray {
    let f = fns(env);
    let arr = f.NewIntArray.unwrap()(env, data.len() as jsize);
    if arr.is_null() {
        return null();
    }
    f.SetIntArrayRegion.unwrap()(env, arr, 0, data.len() as jsize, data.as_ptr());
    arr
}

unsafe fn new_doubles(env: Env, data: &[jdouble]) -> jdoubleArray {
    let f = fns(env);
    let arr = f.NewDoubleArray.unwrap()(env, data.len() as jsize);
    if arr.is_null() {
        return null();
    }
    f.SetDoubleArrayRegion.unwrap()(env, arr, 0, data.len() as jsize, data.as_ptr());
    arr
}

/// 返回 `null` 当 `s` 含无法编码的内容（现代 JNI 用 UTF-16，不会再出现 MUTF-8 问题）。
unsafe fn new_string(env: Env, s: &str) -> jstring {
    let units: Vec<u16> = s.encode_utf16().collect();
    fns(env).NewString.unwrap()(env, units.as_ptr() as *const jchar, units.len() as jsize)
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
#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_clockSync(
    _env: Env,
    _this: jobject,
    java_nanos: jlong,
) {
    set_clock_base((Instant::now(), java_nanos));
}

// ═══════════════════════ 常量导出（防两端漂移） ═══════════════════════

/// 全部整数常量，顺序见 `Udp2CalProtocolTest.nativeConstantsMatchKotlinConstants`。
#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_constants(
    env: Env,
    _this: jobject,
) -> jlongArray {
    let v: [jlong; 43] = [
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
        crate::rtp::RESYNC_GAP_MS as jlong,
        crate::rtp::STEP_TRIM_GROWTH_MS as jlong,
        crate::rtp::STEP_TRIM_EXCESS_MS as jlong,
        crate::rtp::STEP_TRIM_MAX_PER_MIN as jlong,
    ];
    new_longs(env, &v)
}

/// 全部浮点常量。
#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_constantsReal(
    env: Env,
    _this: jobject,
) -> jdoubleArray {
    let v: [jdouble; 9] = [
        crate::rtp::RESAMPLE_MAX_ADJUST,
        crate::rtp::RATIO_SLEW_STEP,
        crate::rtp::PI_KP_PER_MS,
        crate::rtp::PI_TI_S,
        crate::rtp::PI_INTEGRAL_MAX,
        crate::rtp::PI_MIN_UPDATE_MS,
        crate::rtp::WATER_SMOOTH_ALPHA,
        crate::rtp::RATE_WINDOW_S,
        crate::rtp::RATE_FEEDFORWARD_MAX,
    ];
    new_doubles(env, &v)
}

// ═══════════════════════ 控制面（纯函数） ═══════════════════════

/// `[msg_type, payload_len, id0..id7]`，解析失败返回 null。
#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_decodeControlHeader(
    env: Env,
    _this: jobject,
    buf: jbyteArray,
    len: jint,
) -> jlongArray {
    let mut data = Vec::new();
    if !read_bytes(env, buf, 0, len, &mut data) {
        return null();
    }
    let hdr = match ControlHeader::decode(&data) {
        Some(h) => h,
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

/// 与 `decodeControlHeader` 相同，但要求数据报长度精确等于 `15 + payload_len`。
#[no_mangle]
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

#[no_mangle]
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
    let payload_len = fns(env).GetArrayLength.unwrap()(env, payload);
    if payload_len < 0 {
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
#[no_mangle]
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

#[no_mangle]
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
    let p = ConnectPayload {
        reverse_port: reverse_port as u16,
        forward_ssrc: forward_ssrc as u32,
        payload_type: payload_type as u8,
        frame_ms: frame_ms as u8,
        complexity: complexity as u8,
        signal: signal as u8,
        bandwidth: bandwidth as u8,
        opus_flags: opus_flags as u8,
        bitrate_kbps: bitrate_kbps as u16,
        packet_loss: packet_loss as u8,
    };
    new_bytes(env, &p.encode())
}

/// `[reverseSsrc, payloadType, frameMs]`
#[no_mangle]
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
    let out: [jint; 3] = [p.reverse_ssrc as jint, p.payload_type as jint, p.frame_ms as jint];
    new_ints(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_ackEncode(
    env: Env,
    _this: jobject,
    reverse_ssrc: jint,
    payload_type: jint,
    frame_ms: jint,
) -> jbyteArray {
    let p = ConnectAckPayload {
        reverse_ssrc: reverse_ssrc as u32,
        payload_type: payload_type as u8,
        frame_ms: frame_ms as u8,
    };
    new_bytes(env, &p.encode())
}

/// DISCOVER_REPLY 的监听端口；非法返回 0。
#[no_mangle]
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
#[no_mangle]
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

#[no_mangle]
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

#[no_mangle]
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
#[no_mangle]
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

#[no_mangle]
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
    let hdr = RtpHeader::new(marker != 0, seq as u16, timestamp as u32, ssrc as u32);
    let mut raw = [0u8; RTP_HEADER_SIZE];
    hdr.encode(&mut raw);
    // 目标数组太小/offset 非法时返回 false，上层可记录；不再静默发旧头
    if write_bytes(env, dest, offset, &raw) {
        1
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_extendSeq(
    _env: Env,
    _this: jobject,
    highest: jlong,
    seq: jint,
) -> jlong {
    extend_seq(highest as u64, seq as u16) as jlong
}

#[no_mangle]
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
    /// 上一次 `pull` 涉及的时间戳（对应 Kotlin 的 `pulledTs`；WAIT 时保持不变）
    pulled_ts: u64,
}

struct JbHandle {
    inner: Mutex<JbInner>,
}

struct RateHandle {
    inner: Mutex<RateEstimator>,
}

struct StepHandle {
    inner: Mutex<StepAbsorber>,
}

struct DriftHandle {
    inner: Mutex<DriftController>,
}

#[inline]
unsafe fn lock<'a, T>(cell: &'a Mutex<T>) -> MutexGuard<'a, T> {
    // 中毒恢复：JNI 侧不允许把 panic 传染成永久不可用
    cell.lock().unwrap_or_else(|e| e.into_inner())
}

unsafe fn jb_ref(h: Handle) -> Option<&'static JbHandle> {
    if h == 0 {
        return None;
    }
    (h as *const JbHandle).as_ref()
}

unsafe fn rate_ref(h: Handle) -> Option<&'static RateHandle> {
    if h == 0 {
        return None;
    }
    (h as *const RateHandle).as_ref()
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

// ═══════════════════════ JitterBuffer ═══════════════════════

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbCreate(
    _env: Env,
    _this: jobject,
) -> Handle {
    let handle = Box::new(JbHandle {
        inner: Mutex::new(JbInner {
            jb: JitterBuffer::new(),
            scratch: vec![0u8; OPUS_MAX_PAYLOAD],
            pulled_len: 0,
            pulled_ts: 0,
        }),
    });
    Box::into_raw(handle) as Handle
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbDestroy(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if h != 0 {
        drop(Box::from_raw(h as *mut JbHandle));
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbReset(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if let Some(handle) = jb_ref(h) {
        let mut g = lock(&handle.inner);
        g.jb.reset();
        g.pulled_len = 0;
        g.pulled_ts = 0;
    }
}

#[no_mangle]
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
        None => return 0,
    };
    let mut g = lock(&handle.inner);
    // 暂存区借出：同一结构字段的不可变/可变借用无法通过 `g.` 路径共存
    let mut scratch = std::mem::take(&mut g.scratch);
    let mut ok = false;
    if read_bytes(env, buf, offset, length, &mut scratch) {
        let len = scratch.len();
        let pkt = RtpPacket {
            marker: marker != 0,
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
    if ok {
        1
    } else {
        0
    }
}

/// 返回 `Pull` 的判别值（0=WAIT、1=PACKET、2=PLC、3=SILENCE）。
#[no_mangle]
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
    let result = g.jb.pull(ring_ms.max(0) as u32, pipe_ms.max(0) as u32, &mut scratch[..]);
    let (code, ts, len) = match result {
        Pull::Packet { len, ts } => (1, ts, len),
        Pull::Plc { ts } => (2, ts, 0),
        Pull::Silence { ts } => (3, ts, 0),
        Pull::Wait => (0, 0, 0),
    };
    if code != 0 {
        g.pulled_ts = ts;
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

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_jbSkipFrames(
    _env: Env,
    _this: jobject,
    h: Handle,
    n: jint,
) -> jint {
    match jb_ref(h) {
        Some(handle) => {
            let mut g = lock(&handle.inner);
            g.jb.skip_frames(n.max(0) as u32) as jint
        }
        None => 0,
    }
}

macro_rules! jb_scalar {
    ($name:ident, $ret:ty, $body:expr) => {
        #[no_mangle]
        pub unsafe extern "system" fn $name(_env: Env, _this: jobject, h: Handle) -> $ret {
            match jb_ref(h) {
                Some(handle) => {
                    let g = lock(&handle.inner);
                    #[allow(clippy::redundant_closure_call)]
                    ($body)(&g)
                }
                None => Default::default(),
            }
        }
    };
}

jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbNextSeq, jlong, |g: &JbInner| g
    .jb
    .next_seq() as jlong);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbNextTs, jlong, |g: &JbInner| g
    .jb
    .next_ts() as jlong);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbHighestTs, jlong, |g: &JbInner| g
    .jb
    .highest_ts() as jlong);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbStarted,
    jboolean,
    |g: &JbInner| if g.jb.is_started() { 1 } else { 0 }
);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbPlaying,
    jboolean,
    |g: &JbInner| if g.jb.is_playing() { 1 } else { 0 }
);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbPlcRun, jint, |g: &JbInner| g
    .jb
    .plc_run() as jint);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbEpoch, jlong, |g: &JbInner| g
    .jb
    .epoch as jlong);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbPulledLength, jint, |g: &JbInner| g
    .pulled_len);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbPulledTs, jlong, |g: &JbInner| g
    .pulled_ts as jlong);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbWaterMs, jint, |g: &JbInner| g
    .jb
    .water_ms() as jint);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbContiguousFrames,
    jint,
    |g: &JbInner| g.jb.contiguous_frames() as jint
);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbReceived, jlong, |g: &JbInner| g
    .jb
    .stats
    .received as jlong);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbLate, jlong, |g: &JbInner| g
    .jb
    .stats
    .late as jlong);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbDuplicate, jlong, |g: &JbInner| g
    .jb
    .stats
    .duplicate as jlong);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbResyncs, jlong, |g: &JbInner| g
    .jb
    .stats
    .resyncs as jlong);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbSeqRealign,
    jlong,
    |g: &JbInner| g.jb.stats.seq_realign as jlong
);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbOverflow, jlong, |g: &JbInner| g
    .jb
    .stats
    .overflow as jlong);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbPlcFrames, jlong, |g: &JbInner| g
    .jb
    .stats
    .plc_frames as jlong);
jb_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_jbMaxPlcRun,
    jint,
    |g: &JbInner| g.jb.stats.max_plc_run as jint
);
jb_scalar!(Java_com_udp2cal_app_native_ProtocolNative_jbStepTrims, jlong, |g: &JbInner| g
    .jb
    .stats
    .step_trims as jlong);

/// 到达时刻换算成 Java `System.nanoTime()` 口径；查不到返回 -1。
#[no_mangle]
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
#[no_mangle]
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

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_saCreate(
    _env: Env,
    _this: jobject,
) -> Handle {
    Box::into_raw(Box::new(StepHandle { inner: Mutex::new(StepAbsorber::new()) })) as Handle
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_saDestroy(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if h != 0 {
        drop(Box::from_raw(h as *mut StepHandle));
    }
}

#[no_mangle]
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
#[no_mangle]
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
        can_trim != 0,
        target_ms.max(0) as u32,
    );
    // 高 32 位帧数、低 1 位限额标志。隐式契约：`frames <= PLC_MAX_MS / FRAME_MS`（=12），
    // 若将来把上限放大到 2^31 以上，这里必须改成两个导出。
    ((d.frames as jlong) << 32) | (d.limited as jlong)
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_saHits(
    _env: Env,
    _this: jobject,
    h: Handle,
) -> jlong {
    match step_ref(h) {
        Some(handle) => lock(&handle.inner).hits as jlong,
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_saTrimsInWindow(
    _env: Env,
    _this: jobject,
    h: Handle,
) -> jint {
    match step_ref(h) {
        Some(handle) => lock(&handle.inner).trims_in_window() as jint,
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_saTakeLimitedLog(
    _env: Env,
    _this: jobject,
    h: Handle,
) -> jboolean {
    match step_ref(h) {
        Some(handle) => lock(&handle.inner).take_limited_log() as jboolean,
        None => 0,
    }
}

// ═══════════════════════ RateEstimator ═══════════════════════

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_reCreate(
    _env: Env,
    _this: jobject,
) -> Handle {
    Box::into_raw(Box::new(RateHandle { inner: Mutex::new(RateEstimator::new()) })) as Handle
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_reDestroy(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if h != 0 {
        drop(Box::from_raw(h as *mut RateHandle));
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_reReset(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if let Some(handle) = rate_ref(h) {
        lock(&handle.inner).reset();
    }
}

/// `arrived_nanos` 是 Java `System.nanoTime()`，内部换算成 `Instant`。
#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_reObserve(
    _env: Env,
    _this: jobject,
    h: Handle,
    ts: jlong,
    arrived_nanos: jlong,
) {
    if let Some(handle) = rate_ref(h) {
        lock(&handle.inner).observe(ts as u64, instant_from_nanos(arrived_nanos));
    }
}

/// 无估计时返回 NaN（Kotlin 侧转成 null）。
#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_reFeedforward(
    _env: Env,
    _this: jobject,
    h: Handle,
) -> jdouble {
    match rate_ref(h) {
        Some(handle) => lock(&handle.inner).feedforward().unwrap_or(f64::NAN),
        None => f64::NAN,
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_reSigmaPpm(
    _env: Env,
    _this: jobject,
    h: Handle,
) -> jdouble {
    match rate_ref(h) {
        Some(handle) => lock(&handle.inner).sigma_ppm(),
        None => 0.0,
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_reUsedBuckets(
    _env: Env,
    _this: jobject,
    h: Handle,
) -> jint {
    match rate_ref(h) {
        Some(handle) => lock(&handle.inner).used_buckets() as jint,
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_reObservations(
    _env: Env,
    _this: jobject,
    h: Handle,
) -> jlong {
    match rate_ref(h) {
        Some(handle) => lock(&handle.inner).observations() as jlong,
        None => 0,
    }
}

// ═══════════════════════ DriftController ═══════════════════════

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcCreate(
    _env: Env,
    _this: jobject,
    device_rate: jint,
    target_ms: jint,
) -> Handle {
    let rate = if device_rate > 0 { device_rate as u32 } else { OPUS_SAMPLE_RATE };
    let target = if target_ms > 0 { target_ms as u32 } else { water_target_ms() };
    Box::into_raw(Box::new(DriftHandle {
        inner: Mutex::new(DriftController::with_target(rate, target)),
    })) as Handle
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcDestroy(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if h != 0 {
        drop(Box::from_raw(h as *mut DriftHandle));
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcReset(
    _env: Env,
    _this: jobject,
    h: Handle,
) {
    if let Some(handle) = drift_ref(h) {
        lock(&handle.inner).reset();
    }
}

#[no_mangle]
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

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcAdjust(
    _env: Env,
    _this: jobject,
    h: Handle,
) -> jdouble {
    match drift_ref(h) {
        Some(handle) => lock(&handle.inner).adjust(),
        None => 0.0,
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcWater(
    _env: Env,
    _this: jobject,
    h: Handle,
) -> jdouble {
    match drift_ref(h) {
        Some(handle) => lock(&handle.inner).water(),
        None => 0.0,
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcIntegral(
    _env: Env,
    _this: jobject,
    h: Handle,
) -> jdouble {
    match drift_ref(h) {
        Some(handle) => lock(&handle.inner).integral(),
        None => 0.0,
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcTargetMs(
    _env: Env,
    _this: jobject,
    h: Handle,
) -> jint {
    match drift_ref(h) {
        Some(handle) => lock(&handle.inner).target_ms() as jint,
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcRatioUpdates(
    _env: Env,
    _this: jobject,
    h: Handle,
) -> jlong {
    match drift_ref(h) {
        Some(handle) => lock(&handle.inner).ratio_updates as jlong,
        None => 0,
    }
}

/// 环境变量给出的目标水位（Android 无环境变量，恒为 [`WATER_TARGET_MS`]）。
#[no_mangle]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_waterTargetMs(
    _env: Env,
    _this: jobject,
) -> jint {
    water_target_ms() as jint
}
