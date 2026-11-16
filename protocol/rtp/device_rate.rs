//! 设备时钟速率估计：用（设备累计帧, 单调时刻）拟合声卡晶振频差。

use std::time::Instant;

use super::OPUS_SAMPLE_RATE;

// ── 速率前馈参数（规范 §四）──

/// 观测窗长度（秒）。
///
/// 精度下界 `σ_δ ≈ σ_J·√12·√τ_j/T^1.5`（τ_j≈50 ms）：T=10 s 在 σ_J=2 ms 时
/// 约 49 ppm，T=30 s 约 9 ppm。典型晶振差 100 ppm 要靠 30 s 窗口才压得住，
/// 所以取 30 s；窗口未满时照常出估计并上报 1σ，日志里能直接判断可信度。
pub const RATE_WINDOW_S: f64 = 30.0;
/// 最小延迟滤波的分桶数（每桶只保留桶内最小单向时延）
pub const RATE_BUCKETS: usize = 30;
/// 前馈估计钳位（±2%）：真实晶振差 ≤100 ppm，两个量级之外才是安全边界
pub const RATE_FEEDFORWARD_MAX: f64 = 0.02;
/// 至少这么多桶（=5 s 跨度）才对外出估计
pub const RATE_MIN_BUCKETS: usize = 6;
/// 单次观测允许的最大媒体跳变：超过它说明时间线被重建（重同步、发送端
/// 重启、断流后补齐），旧窗口的 `t_local` 与 `media` 已不可比，必须作废重算。
/// 正常相邻两包只差 480 样本，这里给 2 s 的宽松门限。
pub const RATE_JUMP_MAX_SAMPLES: u64 = OPUS_SAMPLE_RATE as u64 * 2;

/// 把"设备帧进度 / QPC 时间"拟合成设备时钟相对标称采样率的频差。
///
/// 这是 Plan §3 的第三个时间参照：RTP 媒体时间、接收端 `Instant`、设备进度。
/// 旧做法用 RTP 时间戳对 `Instant`，得到的是"发送端媒体时钟相对
/// 本机单调钟"；但重采样器真正要补偿的是"发送端媒体时钟相对**声卡设备时钟**"。
/// 两者之差就是设备晶振误差，本估计器把它单独测出来，避免把网络到达抖动
/// 误当成频差。
///
/// 观测方式是每秒取一个 `(QPC 秒, 设备累计帧)` 点，对最近 [`RATE_BUCKETS`]
/// 个点做最小二乘。`feedforward()` 返回的是**比率修正量**：设备时钟快 →
/// 返回负值（重采样比率要变小，才能每秒产出更多设备帧）。
#[derive(Clone, Copy)]
struct DeviceObservation {
    t: f64,
    device: u64,
}

const EMPTY_DEVICE_OBS: DeviceObservation = DeviceObservation { t: 0.0, device: 0 };

pub struct DeviceRateEstimator {
    /// 设备标称采样率（播放回调实际使用的速率，不一定是 48 kHz）
    nominal_rate: f64,
    t0: Option<Instant>,
    dev0: u64,
    last_device: u64,
    last_epoch: u64,
    obs: [DeviceObservation; RATE_BUCKETS],
    pos: usize,
    used: usize,
    feedforward: Option<f64>,
    /// 拟合斜率相对值的不确定度（ppm，1σ）
    sigma_ppm: f64,
}

impl DeviceRateEstimator {
    pub fn new() -> Self {
        Self::with_rate(OPUS_SAMPLE_RATE)
    }

    /// 指定设备标称速率。Windows 输出设备可能是 44.1 kHz，此时不能拿 48 kHz
    /// 当基准，否则会把“设备本来就是 44.1k”误判成 −8.16% 的巨大钟差。
    pub fn with_rate(nominal_rate: u32) -> Self {
        Self {
            nominal_rate: nominal_rate.max(1) as f64,
            t0: None,
            dev0: 0,
            last_device: 0,
            last_epoch: 0,
            obs: [EMPTY_DEVICE_OBS; RATE_BUCKETS],
            pos: 0,
            used: 0,
            feedforward: None,
            sigma_ppm: 0.0,
        }
    }

    pub fn reset(&mut self) {
        let rate = self.nominal_rate as u32;
        *self = Self::with_rate(rate);
    }

    /// 当前前馈比率修正（None = 样本不足）。正值表示设备时钟偏慢。
    pub fn feedforward(&self) -> Option<f64> {
        self.feedforward
    }

    pub fn sigma_ppm(&self) -> f64 {
        self.sigma_ppm
    }

    pub fn buckets(&self) -> usize {
        self.used
    }

