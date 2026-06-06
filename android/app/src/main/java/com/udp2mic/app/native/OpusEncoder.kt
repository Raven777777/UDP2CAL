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
    vbrRaw: Int = Prefs.opusVbr,
    val fec: Int = Prefs.opusFec,
    val packetLoss: Int = Prefs.opusPacketLoss,
    vbrConstraintRaw: Int = Prefs.opusVbrConstraint,
) {
    // 统一强制逻辑：DTX=1 时 VBR 必须=1；VBR=0 时 vbrConstraint 无意义强制=0
    val vbr: Int = if (dtx == 1) 1 else vbrRaw
    val vbrConstraint: Int = if (vbr == 1) vbrConstraintRaw else 0
    companion object {
        private const val TAG = "OpusEncoder"

        /** 将 kbps 编码为协议头单字节：0→auto，1..255→(kbps/2)，支持 0–510kbps。
         * 注意：512 → 512/2=256 → coerce 到 255 → 解码为 510kbps（OPUS 协议硬上限） */
        private fun computeBitrateId(kbps: Int): Byte =
            if (kbps <= 0) Udp2MicProtocol.BITRATE_AUTO
            else (kbps / 2).coerceIn(1, 255).toByte()
    }

    @Volatile private var handle: Long = 0
    val sampleRateId: Byte = Udp2MicProtocol.hzToSampleRate(sampleRateHz)
    /** 协议头码率字段（Byte，0=auto，其它=(bitrateKbps/2).coerceIn(1,255) 支持 0–510kbps） */
    @Volatile var bitrateId: Byte = computeBitrateId(bitrateKbps)
    private var seqNum: Byte = 0
    var encodeCount: Long = 0; private set

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

    /**
     * 编码到预分配缓冲区（完全零分配版本）
     * JNI 层直接将 Opus 压缩数据写入 dest[offset+HEADER_SIZE]，
     * 然后由 writeHeader 原地写入 6 字节包头，无任何 ByteArray new。
     * @return 写入的字节数（HEADER_SIZE + payload），失败返回 -1
     */
    @Synchronized
    fun encodeTo(pcm: ShortArray, dest: ByteArray, offset: Int): Int {
        val h = handle; if (h == 0L) return -1
        return try {
            val payloadOffset = offset + Udp2MicProtocol.HEADER_SIZE
            val nbBytes = OpusNative.encoderEncodeTo(h, pcm, dest, payloadOffset)
            if (nbBytes < 0) return -1
            val written = Udp2MicProtocol.writeHeader(dest, offset, nbBytes, sampleRateId, seqNum++, bitrateId)
            encodeCount++
            written
        } catch (e: Exception) {
            Log.e(TAG, "EncodeTo failed", e)
            -1
        }
    }

    @Synchronized
    fun update(complexity: Int, signalType: Int, bandwidth: Int, dtx: Int, vbr: Int, bitrateKbps: Int, fec: Int, packetLoss: Int, vbrConstraint: Int): Boolean {
        val h = handle; if (h == 0L) return false
        return try {
            val ok = OpusNative.encoderUpdate(h, complexity, signalType, bandwidth, dtx, vbr, bitrateKbps, fec, packetLoss, vbrConstraint)
            if (ok) { bitrateId = computeBitrateId(bitrateKbps) }
            ok
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
