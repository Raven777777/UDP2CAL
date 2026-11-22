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
//! 变成纯相位表密度问题：相位取最近档（最近邻落在末档之后时进位到下一
//! 输入样本的相位 0），误差 ≤`1/(2×32) = 0.015625` 样本，与比率无关。
//!
//! 处理流程：
//! 1. 把上一块的尾段 `hist` 与本块拼成 `work`（交错）；
//! 2. `next_pos` 是下一个输出点在**全局输入样本轴**上的位置，扣除本块
//!    起始下标即块内连续位置（可为负＝仍在历史段）；跨块不做位置减法，
//!    因此任意分块与一次大块的输出逐样本一致；
//! 3. 只有当抽头窗口完全落在已知数据内时才发出样本——不做外插，
//!    也不需要"比率恰好为 1 就直通"的分支（群延迟固定 ≈24.5 样本，恒定无害）；
//! 4. 保存本块尾段，供下一块跨块插值。
//!
//! 相位表按每相 DC 增益归一（`sum == 1`），否则恒定直流会随相位留下纹波。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::rtp::RESAMPLE_MAX_ADJUST;

/// 相位分辨率
pub const RESAMPLE_PHASES: usize = 32;
/// 每相抽头数（偶数，对称）
pub const RESAMPLE_TAPS: usize = 48;
/// 半窗长
pub const RESAMPLE_HALF: usize = RESAMPLE_TAPS / 2;
/// 归一化截止频率上限（cycles/sample，以输入样本计）：0.42 × 48 kHz =
/// 20.2 kHz，加上过渡带后阻带起点约 0.49，折返分量落在带外。
///
/// 上采样与 1:1 用该值；降采样时输出 Nyquist 换算回输入样本是 `0.5/ratio`，
/// 截止必须收紧到 `min(0.42, 0.5/ratio)`，否则高于新 Nyquist 的输入会原样
/// 折返（96k→48k 时 26 kHz 以上曾 0 dB 混叠）。
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

/// 降采样相位表的比率量化步数。
///
/// 比率向上取整到 `1/RESAMPLE_RATIO_QUANT` 档后再生成表，保证表截止
/// `0.5/量化比率 <= 0.5/实际比率`——宁可过渡带略窄，也不能留下混叠。
/// 支持的比率范围最多约 640 档，运行时通常只缓存 1–2 张表。
const RESAMPLE_RATIO_QUANT: f64 = 64.0;

/// 一个满幅输入块在最小比率下可能产出的最大帧数。
///
/// 按全局 `next_pos` 轴：块首位置可低至块首输入下标 `-(HALF+1)`（上一块
/// 停下的位置），发出条件是 `next_pos < 块尾下标 -(HALF+1)`，因此一个
/// `MAX_INPUT_FRAMES` 块的输出不超过 `MAX_INPUT_FRAMES / MIN_RATIO + 1`；
/// 多出的 `RESAMPLE_HIST + 1` 是边界取整与历史的余量。
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
    /// 下一个输出点在**全局输入样本轴**上的位置。
    ///
    /// 用全局位置而不是“相对本块的 `pos` + 每块减 frames”，是为了让
    /// 任意分块顺序产生完全相同的相位序列：跨块不再做 `pos -= frames`
    /// 的浮点减法，分块与单块逐样本一致。
    next_pos: f64,
    /// 当前输入块第一帧在全局输入样本轴上的下标（只用低 53 位精度）。
    input_base: u64,
    /// 当前比率对应的相位表（按降采样比率分档缓存，见 [`coef_table`]）
    coefs: Arc<CoefTable>,
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
            next_pos: 0.0,
            input_base: 0,
            coefs: coef_table(1.0),
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
        // 降采样时换用更窄的截止（上采样/1:1 仍命中 0.42 那张表）
        self.coefs = coef_table(self.base_ratio);
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
        self.next_pos = 0.0;
        self.input_base = 0;
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
        //
        // 判定用全局输入位置：`next_pos < block_end - HALF - 1`。因此不足
        // HALF+2 帧的小块也会把“窗口只依赖历史段”的样本发出去，剩余样本
        // 留在下一块继续，不再出现整块零输出与相位钳位丢样。
        let table = &self.coefs;
        let ratio = self.ratio();
        let block_base = self.input_base as f64;
        let block_end = self.input_base + frames as u64;
        let mut next_pos = self.next_pos;
        let limit = block_end as f64 - RESAMPLE_HALF as f64 - 1.0;
        while next_pos < limit {
            // 本块内的连续位置；可为负（窗口左侧落在 hist 里）
            let pos = next_pos - block_base;
            let floor = pos.floor();
            let frac = pos - floor;
            // 最近邻取相位；frac 落入末档右半（≥31.5/32）时进位到下一输入
            // 样本的相位 0（见 [`nearest_phase`]）。
            let (base, phase) = nearest_phase(floor, frac);
            let coefs = &table[phase];
            // work 中本块第 i 帧的下标是 i + HIST；
            // 抽头窗口从帧 base + 1 - HALF 开始，乘声道数换成样本下标。
            let w0 = base as isize + (RESAMPLE_HIST as isize + 1 - RESAMPLE_HALF as isize);
            debug_assert!(
                w0 >= 0,
                "相位越过历史段下界: pos={pos}（next_pos={next_pos}）"
            );
            let w0 = w0 as usize * channels;
            for c in 0..channels {
                let mut acc = 0.0f32;
                for (j, &coef) in coefs.iter().enumerate() {
                    acc += coef * self.work[w0 + j * channels + c];
                }
                out.push(acc);
            }
            next_pos += ratio;
        }
        self.next_pos = next_pos;
        self.input_base = block_end;

        // 3) 保存尾段作为下一块的历史
        let tail = self.work.len() - RESAMPLE_HIST * channels;
        self.hist.copy_from_slice(&self.work[tail..]);

        out.len() / channels
    }
}

