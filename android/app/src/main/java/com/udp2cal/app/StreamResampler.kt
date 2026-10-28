package com.udp2cal.app

import kotlin.math.PI
import kotlin.math.cos
import kotlin.math.roundToInt
import kotlin.math.sin
import kotlin.math.sqrt

/**
 * 相位连续的多相 FIR 重采样器（与 Windows 端 `audio.rs` 同一套几何）。
 *
 * 为什么不用两点线性插值：它的幅度响应是 |cos(ω/2)|，相位差半个样本时
 * 12 kHz 掉 3 dB、24 kHz 全陷。而比率微调让相位以 u×Fs 持续扫掠，于是
 * 那 3 dB 变成常年高频颤动（u=0.5% 时约 240 Hz）。多相 FIR 把插值误差
 * 变成纯相位表密度问题（1/(2×32) = 0.016 样本），与比率无关。
 *
 * 处理流程（与 Rust 逐行对应）：
 * 1. 把上一块的尾段 [hist] 与本块拼成 `work`；
 * 2. `pos` 是当前输出点落在**本块**输入上的连续位置（可为负＝仍在历史段）；
 * 3. 只有当抽头窗口完全落在已知数据内时才发出样本 —— 不做外插，
 *    也不需要"比率恰好为 1 就直通"的分支（群延迟固定 ≈24.5 样本，恒定无害）。
 */
class StreamResampler(private val channels: Int = 2) {

    /** 相位分辨率 */
    private val phases = PHASES
    private var baseRatio = 1.0
    private var adjust = 0.0

    /** 当前输出点在输入上的连续位置（本块坐标系，可为负） */
    private var pos = 0.0

    /** 每声道的历史样本（上一块的尾段） */
    private val hist = Array(channels) { FloatArray(HIST) }
    /** 每声道的拼接缓冲：hist ++ 本块 */
    private val work = Array(channels) { FloatArray(CAPACITY_FRAMES + HIST) }
    /** 输出（交错） */
    // At the minimum supported ratio (0.1 × 0.995), one input frame can
    // produce slightly more than 10 output frames. Include FIR history slack.
    private val out = ShortArray(MAX_OUTPUT_FRAMES * channels)
    private var outLen = 0

    /** 当前实际使用的比率：`源/设备 × (1 + adjust)` */
    val ratio: Double get() = baseRatio * (1.0 + adjust)

    /** 输出缓冲（`process` 的返回值是**short 个数**，不是帧数） */
    fun buffer(): ShortArray = out

    /** 源采样率变化：重建比率并清空历史（换了一条输入时间线） */
    fun setSourceRate(sourceRate: Int, deviceRate: Int) {
        val dev = deviceRate.coerceAtLeast(1)
        val r = sourceRate.toDouble() / dev
        baseRatio = if (r in 0.1..10.0) r else 1.0
        pos = 0.0
        for (c in 0 until channels) hist[c].fill(0f)
        outLen = 0
    }

    /** 比率微调（±0.5%），由漂移控制器每 10 ms 更新一次 */
    fun setAdjust(u: Double) {
        adjust = if (u.isNaN() || u.isInfinite()) {
            0.0
        } else {
            u.coerceIn(-MAX_ADJUST, MAX_ADJUST)
        }
    }

    fun reset() {
        pos = 0.0
        for (c in 0 until channels) hist[c].fill(0f)
        outLen = 0
    }

