use super::*;

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
    let v: [jlong; 57] = [
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
        // 可选水位档位，顺序与 Kotlin Udp2CalProtocol.WATER_TARGET_STEPS_MS
        // 逐项对应（由 parity 测试守住）。
        crate::rtp::WATER_TARGET_STEPS_MS[0] as jlong,
        crate::rtp::WATER_TARGET_STEPS_MS[1] as jlong,
        crate::rtp::WATER_TARGET_STEPS_MS[2] as jlong,
        crate::rtp::WATER_TARGET_STEPS_MS[3] as jlong,
        crate::rtp::WATER_TARGET_STEPS_MS[4] as jlong,
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
}