type CoefTable = [[f32; RESAMPLE_TAPS]; RESAMPLE_PHASES];

/// 把块内小数位置映射到（整数基准, 相位档）。
///
/// 最近邻为第 [`RESAMPLE_PHASES`] 档（`frac ≥ 31.5/32`）时，取下一输入样本
/// 的相位 0，即 `(base + 1, 0)`；这是与相位 32 等价且无需额外表项的表达。
/// 若像旧实现那样钳回 31 档，最坏误差会从 `1/(2×32)` 退化为 `1/32`。
#[inline]
fn nearest_phase(base: f64, frac: f64) -> (f64, usize) {
    let phase = (frac * RESAMPLE_PHASES as f64 + 0.5) as usize;
    if phase >= RESAMPLE_PHASES {
        (base + 1.0, 0)
    } else {
        (base, phase)
    }
}

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

/// 相位表缓存：key 是量化后的降采样比率（0 表示 0.42 上限表）。
///
/// 表只在 `set_source_rate` 时生成/查找；`process_into` 直接读实例里的
/// `Arc`，热路径无锁。多个 `Resampler` 共享同一份缓存。
static COEF_TABLES: OnceLock<Mutex<HashMap<u32, Arc<CoefTable>>>> = OnceLock::new();

/// 比率 ->（缓存键, 截止频率）。
///
/// 实际比率还会被 `adjust` 放大最多 0.5%，这里按最坏情况取截止，保证微调
/// 期间也不混叠。降采样时把比率**向上**量化到 1/64 档，使表截止不高于
/// 精确值 `0.5/ratio`——宁可过渡带略窄，也不能留下混叠。
fn cutoff_for_ratio(base_ratio: f64) -> (u32, f64) {
    let worst_ratio = base_ratio * (1.0 + RESAMPLE_MAX_ADJUST);
    let cutoff = (0.5 / worst_ratio).min(RESAMPLE_CUTOFF);
    if cutoff >= RESAMPLE_CUTOFF {
        (0, RESAMPLE_CUTOFF)
    } else {
        let q = ((worst_ratio * RESAMPLE_RATIO_QUANT).ceil() as u32).max(1);
        (q, 0.5 / (q as f64 / RESAMPLE_RATIO_QUANT))
    }
}

/// 取用（必要时生成）与比率匹配的相位表。
fn coef_table(base_ratio: f64) -> Arc<CoefTable> {
    let (key, cutoff) = cutoff_for_ratio(base_ratio);
    let cache = COEF_TABLES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(table) = cache.get(&key) {
        return Arc::clone(table);
    }
    let table = Arc::new(build_coef_table(cutoff));
    cache.insert(key, Arc::clone(&table));
    table
}

