package com.udp2cal.app.native

import android.util.Log
import com.udp2cal.app.EncoderCfg
import com.udp2cal.app.Udp2CalProtocol

/**
 * Opus 编码器（v3：固定 48 kHz / 10 ms / 单声道）
 *
 * 与 v2 的差别：**不再写任何私有包头**。`encodeTo` 只把 Opus 负载写进
 * `dest[payloadOffset...]`，RTP 头由调用方（ForwardLink）用
 * [Udp2CalProtocol.writeRtpHeader] 写在偏移 0..11。
 *
 * 负载上限 1275 字节（[Udp2CalProtocol.OPUS_MAX_PAYLOAD]）：
 * 超限的一帧由 JNI 直接丢弃，绝不截断。
 */
class OpusEncoder internal constructor(
    val sampleRateHz: Int = Udp2CalProtocol.OPUS_SAMPLE_RATE,
    cfg: EncoderCfg,
) {
    /**
     * 参数快照：DTX 强制 VBR、VBR 关闭则约束清零等推导由
     * [EncoderCfg.fromPrefs] 统一完成，这里只消费结果。
     */
    private val cfg: EncoderCfg = cfg

    companion object {
        private const val TAG = "OpusEncoder"
    }

    @Volatile private var handle: Long = 0

    val frameSize: Int
        get() = if (handle != 0L) OpusNative.encoderGetFrameSize(handle) else 0

    @Synchronized
    fun start(): Boolean {
        if (handle != 0L) return true
        return try {
            handle = OpusNative.encoderCreate(
                sampleRateHz,
                Udp2CalProtocol.clampBitrateKbps(cfg.bitrateKbps),
                cfg.complexity, cfg.signal, cfg.bandwidth,
                cfg.dtx, cfg.vbr, cfg.fec, cfg.packetLoss, cfg.vbrConstraint
            )
            if (handle != 0L) {
                Log.i(TAG, "Encoder: sr=$sampleRateHz br=${Udp2CalProtocol.clampBitrateKbps(cfg.bitrateKbps)}k cplx=${cfg.complexity} sig=${cfg.signal} bw=${cfg.bandwidth} dtx=${cfg.dtx} vbr=${cfg.vbr} fec=${cfg.fec} pl=${cfg.packetLoss} vbrc=${cfg.vbrConstraint}")
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

    /**
     * 编码一帧到 `dest[payloadOffset .. payloadOffset+n]`（零分配）。
     * @return 负载字节数；0 表示 DTX 空帧（不发包），-1 表示失败
     */
    @Synchronized
    fun encodeTo(pcm: ShortArray, dest: ByteArray, payloadOffset: Int): Int {
        val h = handle
        if (h == 0L) return -1
        return try {
            OpusNative.encoderEncodeTo(h, pcm, dest, payloadOffset)
        } catch (e: Exception) {
            Log.e(TAG, "EncodeTo failed", e)
            -1
        }
    }

    @Synchronized
    fun update(
        complexity: Int, signalType: Int, bandwidth: Int, dtx: Int, vbr: Int,
        bitrateKbps: Int, fec: Int, packetLoss: Int, vbrConstraint: Int
    ): Boolean {
        val h = handle
        if (h == 0L) return false
        return try {
            OpusNative.encoderUpdate(
                h, complexity, signalType, bandwidth, dtx, vbr,
                Udp2CalProtocol.clampBitrateKbps(bitrateKbps), fec, packetLoss, vbrConstraint
            )
        } catch (e: Exception) {
            Log.e(TAG, "Encoder update failed", e)
            false
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
}
