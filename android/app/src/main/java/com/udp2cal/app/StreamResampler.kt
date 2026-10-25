package com.udp2cal.app

import kotlin.math.abs
import kotlin.math.cos
import kotlin.math.roundToInt

/**
 * 流式重采样器（相位连续线性插值，比率可在 ±0.5% 内微调）。
 *
 * 与 Windows 端 `audio.rs::AudioWriter` 的重采样器**同语义**：
 *   * 比率 = `源/设备 × (1 + adjust)`，`adjust > 0` → 每秒产出的设备帧变少
 *     → 缓冲水位回落；
 *   * 相位跨块连续（`phase` 残留在 [0, ratio) 内），不会在块边界产生接缝；
 *   * 比率是连续量，改它不会在 PCM 里留下任何编辑痕迹 —— 这是漂移校正
 *     的**主力手段**，取代了旧的"500 ms 丢/插 5 ms"离散校正。
 *
 * 非线程安全：由反向播放协程独占。
 */
class StreamResampler(private val channels: Int = 2) {
    companion object {
        /** 输出缓冲按帧计的容量：一次只喂 10 ms（480 帧），这里留 8 倍余量 */
        private const val CAPACITY_FRAMES = 4096
    }

    /** 固定比率 = 源采样率 / 设备采样率 */
    private var baseRatio = 1.0

    /** 比率微调量（±0.005） */
    private var adjust = 0.0

    /** 输出相位（输入样本为单位），跨块连续 */
    private var phase = 0.0

    private val out = ShortArray(CAPACITY_FRAMES * channels)

    /** 当前生效比率（含微调），供统计/日志 */
    val ratio: Double get() = baseRatio * (1.0 + adjust)

    fun setSourceRate(sourceRate: Int, deviceRate: Int) {
        val r = if (deviceRate > 0) sourceRate.toDouble() / deviceRate else 1.0
        baseRatio = if (r in 0.1..10.0) r else 1.0
        phase = 0.0
    }

    /**
     * 设置比率微调量。超出 ±0.5% 的值被钳制，`NaN` 视为 0：
     * 宁可少校正一次，也不能让音箱里出现变调或静音。
     */
    fun setAdjust(u: Double) {
        adjust = if (u.isFinite()) {
            u.coerceIn(-Udp2CalProtocol.RESAMPLE_MAX_ADJUST, Udp2CalProtocol.RESAMPLE_MAX_ADJUST)
        } else {
            0.0
        }
    }

    fun reset() {
        phase = 0.0
    }

    /** 输出缓冲（[process] 的结果写在这里） */
    fun buffer(): ShortArray = out

    /**
     * 重采样 [frames] 个交错帧。
     *
     * @return 写入 [buffer] 的 **short 数**（含所有声道）；0 表示比率为 1，
     *         调用方应直接写原始 PCM。
     */
    fun process(src: ShortArray, offset: Int, frames: Int): Int {
        if (frames <= 0) return 0
        val r = ratio
        if (abs(r - 1.0) < 1e-9) return 0
        val n = frames.toDouble()
        var ph = phase
        var o = 0
        val last = frames - 1
        while (ph < n) {
            val idx = ph.toInt()
            val frac = ph - idx
            val i0 = if (idx < frames) idx else last
            val i1 = if (idx + 1 < frames) idx + 1 else last
            val b0 = offset + i0 * channels
            val b1 = offset + i1 * channels
            if (o + channels > out.size) break // 防御：容量永不应触顶
            for (c in 0 until channels) {
                val a = src[b0 + c].toInt()
                val b = src[b1 + c].toInt()
                out[o++] = (a + (b - a) * frac).roundToInt()
                    .coerceIn(-32768, 32767).toShort()
            }
            ph += r
        }
        phase = ph - n
        return o
    }
}

/**
 * 有界回落的 PCM 编辑（与 Windows 端 `audio.rs::slip_drop/slip_insert` 同几何）。
 *
 * 两条铁律：交叉淡化长度 ≥ 1 ms；接缝处一阶连续（升余弦权重，两端导数为零）。
 * 输入/输出都是**交错立体声** `ShortArray`，帧为单位。
 */
object PcmSlip {
    private const val CH = 2

    /** 升余弦淡入权重：i 从 0 到 k-1 → 权重从 ≈0 到 ≈1 */
    private fun weight(i: Int, k: Int): Double =
        0.5 - 0.5 * cos(Math.PI * (i + 1) / (k + 1))

    /**
     * 丢弃 [k] 个帧，在帧尾 [k] 帧内交叉淡化。就地压缩，返回新帧数。
     * 帧数不足 3k 时不动，原样返回。
     */
    fun drop(pcm: ShortArray, frames: Int, k: Int): Int {
        if (k <= 0 || frames < 3 * k) return frames
        val p = frames - 2 * k
        for (i in 0 until k) {
            val w = weight(i, k)
            val a = (p + i) * CH
            val b = (p + i + k) * CH
            for (c in 0 until CH) {
                val v = pcm[a + c] * (1.0 - w) + pcm[b + c] * w
                pcm[a + c] = v.roundToInt().coerceIn(-32768, 32767).toShort()
            }
        }
        System.arraycopy(
            pcm, (p + 2 * k) * CH,
            pcm, (p + k) * CH,
            (frames - p - 2 * k) * CH
        )
        return frames - k
    }

    /**
     * 插入 [k] 个帧：把 `pcm[0..frames]` 重写进 [out]，在帧中间做 [k] 帧
     * 交叉淡化，返回新帧数（`frames + k`）。
     */
    fun insert(pcm: ShortArray, frames: Int, out: ShortArray, k: Int): Int {
        if (k <= 0 || frames < 3 * k || out.size < (frames + k) * CH) {
            // 退化路径：原样拷贝，且必须按输出缓冲的实际容量裁剪
            val copy = kotlin.math.min(frames * CH, out.size)
            System.arraycopy(pcm, 0, out, 0, copy)
            return copy / CH
        }
        val p = frames / 2
        System.arraycopy(pcm, 0, out, 0, p * CH)
        for (i in 0 until k) {
            val w = weight(i, k)
            val cur = (p + i) * CH
            val prev = (p + i - k) * CH
            for (c in 0 until CH) {
                val v = pcm[cur + c] * (1.0 - w) + pcm[prev + c] * w
                out[cur + c] = v.roundToInt().coerceIn(-32768, 32767).toShort()
            }
        }
        System.arraycopy(pcm, p * CH, out, (p + k) * CH, (frames - p) * CH)
        return frames + k
    }
}
