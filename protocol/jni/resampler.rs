use super::*;

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

unsafe fn rs_ref(h: Handle) -> Option<&'static RsHandle> {
    if h == 0 {
        return None;
    }
    (h as *const RsHandle).as_ref()
}

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

#[cfg(test)]
mod tests {
    use super::*;

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
