use super::*;

// ═══════════════════════ RTP ═══════════════════════

/// RTP 字段打包顺序：`[marker, seq, timestamp, ssrc, payloadOffset, payloadLength]`。
fn rtp_fields(p: &RtpPacket<'_>) -> [jlong; 6] {
    [
        p.marker as jlong,
        p.seq as jlong,
        p.timestamp as jlong,
        p.ssrc as jlong,
        RTP_HEADER_SIZE as jlong,
        p.payload.len() as jlong,
    ]
}

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
    new_longs(env, &rtp_fields(&p))
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
    write_longs(env, out, &rtp_fields(&p))
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
