//! 相位连续多相 FIR 重采样器——**三端唯一实现**。
//!
//! 使用方：
//! * `windows/src/audio.rs`：播放侧，48 kHz 媒体 → 设备采样率（单声道）；
//! * `windows/src/capture.rs`：采集侧，环回端点混音率 → 48 kHz（立体声）；
//! * Android `StreamResampler.kt`：反向播放，经 [`crate::jni`] 调用同一份代码。
//!
//! 为什么不用两点线性插值：它的幅度响应是 `|cos(ω/2)|`，相位差半个样本时
//! 12 kHz 掉 3 dB、24 kHz 全陷。而比率微调让相位以 `u×Fs` 持续扫掠，于是
//! 那 3 dB 变成常年高频颤动（u=0.5% 时约 240 Hz）。多相 FIR 把插值误差
//! 变成纯相位表密度问题（`1/(2×32) = 0.016` 样本），与比率无关。
//!
//! 处理流程：
//! 1. 把上一块的尾段 `hist` 与本块拼成 `work`（交错）；
//! 2. `pos` 是当前输出点落在**本块**输入上的连续位置（可为负＝仍在历史段）；
//! 3. 只有当抽头窗口完全落在已知数据内时才发出样本——不做外插，
//!    也不需要"比率恰好为 1 就直通"的分支（群延迟固定 ≈24.5 样本，恒定无害）；
//! 4. 保存本块尾段，供下一块跨块插值。
//!
//! 相位表按每相 DC 增益归一（`sum == 1`），否则恒定直流会随相位留下纹波。

use std::sync::OnceLock;

use crate::rtp::RESAMPLE_MAX_ADJUST;

/// 相位分辨率
pub const RESAMPLE_PHASES: usize = 32;
/// 每相抽头数（偶数，对称）
pub const RESAMPLE_TAPS: usize = 48;
/// 半窗长
pub const RESAMPLE_HALF: usize = RESAMPLE_TAPS / 2;
/// 归一化截止频率（cycles/sample）：0.42 × 48 kHz = 20.2 kHz，
/// 加上过渡带后阻带起点约 0.49，折返分量落在带外
pub const RESAMPLE_CUTOFF: f64 = 0.42;
/// Kaiser 窗 β（≈100 dB 阻带）
pub const RESAMPLE_BETA: f64 = 10.0;
/// 历史样本数（每声道）：≥ HALF+1，保证相位滑到 −(HALF+1) 时仍有数据
pub const RESAMPLE_HIST: usize = RESAMPLE_TAPS + 1;

/// 单次 `process_into` 允许的最大输入帧数（64 位 Opus 帧 480 的 8 倍余量）。
/// 只用于预分配与 JNI 入参校验；核心本身不拒绝更长的块。
pub const MAX_INPUT_FRAMES: usize = 4096;

/// 支持的最小源/设备比率（`set_source_rate` 的下界）
const MIN_BASE_RATIO: f64 = 0.1;
/// 支持的最大源/设备比率
const MAX_BASE_RATIO: f64 = 10.0;

/// 一个满幅输入块在最小比率 + 最负相位下可能产出的最大帧数。
///
/// 相位最负为 `-(HALF+1)`，与满块合并后相当于最多 `MAX_INPUT_FRAMES` 帧的
/// 输入跨度，因此 `MAX_INPUT_FRAMES / MIN_RATIO` 已覆盖；多出的
/// `RESAMPLE_HIST + 1` 是边界取整余量。
pub const MAX_OUTPUT_FRAMES: usize = {
    let min_ratio = MIN_BASE_RATIO * (1.0 - RESAMPLE_MAX_ADJUST);
    (MAX_INPUT_FRAMES as f64 / min_ratio) as usize + RESAMPLE_HIST + 1
};

