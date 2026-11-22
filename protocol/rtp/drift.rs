//! 漂移校正：目标水位定义与抗饱和 PI 反馈，统一以重采样比率输出。

use super::{FRAME_MS, FRAME_SAMPLES, OPUS_SAMPLE_RATE};

// ── 目标水位（规范 7.2）──

/// 目标总水位（20 帧）——**单一口径**：抖动缓冲 + PCM 环 + 声卡缓冲。
///
/// 旧实现把声卡缓冲排除在外（目标只约束抖动缓冲 + PCM 环），实际总水位
/// 因此被悄悄抬到 70–80 ms，"目标 60 ms"这句话失去意义。现在三者同口径
/// 相加：这个数就是"从可听点往后还压着多少音频"，也是控制器唯一的输入。
///
/// **这个值由环境实测决定：**
///
/// * v1.3.0 取 260 ms，是因为当时链路上有"每 60–120 s 一次、双向同时
///   600–1100 ms 的停摆"（手机全频段 Wi-Fi 扫描，`com.vivo.multinlp`）。
///   那 260 ms 里约 200 ms 纯粹是在扛扫描风暴——风暴一停就全是多余延迟。
/// * v1.4.0 关掉扫描（外加双端 DSCP 46）后，同一链路 4 次长跑 **0 次 >200 ms**，
///   最大空档 68–90 ms；按 ×1.6 余量取整 → **150 ms**（PCM 环 44 + 声卡 10 +
///   抖动缓冲 ~96），比 260 低 110 ms。
/// * v1.5.1 同链路双目标实测（docs/LATENCY_CALIBRATION.md §5.2）：目标 80 →
///   总延迟 83–99 ms 且 `PLC=0`。PC 正向（本端接收）默认随之降到 **80 ms**。
///   代价要写明：同一链路 >100 ms 级到达停顿，目标 150 时是 1–3 帧 PLC，
///   目标 80 时会变成 7–10 帧——所以 80 只适合**实测稳态空档 <40 ms** 的
///   链路；反向链路（Android 接收）未按 80 重测，当时仍取 150（见
///   `Udp2CalProtocol.kt` 的同名 Kotlin 常量，两端各自独立定值）。
/// * 2026-11-13 pktmon 复测：Wi-Fi/AP 会按 ~100 ms 把 10 个 10 ms 包攒批交付，
///   目标 80 下必然 7–9 帧 PLC；802.11ax + 屏幕常亮后仍有 83 ms 扫描空档。
///   为稳定优先，两端默认统一回到 **200 ms**（见 docs/LATENCY_CALIBRATION.md §5.3）。
/// * 若环境里重新出现扫描源（换路由器 / 换手机 / 对方装新 App），水位会
///   周期性地被顶穿、`adjust` 贴回 ±5000。此时用环境变量
///   [`WATER_TARGET_ENV`] 临时抬高目标（`UDP2CAL_WATER_TARGET_MS=220`），
///   不要改编译常数；定值规则是"目标 ≥ 实测最大空档 ×1.5"。
///
/// 这仍是整套实现里唯一跟环境相关的常数。**不要靠压低它去赢延迟**：80 只是
/// 干净链路的实测值，不是普适值；空档更深的链路上压低目标只会把延迟换成 PLC 破音
/// 与常年变调（见 [`STEP_TRIM_EXCESS_MS`](super::STEP_TRIM_EXCESS_MS) 的配套措施）。
pub const WATER_TARGET_MS: u32 = 200;

/// 目标水位的环境变量覆盖名（现场调参不改编译，见 [`water_target_ms`]）。
pub const WATER_TARGET_ENV: &str = "UDP2CAL_WATER_TARGET_MS";
/// 覆盖值的下界：必须 ≥ `PLC_LEAD_MS + DECODE_LIMIT_MS` 才有意义
pub const WATER_TARGET_MIN_MS: u32 = 80;
/// 覆盖值的上界：再大就是拿延迟换稳定性，超出说明环境已不适合实时音频
pub const WATER_TARGET_MAX_MS: u32 = 600;

