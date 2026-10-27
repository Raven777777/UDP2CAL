package com.udp2cal.app.native

import android.util.Log
import com.udp2cal.app.Udp2CalProtocol

/**
 * Opus 解码器（v3：48 kHz / 固定 10 ms / 声道数由方向决定）
 *
 * 反向流是立体声，因此 `channels = 2`；声道数**不得**从 RTP 头推断。
 * PLC 每次只请求 480 采样（10 ms）。
 *
 * 线程约束：`start`/`decode`/`plc`/`stop`/`reset` 全部加锁。
 * `stop()` 会释放 native handle，若此时解码线程仍在 `decode` 内，
 * 将出现 native 释放后使用（概率性 SIGSEGV）。
 */
class OpusDecoder(val sampleRateHz: Int = Udp2CalProtocol.OPUS_SAMPLE_RATE, val channels: Int = 2) {
    companion object {
        private const val TAG = "OpusDecoder"
    }

    @Volatile private var handle: Long = 0

    /** 每通道帧采样数（10 ms = 480） */
    val frameSize: Int
        get() = Udp2CalProtocol.OPUS_FRAME_SAMPLES

    /** PCM 缓冲总大小 = frameSize × channels（交错） */
    val pcmBufferSize: Int
        get() = frameSize * channels

    @Synchronized
    fun start(): Boolean {
        if (handle != 0L) return true
        handle = OpusDecodeNative.decoderCreate(sampleRateHz, channels)
        if (handle == 0L) {
            Log.e(TAG, "解码器创建失败")
            return false
        }
        Log.i(TAG, "解码器已创建: sr=$sampleRateHz ch=$channels frame=$frameSize")
        return true
    }

    /**
     * 解码一个 10 ms Opus 包。
     * @return 解码出的交错采样数，或 -1（失败按丢包处理）
     */
    @Synchronized
    fun decode(data: ByteArray, offset: Int, length: Int, pcmOut: ShortArray): Int {
        val h = handle
        if (h == 0L) return -1
        if (offset < 0 || length <= 0 || offset + length > data.size) return -1
        return OpusDecodeNative.decoderDecode(h, data, offset, length, pcmOut, 0)
    }

    /**
     * 丢包隐藏：请求**一帧 10 ms**。Deep PLC 需库以 OPUS_DEEP_PLC 构建且复杂度 ≥5。
     * @return 补偿出的交错采样数，或 -1
     */
    @Synchronized
    fun plc(pcmOut: ShortArray): Int {
        val h = handle
        if (h == 0L) return -1
        return OpusDecodeNative.decoderPlc(h, pcmOut, 0)
    }

    /**
     * 用**下一个包**的 LBRR 冗余重建刚刚丢失的那一帧（Opus 带内 FEC）。
     *
     * libopus 语义：`opus_decode(..., fec = 1)` 不解析 `data` 自己的音频，而是用
     * 它携带的冗余还原**上一帧**。所以之后必须再用 [decode] 正常解一次同一个包，
     * 解码器状态才与发送端连续。包里没有 LBRR（对端没开带内 FEC）时自动退化
     * 成 PLC，不会比 [plc] 更差。
     *
     * @return 重建出的交错采样数，或 -1（调用方退回 [plc]）
     */
    @Synchronized
    fun decodeFec(data: ByteArray, offset: Int, length: Int, pcmOut: ShortArray): Int {
        val h = handle
        if (h == 0L) return -1
        if (offset < 0 || length <= 0 || offset + length > data.size) return -1
        return OpusDecodeNative.decoderDecodeFec(h, data, offset, length, pcmOut, 0)
    }

    /** 重建解码器状态（重同步 / 新会话） */
    @Synchronized
    fun reset() {
        val h = handle
        if (h != 0L) OpusDecodeNative.decoderDestroy(h)
        handle = OpusDecodeNative.decoderCreate(sampleRateHz, channels)
    }

    @Synchronized
    fun stop() {
        val h = handle
        if (h != 0L) {
            OpusDecodeNative.decoderDestroy(h)
            handle = 0
        }
    }
}