/// 相位连续多相 FIR 重采样器（交错 f32，任意声道数）。
pub struct Resampler {
    channels: usize,
    /// 固定比率 = 源采样率 / 设备采样率（不含微调）
    base_ratio: f64,
    /// 比率微调量（±0.005），由漂移控制器周期写入
    adjust: f64,
    /// 当前输出点落在输入上的连续位置（源样本，可为负＝仍落在历史段）
    pos: f64,
    /// 每声道 [`RESAMPLE_HIST`] 帧历史（交错，长度 `HIST * channels`）
    hist: Vec<f32>,
    /// `hist ++ 本块` 拼接缓冲（交错），复用避免高频分配
    work: Vec<f32>,
}

impl Resampler {
    pub fn new(channels: usize) -> Self {
        let channels = channels.max(1);
        Self {
            channels,
            base_ratio: 1.0,
            adjust: 0.0,
            pos: 0.0,
            hist: vec![0.0; RESAMPLE_HIST * channels],
            work: Vec::with_capacity((RESAMPLE_HIST + MAX_INPUT_FRAMES) * channels),
        }
    }

    #[inline]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// 当前实际使用的比率：`源/设备 × (1 + adjust)`
    #[inline]
    pub fn ratio(&self) -> f64 {
        self.base_ratio * (1.0 + self.adjust)
    }

    /// 当前比率微调量（已钳位）
    #[inline]
    pub fn adjust(&self) -> f64 {
        self.adjust
    }

    /// 源采样率变化：重建比率并清空历史（换了一条输入时间线）。
    /// 比率超出 `0.1..=10` 时按 1.0 处理（防御异常的设备上报值）。
    pub fn set_source_rate(&mut self, source_rate: u32, device_rate: u32) {
        let r = source_rate as f64 / device_rate.max(1) as f64;
        self.base_ratio = if (MIN_BASE_RATIO..=MAX_BASE_RATIO).contains(&r) {
            r
        } else {
            1.0
        };
        self.reset();
    }

    /// 比率微调（±0.5%），由漂移控制器周期更新
    pub fn set_ratio_adjust(&mut self, adjust: f64) {
        self.adjust = if adjust.is_finite() {
            adjust.clamp(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST)
        } else {
            0.0
        };
    }

    /// 清空相位与历史（新数据重新从静音淡入）
    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.hist.fill(0.0);
    }

    /// 重采样一块交错 f32 输入，把交错输出追加进 `out`（先清空）。
    ///
    /// `input.len()` 必须是 `channels` 的整数倍；尾部不足一帧的样本被忽略。
    /// 返回值是**输出帧数**，`out.len() == 返回值 × channels`。
    pub fn process_into(&mut self, input: &[f32], out: &mut Vec<f32>) -> usize {
        out.clear();
        let channels = self.channels;
        let frames = input.len() / channels;
        if frames == 0 {
            return 0;
        }
        let input = &input[..frames * channels];

        // 1) 拼 work = hist ++ 本块
        self.work.clear();
        self.work.extend_from_slice(&self.hist);
        self.work.extend_from_slice(input);

        // 2) 相位推进（只有窗口完全落在已知数据内才发出样本）
        let table = coef_table();
        let ratio = self.ratio();
        let mut pos = self.pos;
        if frames > RESAMPLE_HALF + 1 {
            // 窗口右端不得越出本块：base <= frames - HALF - 1
            let limit = frames as f64 - RESAMPLE_HALF as f64 - 1.0;
            while pos < limit {
                let base = pos.floor();
                let frac = pos - base;
                let phase = ((frac * RESAMPLE_PHASES as f64) as usize).min(RESAMPLE_PHASES - 1);
                let coefs = &table[phase];
                // work 中本块第 i 帧的下标是 i + HIST；
                // 抽头窗口从帧 base + 1 - HALF 开始，乘声道数换成样本下标。
                let w0 = base as isize + (RESAMPLE_HIST as isize + 1 - RESAMPLE_HALF as isize);
                debug_assert!(w0 >= 0, "相位越过历史段下界: pos={pos}");
                let w0 = w0 as usize * channels;
                for c in 0..channels {
                    let mut acc = 0.0f32;
                    for (j, &coef) in coefs.iter().enumerate() {
                        acc += coef * self.work[w0 + j * channels + c];
                    }
                    out.push(acc);
                }
                pos += ratio;
            }
        }
        self.pos = pos - frames as f64;
        // 防御：连续收到极小输入块时 pos 会一路后退，钳在历史段范围内
        let floor_pos = -((RESAMPLE_HALF + 1) as f64);
        if self.pos < floor_pos {
            self.pos = floor_pos;
        }

        // 3) 保存尾段作为下一块的历史
        let tail = self.work.len() - RESAMPLE_HIST * channels;
        self.hist.copy_from_slice(&self.work[tail..]);

        out.len() / channels
    }
}

