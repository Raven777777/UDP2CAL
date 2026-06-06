package com.udp2mic.app.native

object OpusNative {
    init {
        System.loadLibrary("opus_jni")
    }

    external fun encoderCreate(
        sampleRate: Int, bitrate: Int,
        complexity: Int, signalType: Int, bandwidth: Int,
        dtx: Int, vbr: Int, fec: Int, packetLoss: Int, vbrConstraint: Int
    ): Long
    external fun encoderEncode(handle: Long, pcmData: ShortArray): ByteArray?
    /** 编码到预分配缓冲区 dest[offset..offset+n]，返回写入字节数，零分配 */
    external fun encoderEncodeTo(handle: Long, pcmData: ShortArray, dest: ByteArray, offset: Int): Int
    external fun encoderGetFrameSize(handle: Long): Int
    external fun encoderDestroy(handle: Long)
    external fun encoderUpdate(handle: Long, complexity: Int, signalType: Int, bandwidth: Int, dtx: Int, vbr: Int, bitrate: Int, fec: Int, packetLoss: Int, vbrConstraint: Int): Boolean
}
