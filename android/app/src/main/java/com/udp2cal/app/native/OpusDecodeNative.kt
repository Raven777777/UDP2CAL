package com.udp2cal.app.native

object OpusDecodeNative {
    init {
        System.loadLibrary("opus_jni")
    }

    /** 创建 Opus 解码器，返回 handle */
    external fun decoderCreate(sampleRate: Int, channels: Int): Long

    /**
     * 解码 Opus 包为 PCM
     * @return 解码出的采样数，或 -1 失败
     */
    external fun decoderDecode(
        handle: Long, opusData: ByteArray, offset: Int, length: Int,
        pcmOut: ShortArray, pcmOffset: Int
    ): Int

    /** 销毁解码器 */
    external fun decoderDestroy(handle: Long)
}