type CoefTable = [[f32; RESAMPLE_TAPS]; RESAMPLE_PHASES];

/// 零阶修正贝塞尔函数（Kaiser 窗用）。级数展开即可，β ≤ 10 时收敛极快。
fn bessel_i0(x: f64) -> f64 {
    let y = x * x / 4.0;
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    for k in 1..40 {
        term *= y / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// 生成/取用相位表（首次调用时计算，之后只读）
fn coef_table() -> &'static CoefTable {
    static TABLE: OnceLock<CoefTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        let win_norm = bessel_i0(RESAMPLE_BETA);
        let mut table = [[0.0f32; RESAMPLE_TAPS]; RESAMPLE_PHASES];
        let w = 2.0 * RESAMPLE_CUTOFF;
        for (p, coefs) in table.iter_mut().enumerate() {
            let frac = p as f64 / RESAMPLE_PHASES as f64;
            let mut sum = 0.0f64;
            for (j, c) in coefs.iter_mut().enumerate() {
                // 抽头 j 对应的输入样本与插值中心的距离（单位：样本）
                let x = j as f64 - (RESAMPLE_HALF as f64 - 1.0) - frac;
                let sinc = if x.abs() < 1e-9 {
                    1.0
                } else {
                    (std::f64::consts::PI * w * x).sin() / (std::f64::consts::PI * w * x)
                };
                let u = x / RESAMPLE_HALF as f64;
                let win = if u.abs() >= 1.0 {
                    0.0
                } else {
                    bessel_i0(RESAMPLE_BETA * (1.0 - u * u).sqrt()) / win_norm
                };
                let h = w * sinc * win;
                *c = h as f32;
                sum += h;
            }
            // 每相单独归一：保证 DC 增益精确为 1，否则会留下低频起伏
            let inv = 1.0 / sum;
            for c in coefs.iter_mut() {
                *c = (*c as f64 * inv) as f32;
            }
        }
        table
    })
}

