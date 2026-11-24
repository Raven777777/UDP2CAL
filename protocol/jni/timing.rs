use super::*;

struct StepHandle {
    inner: Mutex<StepAbsorber>,
}

struct DriftHandle {
    inner: Mutex<DriftController>,
}

struct DeviceRateHandle {
    inner: Mutex<DeviceRateEstimator>,
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

/// `saDecide` / `saDecideAt` 的公共部分；`now_nanos` 为 `None` 时用实时时钟。
unsafe fn sa_decide_common(
    h: Handle,
    jb_water_ms: jint,
    pipe_ms: jint,
    can_trim: jboolean,
    target_ms: jint,
    now_nanos: Option<jlong>,
) -> jlong {
    let handle = match step_ref(h) {
        Some(x) => x,
        None => return 0,
    };
    let mut g = lock(&handle.inner);
    let water_ms = jb_water_ms.max(0) as u32;
    let pipe_ms = pipe_ms.max(0) as u32;
    let target_ms = target_ms.max(0) as u32;
    let d = match now_nanos {
        Some(nanos) => g.decide_at(
            water_ms,
            pipe_ms,
            can_trim,
            target_ms,
            instant_from_nanos(nanos),
        ),
        None => g.decide(water_ms, pipe_ms, can_trim, target_ms),
    };
    // 高 32 位帧数、低 1 位限额标志。隐式契约：`frames <= PLC_MAX_MS / FRAME_MS`（=12），
    // 若将来把上限放大到 2^31 以上，这里必须改成两个导出。
    ((d.frames as jlong) << 32) | (d.limited as jlong)
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
    sa_decide_common(h, jb_water_ms, pipe_ms, can_trim, target_ms, None)
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
    sa_decide_common(
        h,
        jb_water_ms,
        pipe_ms,
        can_trim,
        target_ms,
        Some(now_nanos),
    )
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

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_dcSetTargetMs(
    _env: Env,
    _this: jobject,
    h: Handle,
    target_ms: jint,
) {
    if let Some(handle) = drift_ref(h) {
        lock(&handle.inner).set_target(target_ms.max(0) as u32);
    }
}

handle_scalar!(
    Java_com_udp2cal_app_native_ProtocolNative_dcRatioUpdates,
    drift_ref,
    jlong,
    0,
    |g: &mut DriftController| g.ratio_updates as jlong
);

/// 环境变量给出的目标水位（Android 无环境变量，恒为 [`WATER_TARGET_MS`]）。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_udp2cal_app_native_ProtocolNative_waterTargetMs(
    _env: Env,
    _this: jobject,
) -> jint {
    water_target_ms() as jint
}
