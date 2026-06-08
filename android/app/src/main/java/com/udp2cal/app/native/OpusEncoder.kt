package com.udp2cal.app.native

import android.util.Log
import com.udp2cal.app.Prefs
import com.udp2cal.app.Udp2CalProtocol

class OpusEncoder(
    val sampleRateHz: Int = 48000,
    val bitrateKbps: Int = 128,
    val complexity: Int = Prefs.opusComplexity,
    val signalType: Int = Prefs.opusSignal,
    val bandwidth: Int = Prefs.opusBandwidth,
    val dtx: Int = Prefs.opusDtx,
    vbrRaw: Int = Prefs.opusVbr,
    val fec: Int = Prefs.opusFec,
    val packetLoss: Int = Prefs.opusPacketLoss,
    vbrConstraintRaw: Int = Prefs.opusVbrConstraint,
) {
    val vbr: Int = if (dtx == 1) 1 else vbrRaw
    val vbrConstraint: Int = if (vbr == 1) vbrConstraintRaw else 0
    companion object {
        private const val TAG = "OpusEncoder"
        private fun computeBitrateId(kbps: Int): Byte =
            if (kbps <= 0) Udp2CalProtocol.BITRATE_AUTO
            else (kbps / 2).coerceIn(1, 255).toByte()
    }

    @Volatile private var handle: Long = 0
    val sampleRateId: Byte = Udp2CalProtocol.hzToSampleRate(sampleRateHz)
    @Volatile var bitrateId: Byte = computeBitrateId(bitrateKbps)
    private var seqNum: Byte = 0
    var encodeCount: Long = 0; private set
    var deviceId: ByteArray = ByteArray(Udp2CalProtocol.DEVICE_ID_SIZE)

    val frameSize: Int
        get() = if (handle != 0L) OpusNative.encoderGetFrameSize(handle) else 0

    @Synchronized
    fun start(): Boolean {
        if (handle != 0L) return true
        return try {
            handle = OpusNative.encoderCreate(sampleRateHz, bitrateKbps, complexity, signalType, bandwidth, dtx, vbr, fec, packetLoss, vbrConstraint)
            if (handle != 0L) {
                Log.i(TAG, "Encoder: sr=$sampleRateHz br=${bitrateKbps}k cplx=$complexity sig=$signalType bw=$bandwidth dtx=$dtx vbr=$vbr fec=$fec pl=$packetLoss vbrc=$vbrConstraint")
                true
            } else {
                Log.e(TAG, "Encoder create returned 0"); false
            }
        } catch (e: Exception) { Log.e(TAG, "Encoder create failed", e); false }
    }

    @Synchronized
    fun encode(pcm: ShortArray): ByteArray? {
        val h = handle; if (h == 0L) return null
        return try {
            val encoded = OpusNative.encoderEncode(h, pcm) ?: return null
            val packet = Udp2CalProtocol.buildPacket(
                isAudio = true,
                msgType = Udp2CalProtocol.TYPE_DATA,
                sampleRate = sampleRateId,
                seqNum = seqNum++,
                deviceId = deviceId,
                payload = encoded,
                bitrate = bitrateId
            )
            encodeCount++
            packet
        } catch (e: Exception) { Log.e(TAG, "Encode failed", e); null }
    }

    /**
     * 编码到预分配缓冲区（完全零分配版本）
     * JNI 层直接将 Opus 压缩数据写入 dest[offset+HEADER_SIZE]，
     * 然后由 writeAudioHeader 原地写入 15 字节统一协议头。
     */
    @Synchronized
    fun encodeTo(pcm: ShortArray, dest: ByteArray, offset: Int): Int {
        val h = handle; if (h == 0L) return -1
        return try {
            val payloadOffset = offset + Udp2CalProtocol.HEADER_SIZE
            val nbBytes = OpusNative.encoderEncodeTo(h, pcm, dest, payloadOffset)
            if (nbBytes < 0) return -1
            val written = Udp2CalProtocol.writeAudioHeader(dest, offset, nbBytes, sampleRateId, seqNum++, deviceId, bitrateId)
            encodeCount++
            written
        } catch (e: Exception) { Log.e(TAG, "EncodeTo failed", e); -1 }
    }

    @Synchronized
    fun update(complexity: Int, signalType: Int, bandwidth: Int, dtx: Int, vbr: Int, bitrateKbps: Int, fec: Int, packetLoss: Int, vbrConstraint: Int): Boolean {
        val h = handle; if (h == 0L) return false
        return try {
            val ok = OpusNative.encoderUpdate(h, complexity, signalType, bandwidth, dtx, vbr, bitrateKbps, fec, packetLoss, vbrConstraint)
            if (ok) bitrateId = computeBitrateId(bitrateKbps)
            ok
        } catch (e: Exception) { Log.e(TAG, "Encoder update failed", e); false }
    }

    @Synchronized
    fun stop() {
        val h = handle
        if (h != 0L) { try { OpusNative.encoderDestroy(h) } catch (_: Exception) {}; handle = 0 }
    }

    protected fun finalize() { stop() }
}