// ═══════════════════════════════════════════════════════════════════
// 单元测试
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f32 = 48_000.0;

    /// 12 kHz 正弦（每样本 0.25 周，采样点恰好落在 0/±1 上：
    /// 两点线性插值在相位 0.5 时对它正好 −3 dB，是最能暴露问题的信号）
    fn tone_12k(len: usize, start: usize) -> Vec<f32> {
        (0..len)
            .map(|i| (2.0 * std::f32::consts::PI * 12_000.0 * (start + i) as f32 / FS).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        if x.is_empty() {
            return 0.0;
        }
        (x.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / x.len() as f64).sqrt()
    }

    fn collect(mut r: Resampler, pcm: &[f32], blocks: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let mut all = Vec::new();
        for _ in 0..blocks {
            r.process_into(pcm, &mut out);
            all.extend_from_slice(&out);
        }
        all
    }

    #[test]
    fn ratio_one_outputs_expected_frame_rate() {
        // 48 kHz → 48 kHz：1 秒产出只慢一个固定群延迟（HALF+1 样本）
        let mut r = Resampler::new(1);
        r.set_source_rate(48_000, 48_000);
        let pcm = vec![0.1f32; 480];
        let mut out = Vec::new();
        let mut total = 0usize;
        for _ in 0..100 {
            total += r.process_into(&pcm, &mut out);
        }
        assert!((total as i64 - 47_975).abs() <= 3, "1 秒内产出 {total}");
    }

    #[test]
    fn forty_four_one_device_upsamples_48k_input() {
        // 48 kHz 源 → 44.1 kHz 设备：1 秒 48000 样本应产出约 44100 帧
        let mut r = Resampler::new(1);
        r.set_source_rate(48_000, 44_100);
        let pcm = vec![0.1f32; 480];
        let mut out = Vec::new();
        let mut total = 0usize;
        for _ in 0..100 {
            total += r.process_into(&pcm, &mut out);
        }
        assert!((total as i64 - 44_077).abs() <= 4, "1 秒内产出 {total}");
    }

    #[test]
    fn phase_continues_across_blocks() {
        // 块间相位必须连续：连续 10 块的总产出不得因取整而漏帧
        let mut r = Resampler::new(1);
        r.set_source_rate(48_000, 44_100);
        let pcm = vec![0.1f32; 480];
        let mut out = Vec::new();
        let mut total = 0usize;
        for _ in 0..10 {
            total += r.process_into(&pcm, &mut out);
        }
        assert!((total as i64 - 4_386).abs() <= 4, "10 块产出 {total}");
    }

    #[test]
    fn ratio_adjust_changes_produced_frames() {
        // adjust > 0 → 比率变大 → 每秒**少**产出约 240 帧（水位回落）
        fn one_second(adjust: f64) -> usize {
            let mut r = Resampler::new(1);
            r.set_source_rate(48_000, 48_000);
            r.set_ratio_adjust(adjust);
            let pcm = vec![0.1f32; 480];
            let mut out = Vec::new();
            let mut total = 0usize;
            for _ in 0..100 {
                total += r.process_into(&pcm, &mut out);
            }
            total
        }
        let fast = one_second(0.005);
        let slow = one_second(-0.005);
        // 两个方向的差值必须等于 2×0.5%×48000 ≈ 480 帧（固定群延迟自动抵消）
        assert!(
            (slow as i64 - fast as i64 - 480).abs() <= 3,
            "fast={fast} slow={slow}"
        );
        assert!((fast as i64 - 47_737).abs() <= 4, "fast={fast}");
        assert!((slow as i64 - 48_217).abs() <= 4, "slow={slow}");
    }

    #[test]
    fn dc_gain_is_exactly_one() {
        // 每相单独归一：恒定直流必须原样通过（±1e-6），否则水位标定会带系统偏差
        let all = collect(Resampler::new(1), &vec![0.1f32; 480], 10);
        // 前 25 个样本落在"历史静音 → 信号"的过渡里，其余必须是 0.1
        for (i, &v) in all.iter().enumerate().skip(200) {
            assert!((v - 0.1).abs() < 1e-6, "样本 {i} = {v}");
        }
    }

    #[test]
    fn tie_amplitude_is_flat_at_12k() {
        // P2 验收门：12 kHz 处幅度波动 < 0.1 dB（≈1.15%）。
        // 两点线性插值在这里会掉 3 dB，且随比率扫掠变成常年颤动。
        for adjust in [0.0f64, 0.001, -0.001, 0.005, -0.005] {
            let mut r = Resampler::new(1);
            r.set_source_rate(48_000, 48_000);
            r.set_ratio_adjust(adjust);
            let mut out = Vec::new();
            let mut all = Vec::new();
            for block in 0..50 {
                r.process_into(&tone_12k(480, block * 480), &mut out);
                all.extend_from_slice(&out);
            }
            // 跳过前 480 个样本（历史静音淡入段），其余按 RMS 折算幅度
            let body = &all[480..];
            let amp = rms(body) * std::f64::consts::SQRT_2;
            let err_db = 20.0 * (amp / 1.0).abs().log10().abs().max(1e-12);
            assert!(
                err_db < 0.1,
                "adjust={adjust}: 12 kHz 幅度 {amp:.6}（{err_db:.3} dB）"
            );
        }
    }

    #[test]
    fn no_click_at_block_boundaries() {
        // 相位连续 + 多相插值 ⇒ 块与块之间不得出现台阶
        let mut r = Resampler::new(1);
        r.set_source_rate(48_000, 48_000);
        r.set_ratio_adjust(0.003);
        let mut out = Vec::new();
        let mut all = Vec::new();
        for block in 0..20 {
            r.process_into(&tone_12k(480, block * 480), &mut out);
            all.extend_from_slice(&out);
        }
        // 12 kHz 在 48 kHz 下的最大相邻差 ≈ 2π×12k/48k ≈ 1.57
        for i in 200..all.len() - 1 {
            let d = (all[i + 1] - all[i]).abs();
            assert!(d <= 1.75, "样本 {i} 处有台阶: Δ={d}");
        }
    }

    #[test]
    fn adjust_is_clamped_and_nan_is_ignored() {
        let mut r = Resampler::new(1);
        r.set_ratio_adjust(9.0);
        assert!((r.ratio() - (1.0 + RESAMPLE_MAX_ADJUST)).abs() < 1e-12);
        r.set_ratio_adjust(f64::NAN);
        assert_eq!(r.adjust(), 0.0);
        assert_eq!(r.ratio(), 1.0);
        r.set_ratio_adjust(f64::INFINITY);
        assert_eq!(r.adjust(), 0.0);
    }

    #[test]
    fn out_of_range_source_ratio_falls_back_to_one() {
        let mut r = Resampler::new(1);
        // 48× 与 0.02× 都超出支持范围，必须按 1.0 处理
        r.set_source_rate(48_000, 1_000);
        assert_eq!(r.ratio(), 1.0);
        r.set_source_rate(1_000, 48_000);
        assert_eq!(r.ratio(), 1.0);
        // 零设备采样率不得除零
        r.set_source_rate(48_000, 0);
        assert_eq!(r.ratio(), 1.0);
    }

    #[test]
    fn reset_restarts_like_a_fresh_instance() {
        let mut used = Resampler::new(1);
        used.set_source_rate(48_000, 48_000);
        let mut out = Vec::new();
        for block in 0..5 {
            used.process_into(&tone_12k(480, block * 480), &mut out);
        }
        used.reset();

        let mut fresh = Resampler::new(1);
        fresh.set_source_rate(48_000, 48_000);
        let pcm = tone_12k(480, 5 * 480);
        let mut a = Vec::new();
        let mut b = Vec::new();
        used.process_into(&pcm, &mut a);
        fresh.process_into(&pcm, &mut b);
        assert_eq!(a, b, "reset 后必须与全新实例逐样本一致");
    }

    #[test]
    fn channels_are_independent() {
        // 立体声两路分别与单声道参考逐样本一致：抽头累加不得串道
        let left = tone_12k(480, 0);
        let right: Vec<f32> = (0..480).map(|i| 0.25 * (i as f32 * 0.01).sin()).collect();
        let mut interleaved = Vec::with_capacity(960);
        for i in 0..480 {
            interleaved.push(left[i]);
            interleaved.push(right[i]);
        }

        let mut stereo = Resampler::new(2);
        stereo.set_source_rate(44_100, 48_000);
        let mut stereo_out = Vec::new();
        stereo.process_into(&interleaved, &mut stereo_out);

        let mut mono_l = Resampler::new(1);
        mono_l.set_source_rate(44_100, 48_000);
        let mut left_out = Vec::new();
        mono_l.process_into(&left, &mut left_out);

        let mut mono_r = Resampler::new(1);
        mono_r.set_source_rate(44_100, 48_000);
        let mut right_out = Vec::new();
        mono_r.process_into(&right, &mut right_out);

        assert_eq!(stereo_out.len(), left_out.len() * 2);
        for i in 0..left_out.len() {
            assert_eq!(stereo_out[i * 2], left_out[i], "左声道样本 {i} 串道");
            assert_eq!(stereo_out[i * 2 + 1], right_out[i], "右声道样本 {i} 串道");
        }
    }

    #[test]
    fn empty_input_is_a_noop() {
        let mut r = Resampler::new(2);
        r.set_source_rate(44_100, 48_000);
        let mut out = vec![1.0f32; 8];
        assert_eq!(r.process_into(&[], &mut out), 0);
        assert!(out.is_empty());
        // 状态未动：随后一块仍按正常几何产出（首块受半窗启动影响，只查整帧）
        let produced = r.process_into(&[0.5f32; 960], &mut out);
        assert!(produced > 0);
        assert_eq!(out.len(), produced * 2);
    }

    #[test]
    fn tiny_blocks_and_high_ratio_never_panic() {
        // 回归：历史实现（捕获侧线性插值）在 ratio > 2（176.4k/192k/384k 端点）
        // 时 `drain(0..consumed*2)` 越界，捕获线程直接 panic 并永久静音。
        // 这里的实现只有相位推进，不持有消费指针，任何比率/块长都必须安全。
        for &src_hz in &[144_000u32, 176_400, 192_000, 384_000] {
            for &(base, span) in &[(64usize, 32usize), (3_840, 4), (7_680, 4)] {
                for frames in base.saturating_sub(span)..=base + span {
                    let mut r = Resampler::new(2);
                    r.set_source_rate(src_hz, 48_000);
                    let input = vec![0.25f32; frames * 2];
                    let mut out = Vec::new();
                    let produced = r.process_into(&input, &mut out);
                    assert_eq!(out.len() % 2, 0, "输出必须是整帧");
                    assert_eq!(out.len(), produced * 2);
                    let ratio = src_hz as f64 / 48_000.0;
                    let max_out = (frames as f64 / ratio).ceil() as usize + 2;
                    assert!(
                        produced <= max_out,
                        "src={src_hz} frames={frames} out={produced} max={max_out}"
                    );
                }
            }
        }
        // 连续极小输入块把相位压到历史段下限后，再给满块也不得越界
        let mut r = Resampler::new(2);
        r.set_source_rate(192_000, 48_000);
        let mut out = Vec::new();
        for _ in 0..200 {
            r.process_into(&[0.0f32; 2], &mut out);
        }
        let produced = r.process_into(&vec![0.0f32; MAX_INPUT_FRAMES * 2], &mut out);
        assert!(produced <= MAX_OUTPUT_FRAMES);
    }

    #[test]
    fn output_bound_covers_worst_case() {
        // JNI 按 MAX_OUTPUT_FRAMES 分配 Kotlin 输出数组：
        // 最小比率 + 最负相位 + 满块必须全部装得下。
        let mut r = Resampler::new(2);
        r.set_source_rate(100, 1_000); // 比率 0.1
        r.set_ratio_adjust(-RESAMPLE_MAX_ADJUST);
        let mut out = Vec::new();
        for _ in 0..100 {
            r.process_into(&[0.0f32; 2], &mut out);
        }
        let block = vec![0.0f32; MAX_INPUT_FRAMES * 2];
        for _ in 0..5 {
            let produced = r.process_into(&block, &mut out);
            assert!(
                produced <= MAX_OUTPUT_FRAMES,
                "产出 {produced} 超过上界 {MAX_OUTPUT_FRAMES}"
            );
            assert_eq!(out.len(), produced * 2);
        }
    }
}
