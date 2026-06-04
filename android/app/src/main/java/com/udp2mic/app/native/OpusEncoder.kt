package com.udp2mic.app.native

import android.util.Log
import com.udp2mic.app.Prefs
import com.udp2mic.app.Udp2MicProtocol

class OpusEncoder(
    val sampleRateHz: Int = 48000,
    val bitrateKbps: Int = 128,
    val complexity: Int = Prefs.opusComplexity,
    val signalType: Int = Prefs.opusSignal,
    val bandwidth: Int = Prefs.opusBandwidth,
    val dtx: Int = Prefs.opusDtx,
    val vbr: Int = Prefs.opusVbr,
) {
    companion object {
        private const val TAG = "OpusEncoder"
    }

    @Volatile private var handle: Long = 0
    val sampleRateId: Byte = Udp2MicProtocol.hzToSampleRate(sampleRateHz)
    val bitrateId: Byte = bitrateKbps.coerceIn(0, 255).toByte()
    private var seqNum: Byte = 0
    var encodeCount: Long = 0; private set

    val frameSize: Int
        get() = if (handle != 0L) OpusNative.encoderGetFrameSize(handle) else 0

    @Synchronized
    fun start(): Boolean {
        if (handle != 0L) return true
        return try {
            handle = OpusNative.encoderCreate(sampleRateHz, bitrateKbps, complexity, signalType, bandwidth, dtx, vbr)
            if (handle != 0L) {
                Log.i(TAG, "Encoder: sr=$sampleRateHz br=${bitrateKbps}k cplx=$complexity sig=$signalType bw=$bandwidth dtx=$dtx vbr=$vbr")
                true
            } else {
                Log.e(TAG, "Encoder create returned 0")
                false
            }
        } catch (e: Exception) {
            Log.e(TAG, "Encoder create failed", e)
            false
        }
    }

    @Synchronized
    fun encode(pcm: ShortArray): ByteArray? {
        val h = handle; if (h == 0L) return null
        return try {
            val encoded = OpusNative.encoderEncode(h, pcm) ?: return null
            val packet = Udp2MicProtocol.buildPacket(sampleRateId, seqNum++, encoded, bitrateId)
            encodeCount++
            packet
        } catch (e: Exception) {
            Log.e(TAG, "Encode failed", e)
            null
        }
    }

    @Synchronized
    fun update(complexity: Int, signalType: Int, bandwidth: Int, dtx: Int, vbr: Int, bitrateKbps: Int): Boolean {
        val h = handle; if (h == 0L) return false
        return try {
            OpusNative.encoderUpdate(h, complexity, signalType, bandwidth, dtx, vbr, bitrateKbps)
        } catch (e: Exception) {
            Log.e(TAG, "Encoder update failed", e)
            false
        }
    }

    @Synchronized
    fun stop() {
        val h = handle
        if (h != 0L) { try { OpusNative.encoderDestroy(h) } catch (_: Exception) {}; handle = 0 }
    }

    protected fun finalize() { stop() }
}
