package com.udp2cal.app.native

import android.util.Log

/**
 * Opus 解码器 Kotlin 封装
 * 用于解码 PC 端传来的反向音频
 */
class OpusDecoder(val sampleRateHz: Int = 48000) {
    companion object {
        private const val TAG = "OpusDecoder"
    }

    @Volatile private var handle: Long = 0
    val frameSize: Int
        get() = (sampleRateHz * 20) / 1000 // 20ms 帧

    fun start(): Boolean {
        if (handle != 0L) return true
        handle = OpusDecodeNative.decoderCreate(sampleRateHz)
        if (handle == 0L) {
            Log.e(TAG, "解码器创建失败")
            return false
        }
        Log.i(TAG, "解码器已创建: sr=$sampleRateHz")
        return true
    }

    /**
     * 解码 Opus 数据到 PCM
     * @param data Opus 压缩数据所在的 ByteArray
     * @param offset 数据起始偏移
     * @param length Opus 数据长度
     * @param pcmOut 输出 PCM 缓冲
     * @return 解码出的采样数，或 -1
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