/// 生成一张截止为 `cutoff`（cycles/input-sample）的 Kaiser 窗多相表。
fn build_coef_table(cutoff: f64) -> CoefTable {
    let win_norm = bessel_i0(RESAMPLE_BETA);
    let mut table = [[0.0f32; RESAMPLE_TAPS]; RESAMPLE_PHASES];
    let w = 2.0 * cutoff;
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
    fn phase_rounding_error_never_exceeds_half_a_step() {
        // 相位量化的等价性质：把 [0,1) 上密集扫描的 frac 映射到 32 档，
        // 输出位置（进位后 base + phase/32）与精确位置的差必须 ≤ 1/(2×32)。
        // 旧实现把最近邻 32 钳回 31，frac 接近 1 时误差会到 1/32。
        let bound = 1.0 / (2.0 * RESAMPLE_PHASES as f64);
        let mut max_err = 0.0f64;
        const STEPS: usize = 1_000_000;
        for k in 0..STEPS {
            let frac = k as f64 / STEPS as f64;
            let (base, phase) = nearest_phase(7.0, frac);
            let got = base + phase as f64 / RESAMPLE_PHASES as f64;
            max_err = max_err.max((got - (7.0 + frac)).abs());
        }
        assert!(
            max_err <= bound,
            "最大相位误差 {max_err:.6} 超过半档 {bound:.6}"
        );
        // 进位区必须真正走到下一输入样本的相位 0，而不是钳回末档
        assert_eq!(
            nearest_phase(0.0, 1.0 - 0.5 / RESAMPLE_PHASES as f64),
            (1.0, 0)
        );
        assert_eq!(nearest_phase(0.0, 0.0), (0.0, 0));
        assert_eq!(nearest_phase(0.0, 31.4 / 32.0), (0.0, 31));
    }

    #[test]
    fn carry_at_the_last_emitted_sample_reads_the_final_work_frame() {
        // 最紧进位边界：ratio = 47999/48000，480 帧块。最后一个发出位置
        // 455×ratio = 454.9905，frac 落入进位区（≥31.5/32）；进位后
        // base = 455，最大抽头下标恰为 work 的最后一帧（每声道 529 帧），
        // 再多一帧就越界 panic。该用例把“进位路径读到最后样本”固化为回归。
        let mut r = Resampler::new(1);
        r.set_source_rate(47_999, 48_000);
        let ratio = r.ratio();
        let limit = 480.0 - RESAMPLE_HALF as f64 - 1.0;
        let mut expected = 0usize;
        let mut last = 0.0f64;
        let mut p = 0.0f64;
        while p < limit {
            last = p;
            expected += 1;
            p += ratio;
        }
        // 确认最后一个发出位置确实落在进位区（否则用例没覆盖修复点）
        let frac = last - last.floor();
        assert!(
            frac >= 1.0 - 0.5 / RESAMPLE_PHASES as f64,
            "用例未覆盖进位区: frac={frac}"
        );
        // 进位后抽头窗口的最大下标 = 每声道 work 的最后一帧
        let carried_base = last.floor() + 1.0;
        let taps_end = carried_base as usize + RESAMPLE_HIST + 1 - RESAMPLE_HALF + RESAMPLE_TAPS
            - 1;
        assert_eq!(taps_end, RESAMPLE_HIST + 480 - 1);

        let mut out = Vec::new();
        let produced = r.process_into(&[0.5f32; 480], &mut out);
        assert_eq!(produced, expected);
        assert_eq!(out.len(), expected);
        assert!(produced <= MAX_OUTPUT_FRAMES);
        assert!(out.iter().all(|v| v.is_finite()), "进位路径产出了非有限值");
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

    #[test]
    fn coef_tables_are_tiered_and_cached() {
        // 上采样 / 1:1 共用 0.42 那张表（ratio=1 行为与历史实现一致）
        let base = coef_table(1.0);
        assert!(Arc::ptr_eq(&base, &coef_table(0.5)));
        assert_eq!(cutoff_for_ratio(1.0), (0, RESAMPLE_CUTOFF));

        // 降采样按最坏比率向上量化，表截止不得高于 0.5/ratio
        let worst = 2.0 * (1.0 + RESAMPLE_MAX_ADJUST);
        let (key, cutoff) = cutoff_for_ratio(2.0);
        assert!(key > 0, "降采样必须使用独立表");
        assert!(cutoff <= 0.5 / worst, "截止 {cutoff} 高于安全上限");
        assert!((cutoff - 0.25).abs() < 0.01, "96k→48k 截止 {cutoff}");

        // 同一比率复用同一张表；更高比率只会更窄
        assert!(Arc::ptr_eq(&coef_table(2.0), &coef_table(2.0)));
        assert!(cutoff_for_ratio(4.0).1 < cutoff);
    }

    /// 确定性伪随机交错信号（不引入 rand 依赖）。
    fn noise(frames: usize, channels: usize, seed: u64) -> Vec<f32> {
        let mut x = seed | 1;
        (0..frames * channels)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                ((x >> 11) as f64 / (1u64 << 53) as f64) as f32 * 2.0 - 1.0
            })
            .collect()
    }

    /// 按 `chunks` 指定的帧数分块喂入同一段输入，返回拼接后的输出。
    fn run_chunks(src: u32, dev: u32, channels: usize, pcm: &[f32], chunks: &[usize]) -> Vec<f32> {
        let frames = pcm.len() / channels;
        let mut r = Resampler::new(channels);
        r.set_source_rate(src, dev);
        let mut out = Vec::new();
        let mut all = Vec::new();
        let mut pos = 0;
        for &n in chunks {
            let end = (pos + n).min(frames);
            r.process_into(&pcm[pos * channels..end * channels], &mut out);
            all.extend_from_slice(&out);
            pos = end;
            if pos == frames {
                break;
            }
        }
        assert_eq!(pos, frames, "分块没有覆盖全部输入");
        all
    }

    #[test]
    fn arbitrary_chunking_matches_one_block() {
        // 任意分块（含 ≤HALF+1 的极小块）必须与一次大块逐样本一致：
        // 全局相位轴避免了跨块 `pos -= frames` 的浮点误差，也消除了
        // 小块的整块零输出与相位钳位丢样。
        let total = 4_000;
        let channels = 2;
        let pcm = noise(total, channels, 0x243F_6A88_85A3_08D3);
        let pattern = [
            20usize, 7, 480, 1, 33, 2, 257, 20, 20, 25, 26, 3, 191, 1, 409,
        ];
        let mut chunks = Vec::new();
        let mut left = total;
        while left > 0 {
            for &n in &pattern {
                let n = n.min(left);
                if n == 0 {
                    break;
                }
                chunks.push(n);
                left -= n;
                if left == 0 {
                    break;
                }
            }
        }

        // ratio=1、上采样、整数/非整数降采样都覆盖。
        for &(src, dev) in &[
            (48_000u32, 48_000u32),
            (44_100, 48_000),
            (96_000, 48_000),
            (48_000, 44_100),
        ] {
            let one = run_chunks(src, dev, channels, &pcm, &[total]);
            let many = run_chunks(src, dev, channels, &pcm, &chunks);
            assert_eq!(
                one.len(),
                many.len(),
                "src={src} dev={dev}: 分块改变了输出帧数"
            );
            assert_eq!(one, many, "src={src} dev={dev}: 分块输出与单块不逐样本一致");
        }
    }

    /// 输入 `tone_hz` 正弦经 src→48 kHz 后，折返分量的幅度（dB，越负越干净）。
    ///
    /// 折返分量 = 滤波后残留在输出 Nyquist 以下的输入高频；幅度按输出 RMS
    /// 折算，通带内信号（如 12 kHz）应给出 ≈0 dB。
    fn alias_attenuation_db(src_hz: u32, tone_hz: f64) -> f64 {
        let frames = src_hz as usize / 2; // 0.5 s，保证整周期且覆盖瞬态
        let pcm: Vec<f32> = (0..frames)
            .map(|i| (2.0 * std::f64::consts::PI * tone_hz * i as f64 / src_hz as f64).sin() as f32)
            .collect();
        let mut r = Resampler::new(1);
        r.set_source_rate(src_hz, 48_000);
        let block = (src_hz as usize / 100).max(1); // 10 ms 输入块
        let mut out = Vec::new();
        let mut all = Vec::new();
        for chunk in pcm.chunks(block) {
            r.process_into(chunk, &mut out);
            all.extend_from_slice(&out);
        }
        let skip = 480.min(all.len() / 2); // 跳过 10 ms 启动瞬态
        let amp = rms(&all[skip..]) * std::f64::consts::SQRT_2;
        -20.0 * amp.max(1e-12).log10()
    }

    #[test]
    fn downsampling_suppresses_aliases() {
        // 旧实现固定 0.42 截止：96k→48k 时 26 kHz 折返 0 dB。
        // 新表按 min(0.42, 0.5/ratio) 收紧；48 抽头的过渡带较宽，26 kHz
        // 仍落在过渡带内，深阻带从更高频开始（30/40 kHz 已 ≥50 dB）。
        let a96_26 = alias_attenuation_db(96_000, 26_000.0);
        let a96_30 = alias_attenuation_db(96_000, 30_000.0);
        let a192_26 = alias_attenuation_db(192_000, 26_000.0);
        let a192_40 = alias_attenuation_db(192_000, 40_000.0);
        eprintln!(
            "折返衰减: 96k@26k={a96_26:.1} dB, 96k@30k={a96_30:.1} dB, \
             192k@26k={a192_26:.1} dB, 192k@40k={a192_40:.1} dB"
        );
        assert!(a96_26 >= 12.0, "96k→48k 26 kHz 折返仅 {a96_26:.1} dB");
        assert!(a96_30 >= 50.0, "96k→48k 30 kHz 折返仅 {a96_30:.1} dB");
        assert!(a192_26 >= 8.0, "192k→48k 26 kHz 折返仅 {a192_26:.1} dB");
        assert!(a192_40 >= 50.0, "192k→48k 40 kHz 折返仅 {a192_40:.1} dB");

        // 1:1 与上采样继续使用 0.42 截止：12 kHz 通带不得被新分档波及。
        for &(src, dev) in &[(48_000u32, 48_000u32), (44_100, 48_000)] {
            let a = alias_attenuation_db(src, 12_000.0);
            assert!(a < 0.5, "src={src} dev={dev} 的 12 kHz 通带衰减 {a:.2} dB");
        }
    }
}
