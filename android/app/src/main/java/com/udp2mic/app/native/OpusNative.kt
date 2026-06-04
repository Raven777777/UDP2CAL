package com.udp2mic.app.native

object OpusNative {
    init {
        System.loadLibrary("opus_jni")
    }

    external fun encoderCreate(
        sampleRate: Int, bitrate: Int,
        complexity: Int, signalType: Int, bandwidth: Int,
        dtx: Int, vbr: Int
    ): Long
    external fun encoderEncode(handle: Long, pcmData: ShortArray): ByteArray?
    external fun encoderGetFrameSize(handle: Long): Int
    external fun encoderDestroy(handle: Long)
    external fun encoderUpdate(handle: Long, complexity: Int, signalType: Int, bandwidth: Int, dtx: Int, vbr: Int, bitrate: Int): Boolean
}