    /**
     * 重采样 [frames] 帧（交错、每帧 [channels] 个 short）。
     *
     * @return 写入 [buffer] 的 short 个数（0 表示本块没有输出）
     */
    fun process(src: ShortArray, offset: Int, frames: Int): Int {
        outLen = 0
        if (frames <= 0 || frames > CAPACITY_FRAMES) return 0

        // 1) 拼 work = hist ++ 本块（short → float，满量程 1.0）
        for (c in 0 until channels) {
            val w = work[c]
            System.arraycopy(hist[c], 0, w, 0, HIST)
            var si = offset + c
            var di = HIST
            for (i in 0 until frames) {
                w[di] = src[si] / 32768.0f
                si += channels
                di++
            }
        }

        // 2) 相位推进（只有窗口完全落在已知数据内才发出样本）
        val table = COEFS
        val r = ratio
        var p = pos
        if (frames > HALF + 1) {
            val limit = frames - HALF - 1.0
            while (p < limit) {
                val base = kotlin.math.floor(p)
                val frac = p - base
                val phase = (frac * PHASES).toInt().coerceIn(0, PHASES - 1)
                val coefs = table[phase]
                // pcm[i] 在 work 中的下标是 i + HIST，
                // 而抽头窗口从下标 base + 1 - HALF 开始
                val w0 = base.toInt() + (HIST + 1 - HALF)
                var oi = outLen * channels
                for (c in 0 until channels) {
                    val w = work[c]
                    var acc = 0.0f
                    for (j in 0 until TAPS) acc += coefs[j] * w[w0 + j]
                    out[oi] = (acc * 32768.0f).roundToInt().coerceIn(-32768, 32767).toShort()
                    oi++
                }
                outLen++
                p += r
            }
        }
        pos = p - frames
        // 防御：连续收到极小输入块时 pos 会一路后退，钳在历史段范围内
        val floorPos = -(HALF + 1).toDouble()
        if (pos < floorPos) pos = floorPos

        // 3) 保存尾段作为下一块的历史
        for (c in 0 until channels) {
            System.arraycopy(work[c], frames, hist[c], 0, HIST)
        }
        return outLen * channels
    }

    companion object {
        /** 每帧最多的输入帧数（一帧 Opus 是 480） */
        const val CAPACITY_FRAMES = 4096
        /** 相位分辨率 */
        const val PHASES = 32
        /** 每相抽头数（偶数，对称） */
        const val TAPS = 48
        /** 半窗长 */
        const val HALF = TAPS / 2
        /** 归一化截止频率（cycles/sample）：0.42 × 48 kHz = 20.2 kHz */
        const val CUTOFF = 0.42
        /** Kaiser 窗 β（≈100 dB 阻带） */
        const val BETA = 10.0
        /** 历史样本数：≥ HALF+1 */
        const val HIST = TAPS + 1
        /** 与 Udp2CalProtocol.RESAMPLE_MAX_ADJUST 同值（避免循环依赖） */
        private const val MAX_ADJUST = 0.005
        /** Maximum output for a full input block, plus phase/history boundary slack. */
        private val MAX_OUTPUT_FRAMES =
            (CAPACITY_FRAMES / (0.1 * (1.0 - MAX_ADJUST))).toInt() + HIST + 1
        /** 零阶修正贝塞尔函数（Kaiser 窗用） */
        private fun besselI0(x: Double): Double {
            val y = x * x / 4.0
            var sum = 1.0
            var term = 1.0
            for (k in 1 until 40) {
                term *= y / (k.toDouble() * k.toDouble())
                sum += term
                if (term < sum * 1e-17) break
            }
            return sum
        }

        /** 相位表：首次使用时计算，每相单独按 DC 增益归一 */
        private val COEFS: Array<FloatArray> by lazy {
            val winNorm = besselI0(BETA)
            val w = 2.0 * CUTOFF
            Array(PHASES) { ph ->
                val frac = ph.toDouble() / PHASES
                val coefs = FloatArray(TAPS)
                var sum = 0.0
                for (j in 0 until TAPS) {
                    // 抽头 j 对应的输入样本与插值中心的距离（单位：样本）
                    val x = j - (HALF - 1) - frac
                    val sinc = if (kotlin.math.abs(x) < 1e-9) 1.0 else sin(PI * w * x) / (PI * w * x)
                    val u = x / HALF
                    val win = if (kotlin.math.abs(u) >= 1.0) 0.0 else besselI0(BETA * sqrt(1.0 - u * u)) / winNorm
                    val h = w * sinc * win
                    coefs[j] = h.toFloat()
                    sum += h
                }
                // 每相归一：保证 DC 增益精确为 1
                val inv = 1.0 / sum
                for (j in 0 until TAPS) coefs[j] = (coefs[j] * inv).toFloat()
                coefs
            }
        }
    }
}
