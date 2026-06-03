package com.udp2mic.app.native

import android.util.Log
import com.udp2mic.app.Udp2MicProtocol

class OpusEncoder(
    val sampleRateHz: Int = 48000,
    val bitrateKbps: Int = 32
) {
    companion object {
        private const val TAG = "OpusEncoder"
    }

    @Volatile
    private var handle: Long = 0
    val sampleRateId: Byte = Udp2MicProtocol.hzToSampleRate(sampleRateHz)
    val bitrateId: Byte = bitrateKbps.coerceIn(0, 255).toByte()
    private var seqNum: Byte = 0
    var encodeCount: Long = 0
        private set
    var totalEncodedBytes: Long = 0
        private set
    var totalPacketBytes: Long = 0
        private set

    val frameSize: Int
        get() = if (handle != 0L) OpusNative.encoderGetFrameSize(handle) else 0

    @Synchronized
    fun start(): Boolean {
        if (handle != 0L) return true
        return try {
            handle = OpusNative.encoderCreate(sampleRateHz, bitrateKbps)
            if (handle != 0L) {
                Log.i(TAG, "编码器启动: sampleRate=$sampleRateHz bitrate=$bitrateKbps frameSize=$frameSize")
                true
            } else {
                Log.e(TAG, "编码器创建失败: return 0")
                false
            }
        } catch (e: Exception) {
            Log.e(TAG, "编码器创建异常", e)
            false
        }
    }

    @Synchronized
    fun encode(pcm: ShortArray): ByteArray? {
        val h = handle
        if (h == 0L) return null
        return try {
            val encoded = OpusNative.encoderEncode(h, pcm) ?: return null
            val seq = seqNum++
            val packet = Udp2MicProtocol.buildPacket(sampleRateId, seq, encoded, bitrateId)

            encodeCount++
            totalEncodedBytes += encoded.size
            totalPacketBytes += packet.size

            // 逐帧审计: 如果输入≠frameSize, 说明帧累积逻辑有问题
            if (pcm.size != frameSize) {
                Log.w(TAG, "⚠️ 帧大小异常! encode#${encodeCount}: pcm.size=${pcm.size} frameSize=$frameSize seq=$seq")
            }

            packet
        } catch (e: Exception) {
            Log.e(TAG, "编码异常 seq=${seqNum}", e)
            null
        }
    }

    @Synchronized
    fun stop() {
        val h = handle
        if (h != 0L) {
            try { OpusNative.encoderDestroy(h) } catch (_: Exception) {}
            handle = 0
        }
    }

    protected fun finalize() {
        stop()
    }
}
