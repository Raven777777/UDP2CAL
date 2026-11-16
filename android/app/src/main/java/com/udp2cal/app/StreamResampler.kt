package com.udp2cal.app

import com.udp2cal.app.native.ProtocolNative

/**
 * 相位连续的多相 FIR 重采样器（JNI 胶水）。
 *
 * 算法只有一份：`protocol/resampler.rs`。Windows 播放（`audio.rs`）、Windows
 * 采集（`capture.rs`）直接调用它，Android 经 [ProtocolNative] 调用同一段
 * 机器码；本类只剩句柄与缓冲管理，不再参与任何 DSP 计算。
 *
 * 为什么不用两点线性插值：它的幅度响应是 |cos(ω/2)|，相位差半个样本时
 * 12 kHz 掉 3 dB、24 kHz 全陷。而比率微调让相位以 u×Fs 持续扫掠，于是
 * 那 3 dB 变成常年高频颤动（u=0.5% 时约 240 Hz）。多相 FIR 把插值误差
 * 变成纯相位表密度问题（1/(2×32) = 0.016 样本），与比率无关。
 *
 * 句柄在 [close] 后不得再用；反向播放循环退出时必须调用（每连接新建一个）。
 */
class StreamResampler(private val channels: Int = 2) : AutoCloseable {

    private var handle: Long = ProtocolNative.rsCreate(channels).also {
        check(it != 0L) { "channels 必须在 1..=32：$channels" }
    }

    /** 输出（交错 short）；Rust 侧保证最坏比率 + 满块也装得下 */
    private val out = ShortArray(ProtocolNative.rsMaxOutputShorts(handle))

    /** 当前实际使用的比率：`源/设备 × (1 + adjust)` */
    val ratio: Double
        get() = ProtocolNative.rsRatio(alive())

    /** 输出缓冲（[process] 的返回值是**short 个数**，不是帧数） */
    fun buffer(): ShortArray = out

    /** 源采样率变化：重建比率并清空历史（换了一条输入时间线） */
    fun setSourceRate(sourceRate: Int, deviceRate: Int) {
        ProtocolNative.rsSetSourceRate(alive(), sourceRate, deviceRate)
    }

    /** 比率微调（±0.5%），由漂移控制器每 10 ms 更新一次；钳位在 Rust 侧 */
    fun setAdjust(u: Double) {
        ProtocolNative.rsSetAdjust(alive(), u)
    }

    fun reset() {
        ProtocolNative.rsReset(alive())
    }

    /**
     * 重采样 [frames] 帧（交错、每帧 [channels] 个 short）。
     *
     * `offset` 是 [src] 中第一个样本的下标（单位：short）。
     *
     * @return 写入 [buffer] 的 short 个数（0 表示本块没有输出）
     */
    fun process(src: ShortArray, offset: Int, frames: Int): Int {
        val n = ProtocolNative.rsProcess(alive(), src, offset, frames, out)
        check(n >= 0) {
            "重采样器调用失败（frames=$frames, src=${src.size}, out=${out.size}）"
        }
        return n
    }

    override fun close() {
        if (handle != 0L) {
            ProtocolNative.rsDestroy(handle)
            handle = 0L
        }
    }

    @Suppress("deprecation")
    protected fun finalize() {
        close()
    }

    private fun alive(): Long {
        check(handle != 0L) { "StreamResampler 已释放" }
        return handle
    }

    companion object {
        /** 单次 [process] 允许的最大输入帧数；由 Rust 导出，防止两端常量漂移 */
        val CAPACITY_FRAMES: Int = ProtocolNative.rsMaxInputFrames()
    }
}
