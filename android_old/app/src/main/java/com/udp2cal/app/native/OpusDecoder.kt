package com.udp2cal.app.native

import android.util.Log

/**
 * Opus 解码器 Kotlin 封装
 * 用于解码 PC 端传来的反向音频
 * 支持单声道(1ch)和立体声(2ch)
 */
class OpusDecoder(val sampleRateHz: Int = 48000, val channels: Int = 2) {
    companion object {
        private const val TAG = "OpusDecoder"
    }

    @Volatile private var handle: Long = 0
    /** 每通道帧采样数（20ms） */
    val frameSize: Int
        get() = (sampleRateHz * 20) / 1000
    /** PCM 缓冲总大小 = frameSize × channels */
    val pcmBufferSize: Int
        get() = frameSize * channels

    fun start(): Boolean {
        if (handle != 0L) return true
        handle = OpusDecodeNative.decoderCreate(sampleRateHz, channels)
        if (handle == 0L) {
            Log.e(TAG, "解码器创建失败")
            return false
        }
        Log.i(TAG, "解码器已创建: sr=$sampleRateHz ch=$channels")
        return true
    }

    /**
     * 解码 Opus 数据到 PCM
     * @param data Opus 压缩数据所在的 ByteArray
     * @param offset 数据起始偏移
     * @param length Opus 数据长度
     * @param pcmOut 输出 PCM 缓冲（大小至少 pcmBufferSize）
     * @return 解码出的总采样数，或 -1
     */
    fun decode(data: ByteArray, offset: Int, length: Int, pcmOut: ShortArray): Int {
        val h = handle
        if (h == 0L) return -1
        return OpusDecodeNative.decoderDecode(h, data, offset, length, pcmOut, 0)
    }

    fun stop() {
        val h = handle
        if (h != 0L) {
            OpusDecodeNative.decoderDestroy(h)
            handle = 0
        }
    }

    protected fun finalize() { stop() }
}