    /// `device_frames` 是平台报告的**累计已播放设备帧**（Windows：
    /// `AudioWriter::played_frames()`；Android：`AudioTrack` 播放头对应的累计帧）。
    pub fn observe(&mut self, device_frames: u64, now: Instant) {
        let Some(t0) = self.t0 else {
            self.t0 = Some(now);
            self.dev0 = device_frames;
            self.last_device = device_frames;
            return;
        };
        if device_frames < self.last_device {
            // 设备帧回退（AudioTrack flush / 音频路径重建）：旧窗口不可比，
            // 必须从新基准重算，而不是永远忽略后续样本。
            self.reset();
            self.observe(device_frames, now);
            return;
        }
        if device_frames == self.last_device {
            return;
        }
        // 设备进度跳变超过 2 s：说明音频路径被重建 / 长时间挂起，旧窗口不可比
        if device_frames - self.last_device > RATE_JUMP_MAX_SAMPLES {
            self.reset();
            self.observe(device_frames, now);
            return;
        }
        self.last_device = device_frames;

        let t = now.saturating_duration_since(t0).as_secs_f64();
        let bucket_secs = (RATE_WINDOW_S / RATE_BUCKETS as f64).max(0.5);
        let epoch = (t / bucket_secs) as u64;
        if epoch == self.last_epoch {
            return;
        }
        self.last_epoch = epoch;
        self.obs[self.pos] = DeviceObservation {
            t,
            device: device_frames - self.dev0,
        };
        self.pos = (self.pos + 1) % RATE_BUCKETS;
        self.used = (self.used + 1).min(RATE_BUCKETS);
        self.fit();
    }

    fn fit(&mut self) {
        let n = self.used;
        if n < RATE_MIN_BUCKETS {
            self.feedforward = None;
            return;
        }
        let mut sx = 0.0;
        let mut sy = 0.0;
        for o in self.obs[..n].iter() {
            sx += o.t;
            sy += o.device as f64;
        }
        let mx = sx / n as f64;
        let my = sy / n as f64;
        let mut sxx = 0.0;
        let mut sxy = 0.0;
        for o in self.obs[..n].iter() {
            let dx = o.t - mx;
            sxx += dx * dx;
            sxy += dx * (o.device as f64 - my);
        }
        if sxx <= 1e-9 {
            self.feedforward = None;
            return;
        }
        let slope = sxy / sxx;
        // device/s 相对设备标称速率的偏差；比率为"每个输出消耗多少输入"，故取负
        let ratio_err = slope / self.nominal_rate - 1.0;
        self.feedforward = Some((-ratio_err).clamp(-RATE_FEEDFORWARD_MAX, RATE_FEEDFORWARD_MAX));

        let mut sse = 0.0;
        for o in self.obs[..n].iter() {
            let pred = my + slope * (o.t - mx);
            let e = o.device as f64 - pred;
            sse += e * e;
        }
        let sigma_slope = if n > 2 {
            (sse / (n as f64 - 2.0) / sxx).sqrt()
        } else {
            0.0
        };
        self.sigma_ppm = if slope.abs() > 1e-9 {
            (sigma_slope / slope.abs() * 1e6).abs()
        } else {
            0.0
        };
    }
}

impl Default for DeviceRateEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn device_rate_estimator_tracks_fast_and_slow_clock() {
        for (ppm, sign) in [(200.0, -1.0), (-300.0, 1.0)] {
            let mut est = DeviceRateEstimator::new();
            let base = Instant::now();
            let rate = 48000.0 * (1.0 + ppm / 1e6);
            // 第一个点只建立基准；随后每秒一个点，攒满 6 桶后发布。
            for sec in 0..=8u64 {
                let frames = (rate * sec as f64) as u64;
                est.observe(frames, base + Duration::from_secs(sec));
            }
            let ff = est.feedforward().expect("enough buckets");
            assert!(
                (ff - sign * ppm.abs() / 1e6).abs() < 20e-6,
                "ppm={ppm} ff={ff}"
            );
            assert!(est.buckets() >= RATE_MIN_BUCKETS);
        }
    }

    #[test]
    fn device_rate_estimator_uses_device_nominal_rate() {
        let mut est = DeviceRateEstimator::with_rate(44100);
        let base = Instant::now();
        for sec in 0..=8u64 {
            est.observe(44100 * sec, base + Duration::from_secs(sec));
        }
        let ff = est.feedforward().expect("enough buckets");
        assert!(
            ff.abs() < 20e-6,
            "44.1k nominal must not look like drift: {ff}"
        );
    }

    #[test]
    fn device_rate_estimator_resets_after_jump() {
        let mut est = DeviceRateEstimator::new();
        let base = Instant::now();
        for sec in 0..=8u64 {
            est.observe(48000 * sec, base + Duration::from_secs(sec));
        }
        assert!(est.feedforward().is_some());
        // 2 s 以上的跳变说明时间线重建：旧窗必须作废
        est.observe(
            48000 * 8 + RATE_JUMP_MAX_SAMPLES + 1,
            base + Duration::from_secs(9),
        );
        assert_eq!(est.buckets(), 0);
        assert!(est.feedforward().is_none());

        // 设备帧回退同样必须重建（AudioTrack flush / 音频路径重建）
        for sec in 0..=8u64 {
            est.observe(48000 * (10 + sec), base + Duration::from_secs(10 + sec));
        }
        assert!(est.feedforward().is_some());
        est.observe(0, base + Duration::from_secs(20));
        assert_eq!(est.buckets(), 0);
        assert!(est.feedforward().is_none());
    }
}