/// 运行时目标水位：默认 [`WATER_TARGET_MS`]，可用 `UDP2CAL_WATER_TARGET_MS` 覆盖。
///
/// 只在首次调用时读一次环境变量（`OnceLock` 缓存）：控制器每 10 ms 调一次，
/// 不能每次都 `getenv`。取值被钳到 [`WATER_TARGET_MIN_MS`, `WATER_TARGET_MAX_MS`]，
/// 非法值（非数字、空串）静默退回常数。
pub fn water_target_ms() -> u32 {
    static CACHED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var(WATER_TARGET_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map(|v| v.clamp(WATER_TARGET_MIN_MS, WATER_TARGET_MAX_MS))
            .unwrap_or(WATER_TARGET_MS)
    })
}

// ── 漂移校正参数（规范 7.9）──
//
// 唯一执行手段是重采样比率，前馈管速率、反馈管相位：
//
//   实际比率 = (源/设备) × (1 + 前馈估计) × (1 + PI 输出)
//
// 前馈估计回答"发送端的 1 秒有多长"（晶振频差），PI 只负责把水位残差收干净。
// 没有第二级"丢/插 1 ms"离散回落：样本级接缝只可能比连续比率更差，而任何
// 需要"记住偏移"的中间表示（播放头 + offset + offset_frac）都说明口径选错了。

/// 重采样比率的微调上限（±0.5%）
pub const RESAMPLE_MAX_ADJUST: f64 = 0.005;
/// 比率更新周期：每 10 ms（1 帧）更新一次
pub const RATIO_UPDATE_SAMPLES: u64 = FRAME_SAMPLES as u64;
/// 单次比率更新的最大步进。0.0001/10 ms = 1%/s，
/// 到达 ±0.5% 满幅需要 0.5 s——既是变调不可闻的斜坡，也足够跟上真实漂移。
pub const RATIO_SLEW_STEP: f64 = 0.0001;
/// 比例增益：水位偏差每 1 ms → 0.002% 比率（20 ppm）。
///
/// 速率差由前馈 + 设备时钟估计承担；反馈只收**长期**残差。比例项刻意做小，
/// 再叠加 [`WATER_SMOOTH_TAU_MS`] 的长低通，
/// 90 ms 的周期性网络攒批不会变成可闻的音高调制。
pub const PI_KP_PER_MS: f64 = 0.00002;
/// 积分时间常数（秒）：只吃晶振偏置，不追网络抖动
pub const PI_TI_S: f64 = 180.0;
/// 积分项权限（±0.1% = ±1000 ppm ≈ 1.7 音分）
pub const PI_INTEGRAL_MAX: f64 = 0.001;
/// 水位低通时间常数（ms）。旧值 150 ms 会把 1.2 s 周期的攒批直接放进 PI；
/// 4 s 让"成批到达"在控制环里衰减约 20 倍，只剩长期平均水位。
pub const WATER_SMOOTH_TAU_MS: f64 = 4000.0;

/// PI 的最小更新间隔（ms）：更短的间隔只累计、不评估
pub const PI_MIN_UPDATE_MS: f64 = 5.0;

/// 漂移校正器：**只收长期残差的抗饱和 PI**。
///
/// 频差的主项（发送端媒体时钟 vs 声卡设备时钟）由
/// [`DeviceRateEstimator`](super::DeviceRateEstimator) 前馈承担；本控制器只负责把
/// 水位低通后的慢偏差收干净。因此增益刻意很小、低通很长（[`WATER_SMOOTH_TAU_MS`]
/// = 4 s），网络 90 ms 攒批不会变成可闻的音高调制。
///
/// 计时用**声卡已消费帧数**而不是媒体样本数：断流期间媒体样本不前进，墙钟
/// 却在走，用媒体样本计时会让积分项在断流时"冻结"，恢复后一次性补偿过头。
pub struct DriftController {
    device_rate: u32,
    /// 目标总水位（ms）：默认 [`WATER_TARGET_MS`]，可被环境变量/现场调参覆盖
    target_ms: u32,
    /// 低通后的总水位（ms），只用于 PI 输入
    water: f64,
    /// 当前比率微调量
    adjust: f64,
    /// 积分项
    integral: f64,
    primed: bool,
    last_device: u64,
    /// 评估次数（诊断）
    pub ratio_updates: u64,
}

