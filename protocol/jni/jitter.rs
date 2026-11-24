use super::*;

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

/// 借出 `JbInner` 的复用暂存区执行 `f`，无论 `f` 走哪条返回路径都把暂存区
/// 放回句柄（提前 `return` 也不会丢缓冲）。
fn with_scratch<R>(g: &mut JbInner, f: impl FnOnce(&mut JitterBuffer, &mut Vec<u8>) -> R) -> R {
    let mut scratch = std::mem::take(&mut g.scratch);
    ensure_payload_capacity(&mut scratch);
    let result = f(&mut g.jb, &mut scratch);
    // `read_bytes` 会精确缩容到本次载荷长度；放回去前恢复成整帧容量，
    // 否则下一次 pull 会因输出切片不够长而把可播帧当成 Wait（VBR 下必现）
    ensure_payload_capacity(&mut scratch);
    g.scratch = scratch;
    result
}

unsafe fn jb_ref(h: Handle) -> Option<&'static JbHandle> {
    if h == 0 {
        return None;
    }
    (h as *const JbHandle).as_ref()
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
    with_scratch(&mut g, |jb, scratch| {
        if !read_bytes(env, buf, offset, length, scratch) {
            return false;
        }
        let len = scratch.len();
        let pkt = RtpPacket {
            marker,
            payload_type: OPUS_PAYLOAD_TYPE,
            seq: seq as u16,
            timestamp: ts as u32,
            ssrc: 0,
            payload: &scratch[..len],
        };
        !matches!(jb.insert(&pkt), InsertResult::Dropped)
    })
}

/// `jbPull` / `jbPullTrim` 的公共部分：加锁、借暂存区、`Pull → (code, len)`
/// 映射，以及“帧已从缓冲取走、但写回 Java 数组失败”时降级成 PLC。
unsafe fn jb_pull_common(
    env: Env,
    h: Handle,
    out: jbyteArray,
    op: impl FnOnce(&mut JitterBuffer, &mut [u8]) -> Pull,
) -> jint {
    let handle = match jb_ref(h) {
        Some(x) => x,
        None => return 0,
    };
    let mut g = lock(&handle.inner);
    g.pulled_len = 0;
    let result = with_scratch(&mut g, |jb, scratch| op(jb, &mut scratch[..]));
    let (code, len) = match result {
        Pull::Packet { len, .. } => (1, len),
        Pull::Plc { .. } => (2, 0),
        Pull::Silence { .. } => (3, 0),
        Pull::Wait => (0, 0),
    };
    if code == 0 {
        return code;
    }
    g.pulled_len = len as jint;
    if len > 0 && !write_bytes(env, out, 0, &g.scratch[..len]) {
        // 帧已经从缓冲里取走了（游标已前进）：不能对外报 Wait（那会让上层
        // 以为没东西可播，而时间轴其实已经跳了一帧）。降级成 PLC，
        // 时间轴与声音都不缺，代价只是这一帧的载荷丢了。
        g.pulled_len = 0;
        return 2;
    }
    code
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
    jb_pull_common(env, h, out, |jb, buf| {
        jb.pull(ring_ms.max(0) as u32, pipe_ms.max(0) as u32, buf)
    })
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
    let len = match with_scratch(&mut g, |jb, scratch| jb.peek_current(&mut scratch[..])) {
        Some(n) => n,
        None => return 0,
    };
    let ok = write_bytes(env, out, 0, &g.scratch[..len]);
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
    jb_pull_common(env, h, out, |jb, buf| jb.pull_trim(buf))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