impl DriftController {
    pub fn new(device_rate: u32) -> Self {
        Self::with_target(device_rate, water_target_ms())
    }

    /// 显式指定目标水位（单测与现场重定值用）。
    pub fn with_target(device_rate: u32, target_ms: u32) -> Self {
        Self {
            device_rate: device_rate.max(1),
            target_ms: target_ms.clamp(WATER_TARGET_MIN_MS, WATER_TARGET_MAX_MS),
            water: 0.0,
            adjust: 0.0,
            integral: 0.0,
            primed: false,
            last_device: 0,
            ratio_updates: 0,
        }
    }

    /// 当前目标总水位（ms）
    pub fn target_ms(&self) -> u32 {
        self.target_ms
    }

    /// 运行时重定目标（托盘 / 设置界面），保持积分与水位状态连续。
    pub fn set_target(&mut self, target_ms: u32) {
        self.target_ms = target_ms.clamp(WATER_TARGET_MIN_MS, WATER_TARGET_MAX_MS);
    }

    pub fn reset(&mut self) {
        self.water = 0.0;
        self.adjust = 0.0;
        self.integral = 0.0;
        self.primed = false;
        self.last_device = 0;
    }

    /// 当前比率微调量（供诊断/日志读取）
    pub fn adjust(&self) -> f64 {
        self.adjust
    }

    /// 低通后的水位（诊断：与瞬时水位对比可以看出控制器有多"钝"）
    pub fn water(&self) -> f64 {
        self.water
    }

    /// 积分项（诊断：长期顶满说明比率钳位不够用）
    pub fn integral(&self) -> f64 {
        self.integral
    }

    /// `water_ms` 为**单一口径总水位**（抖动缓冲 + PCM 环 + 声卡缓冲），
    /// `device_frames` 为声卡已消费的设备帧数（真实时间）。
    ///
    /// 返回反馈比率微调量：水位偏高 → 正数 → 重采样比率变大 → 每秒产出的
    /// 设备帧变少（见 `AudioWriter::set_ratio_adjust`）。
    pub fn update(&mut self, water_ms: u32, device_frames: u64) -> f64 {
        if device_frames < self.last_device {
            // 设备帧回退（AudioTrack flush / 音频路径重建）：与
            // [`DeviceRateEstimator::observe`](super::DeviceRateEstimator::observe)
            // 的回退分支一致——旧增量基线不可比，整块状态作废并以新计数
            // 重新起算。旧实现用 `saturating_sub` 恒得 0 后直接返回且**不更新
            // `last_device`**，PI / 水位低通 / 斜率限制会一直冻结，直到计数
            // 重新越过旧基线（可能是数亿帧）。
            self.reset();
            self.last_device = device_frames;
            return self.adjust;
        }
        let dframes = device_frames.saturating_sub(self.last_device);
        if dframes == 0 {
            return self.adjust;
        }
        let dt_ms = dframes as f64 * 1000.0 / self.device_rate as f64;
        if dt_ms < PI_MIN_UPDATE_MS {
            // 太密的调用只累计时间，不评估：低通与积分都按真实 dt 缩放
            return self.adjust;
        }
        self.last_device = device_frames;

        if !self.primed {
            self.primed = true;
            self.water = water_ms as f64;
            return self.adjust;
        }

        // ── 水面低通（τ=4 s：只留长期平均，90 ms 攒批衰减约 20 倍）──
        let steps = (dt_ms / FRAME_MS as f64).max(1e-3);
        let alpha = 1.0 - (-dt_ms / WATER_SMOOTH_TAU_MS).exp();
        self.water += alpha * (water_ms as f64 - self.water);

        // 4 s 低通已经把攒批噪声压到 <1 ms，这里直接用低通后的误差；
        // 不加死区，否则稳态会停在 target ± deadband，永远收不干净。
        let diff = self.water - self.target_ms as f64;

        // ── 比例项 ──
        let p = (diff * PI_KP_PER_MS).clamp(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST);

        // ── 积分项（权限 ±PI_INTEGRAL_MAX，且抗饱和：输出顶满且误差还在
        //    往同方向顶时冻结）──
        let ki = PI_KP_PER_MS / PI_TI_S;
        let i_next =
            (self.integral + ki * diff * (dt_ms / 1000.0)).clamp(-PI_INTEGRAL_MAX, PI_INTEGRAL_MAX);
        let want = p + i_next;
        let saturated = want.abs() >= RESAMPLE_MAX_ADJUST;
        if !saturated || diff * i_next < 0.0 {
            self.integral = i_next;
        }

        // ── 斜率限制（1%/s，按真实 dt 缩放）：不让比例项跳变成可闻的变调 ──
        let target = want.clamp(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST);
        let slew = RATIO_SLEW_STEP * steps.max(1.0);
        let step = (target - self.adjust).clamp(-slew, slew);
        self.adjust = (self.adjust + step).clamp(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST);
        self.ratio_updates += 1;
        self.adjust
    }
}

impl Default for DriftController {
    fn default() -> Self {
        Self::new(OPUS_SAMPLE_RATE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 10 ms 一帧推进 `frames` 帧（声卡消费帧数同步前进），返回最终的比率微调
    fn run_pi(d: &mut DriftController, water: u32, frames: u64, dev: &mut u64) -> f64 {
        for _ in 0..frames {
            *dev += 480;
            d.update(water, *dev);
        }
        d.adjust()
    }

    #[test]
    fn water_target_default_is_200_and_override_is_clamped() {
        assert_eq!(WATER_TARGET_MS, 200);
        // 同上：编译期求值（默认值必须落在允许区间内）
        const { assert!(WATER_TARGET_MIN_MS <= WATER_TARGET_MS) };
        const { assert!(WATER_TARGET_MS < WATER_TARGET_MAX_MS) };
        // 环境变量缓存后的值必须落在允许区间内（未设置时等于常数）
        assert!((WATER_TARGET_MIN_MS..=WATER_TARGET_MAX_MS).contains(&water_target_ms()));
    }

    #[test]
    fn set_target_applies_and_clamps() {
        let mut d = DriftController::with_target(48_000, WATER_TARGET_MS);
        d.set_target(120);
        assert_eq!(d.target_ms(), 120);
        d.set_target(1);
        assert_eq!(d.target_ms(), WATER_TARGET_MIN_MS);
        d.set_target(u32::MAX);
        assert_eq!(d.target_ms(), WATER_TARGET_MAX_MS);

        // 重定目标后 PI 必须按新目标算误差：水位仍停在旧目标 200 时，
        // 目标降到 80 应判“水位偏高”并加速播放排掉缓冲。
        let mut d = DriftController::with_target(48_000, WATER_TARGET_MS);
        let mut dev = 0u64;
        run_pi(&mut d, WATER_TARGET_MS, 100, &mut dev);
        d.set_target(WATER_TARGET_MIN_MS);
        let a = run_pi(&mut d, WATER_TARGET_MS, 1000, &mut dev);
        assert!(a > 0.0, "目标降低后旧水位应判偏高: {a}");
    }

    #[test]
    fn drift_on_target_does_nothing() {
        // 水位稳定在目标上：比率一动不动
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(WATER_TARGET_MS, dev);
        let a = run_pi(&mut d, WATER_TARGET_MS, 1000, &mut dev);
        assert_eq!(a, 0.0);
        assert_eq!(d.integral(), 0.0);
    }

    #[test]
    fn prebuffer_phase_winds_up_the_integral_to_the_negative_stop() {
        // 预缓冲阶段（抖动缓冲还没攒够、还没出声）水位必然低于目标。
        // 若照常喂 PI，积分会一路被压到负饱和，开播后只能按 `ki·diff·dt`
        // （≈3 ppm/s）慢慢退，稳态水位因此被顶在目标之上 10–30 ms 长达数分钟。
        // 现场日志指纹：开播瞬间 `反馈 -1100ppm`，之后 -457 → -294 → +21 → +65。
        // 因此调用方必须以“是否在出声”为门控（Windows 解码线程已如此）。
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(0, dev); // 起播瞬间：水位 0
        // 120 s 的“水位远低于目标”（预缓冲 + 断流，水位 0）：积分以
        // ≈22 ppm/s 往下积，约 45 s 触到 -1000 ppm 的权限下限。
        for _ in 0..12_000 {
            dev += 480; // 10 ms
            d.update(0, dev);
        }
        assert!(
            d.integral() <= -PI_INTEGRAL_MAX + 1e-9,
            "低水位应把积分项压到负饱和: {}",
            d.integral()
        );
    }

    #[test]
    fn drift_ratio_has_expected_sign_and_stays_bounded() {
        // 水位高出目标 40 ms：比率向上顶，但绝不越过 ±0.5%
        let high = WATER_TARGET_MS + 40;
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(high, dev);
        // KP 只有 20 ppm/ms，40 ms 偏差 → 800 ppm；积分项权限 1000 ppm
        // （Ti = 180 s，约 300 s 长满），所以比率封顶在 +0.18% 附近。
        for _ in 0..30_000 {
            dev += 480;
            let u = d.update(high, dev);
            assert!(u.abs() <= RESAMPLE_MAX_ADJUST + 1e-12, "比率越界: {u}");
            assert!(u >= -1e-12, "水位偏高时 adjust 不得为负: {u}");
        }
        assert!(
            d.integral() >= PI_INTEGRAL_MAX - 1e-9,
            "高水位应把积分项推到权限上限: {}",
            d.integral()
        );
        assert!(
            d.adjust() >= PI_INTEGRAL_MAX * 0.99,
            "高水位应把比率推到积分权限之上: {}",
            d.adjust()
        );
        // 反方向对称
        let low = WATER_TARGET_MS - 60;
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(low, dev);
        let a = run_pi(&mut d, low, 30_000, &mut dev);
        assert!(
            a <= -PI_INTEGRAL_MAX * 0.99,
            "缺水应把比率推到积分权限之下: {a}"
        );
        assert!(a > -RESAMPLE_MAX_ADJUST, "不得越过钳位: {a}");
    }

    #[test]
    fn drift_ratio_slew_is_gradual() {
        // 单次比率更新步进受限：不允许一帧之内跳到满幅（那是可闻的变调台阶）
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(300, dev);
        let mut prev = 0.0f64;
        for _ in 0..60 {
            dev += 480;
            let u = d.update(300, dev);
            assert!(
                (u - prev).abs() <= RATIO_SLEW_STEP + 1e-12,
                "步进过大: {prev} → {u}"
            );
            prev = u;
        }
        assert!(prev > 0.0);
    }

    /// 闭环仿真一帧：水位是"流入 10 ms − 流出 10 ms×(1+adjust)"的积分。
    /// 这就是真实系统里的抖动缓冲水位：比率调大 → 每帧产出变少 → 水位回落。
    fn plant_step(water: f64, adjust: f64) -> f64 {
        (water - 10.0 * adjust).clamp(0.0, 640.0)
    }

    #[test]
    fn drift_closed_loop_settles_on_target_without_offset() {
        // PI 与纯比例的根本区别：纯比例必然留静差（水位永远差一点），
        // 这里要求稳态偏差 → 0，且稳态比率 → 0（不再有任何变调）。
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        // 生产在预缓冲阶段门控 PI（见 prebuffer_phase...），这里从目标下方
        // 20 ms 起步，避免把预缓冲亏空误当成稳态偏差。
        let mut water = WATER_TARGET_MS as f64 - 20.0;
        d.update(water as u32, dev);
        let mut tail = Vec::new();
        for i in 0..40_000u64 {
            dev += 480;
            let a = d.update(water as u32, dev);
            water = plant_step(water, a);
            if i >= 35_000 {
                tail.push(water);
            }
        }
        let mean = tail.iter().sum::<f64>() / tail.len() as f64;
        // 水位以整毫秒进入控制器（生产也是整毫秒），量化会留下 < 1 帧的
        // 极限环；这里只要求收敛到目标附近，不要求浮点仿真级的零误差。
        assert!(
            (mean - WATER_TARGET_MS as f64).abs() < 8.0,
            "稳态水位必须收敛到目标: {mean}"
        );
        assert!(
            d.adjust().abs() < 3e-4,
            "稳态比率必须收敛到零: {}",
            d.adjust()
        );
    }

    #[test]
    fn drift_closed_loop_tracks_a_clock_skew() {
        // 闭环里出现 300 ppm 的持续速率差（前馈没跟上的部分）：PI 必须靠积分项
        // 把比率推到 −300 ppm 附近，让水位稳定在目标上（无静差）。
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        let mut water = WATER_TARGET_MS as f64;
        d.update(water as u32, dev);
        let skew = 300e-6;
        // Ti = 90 s、KP = 20 ppm/ms → ζ≈0.67、ω_n≈0.015 rad/s，
        // 整定时间 ≈4/(ζ·ω_n) ≈ 400 s，所以这里跑 900 s。
        for _ in 0..90_000u64 {
            dev += 480;
            let a = d.update(water as u32, dev);
            // 发送端比本机快 skew → 每 10 ms 多进来 10×skew 的媒体
            water = plant_step(water + 10.0 * skew, a);
        }
        assert!(
            (water - WATER_TARGET_MS as f64).abs() < 2.0,
            "有速率差时水位仍需钉在目标: {water}"
        );
        let total = d.adjust();
        // 发送端快 → 缓冲净增 → 必须播得更快（正的比率微调）才能把水位推回去
        assert!(
            (total - skew).abs() < 60e-6,
            "比率必须收敛到 +skew（残余 {:.0} ppm）",
            (total - skew) * 1e6
        );
    }

    #[test]
    fn drift_integral_is_anti_windup() {
        // 长期饱和不得让积分项越界蓄能，否则退饱和时会长时间反向过冲
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(300, dev);
        run_pi(&mut d, 300, 30_000, &mut dev); // 300 s 极端高水位
        assert!(
            d.integral().abs() <= PI_INTEGRAL_MAX + 1e-12,
            "积分项溢出: {}",
            d.integral()
        );
        assert!(d.adjust() <= RESAMPLE_MAX_ADJUST + 1e-12);
    }

    #[test]
    fn drift_jitter_in_band_barely_modulates_ratio() {
        // 规范 §六 风险 2：比例项不得把网络抖动变成变调。
        // ±4 ms、周期 1 s 的水位抖动 → 比率调制必须远小于 500 ppm。
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(WATER_TARGET_MS, dev);
        let mut worst = 0.0f64;
        for i in 0..3000u64 {
            dev += 480;
            let w = if (i / 100) % 2 == 0 {
                WATER_TARGET_MS + 4
            } else {
                WATER_TARGET_MS - 4
            };
            worst = worst.max(d.update(w, dev).abs());
        }
        assert!(worst < 0.0005, "抖动被放大成变调: {worst}");
    }

    #[test]
    fn drift_jitter_steps_do_not_become_a_pitch_cycle() {
        // 现场症状（v1.5.0 调参）：网络抖动每次都会敲响"水位 → 比率"环。
        // 水位是速率差的纯积分，再套一个积分就是双积分器：振铃周期几十秒，
        // 幅度顶到 ±0.5%（8.6 音分），听感就是"几十秒一次循环的变调"。
        //
        // 这里用固定的伪随机抖动台阶（每 5 s 一次、幅度 0–25 ms）喂闭环，
        // 要求任意 30 s 窗口内 adjust 的峰峰值 < 2000 ppm（≈3.5 音分）。
        // 实测：旧调参（KP=5e-5 / Ti=10 / 积分权限 ±0.5%）在同一序列上是
        // 7808 ppm ≈ 13.5 音分——正是现场"几十秒一次循环的变调"。
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        let mut water = WATER_TARGET_MS as f64;
        d.update(water as u32, dev);
        let mut seed = 0x1234_5678_9ABC_DEF0u64;
        let mut next_step_at = 50u64;
        let mut hist: Vec<f64> = Vec::new();
        for i in 0..60_000u64 {
            // 600 s
            if i == next_step_at {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let r = ((seed >> 33) as f64) / ((1u64 << 31) as f64); // 0..1
                let sign = if (seed >> 20) & 1 == 1 { 1.0 } else { -1.0 };
                water = (water + sign * 25.0 * r).clamp(0.0, 640.0);
                next_step_at = i + 500; // 5 s
            }
            dev += 480;
            let a = d.update(water as u32, dev);
            water = plant_step(water, a);
            hist.push(a);
        }
        // 后半段每 30 s 取一个窗口，看峰峰值
        let mut worst = 0.0f64;
        let mut start = 30_000usize;
        while start + 3000 <= hist.len() {
            let seg = &hist[start..start + 3000];
            let hi = seg.iter().copied().fold(f64::MIN, f64::max);
            let lo = seg.iter().copied().fold(f64::MAX, f64::min);
            worst = worst.max(hi - lo);
            start += 1000;
        }
        assert!(
            worst < 0.002,
            "抖动被放大成周期性变调（30 s 峰峰 {:.0} ppm ≈ {:.1} 音分）",
            worst * 1e6,
            worst * 1e6 / 578.0
        );
    }

    #[test]
    fn drift_ignores_repeated_updates_without_time_passing() {
        // 计时用声卡帧数：没有新的设备帧就不评估，避免忙等循环虚增积分
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(WATER_TARGET_MS, dev);
        run_pi(&mut d, 120, 100, &mut dev);
        let updates = d.ratio_updates;
        let adjust = d.adjust();
        for _ in 0..50 {
            d.update(1000, dev);
        }
        assert_eq!(d.ratio_updates, updates);
        assert_eq!(d.adjust(), adjust);
    }

    #[test]
    fn drift_rebases_when_device_frame_counter_rolls_back() {
        // AudioTrack flush / 音频路径重建会让设备累计帧数回退。旧实现
        // saturating_sub 恒得 0 后直接返回且不更新 last_device，调节会冻结
        // 到计数重新越过旧基线。回退必须视为计数源重置（同
        // DeviceRateEstimator），以新计数为基线继续评估。
        let mut d = DriftController::with_target(48_000, WATER_TARGET_MS);
        let mut dev = 5_000_000u64;
        let a = run_pi(&mut d, WATER_TARGET_MS + 60, 2000, &mut dev);
        assert!(a > 0.0, "先让控制器进入调节状态: {a}");
        assert!(d.ratio_updates > 0);

        // 设备计数回退（新音频路径从 0 附近重新起算）
        let updates_before = d.ratio_updates;
        d.update(WATER_TARGET_MS + 60, 0);
        assert_eq!(d.adjust(), 0.0, "计数源重置应清空旧调节量");

        // 之后必须从新基线继续评估，而不是永久返回旧的 adjust
        let a2 = run_pi(&mut d, WATER_TARGET_MS + 60, 2000, &mut dev);
        assert!(
            d.ratio_updates > updates_before,
            "回退后必须恢复评估（回退前 {}，回退后 {}）",
            updates_before,
            d.ratio_updates
        );
        assert!(a2 > 0.0, "回退后应恢复调节作用: {a2}");
    }

    #[test]
    fn drift_works_at_44k1_device_rate() {
        // 计时按设备率折算：44.1 kHz 上 441 帧 = 10 ms
        let high = WATER_TARGET_MS + 60;
        let mut d = DriftController::new(44_100);
        let mut dev = 0u64;
        d.update(high, dev);
        for _ in 0..2000 {
            dev += 441;
            d.update(high, dev);
        }
        assert!(
            d.adjust() > 0.001,
            "44.1 kHz 上高水位也必须顶起比率: {}",
            d.adjust()
        );
        assert!(d.ratio_updates > 1000);
    }
}
