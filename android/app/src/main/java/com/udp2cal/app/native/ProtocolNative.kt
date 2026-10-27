package com.udp2cal.app.native

/**
 * 协议实现的 JNI 入口——**唯一的协议真源是 `protocol` crate 的 Rust 源文件**。
 *
 * v1.4.0 之前，`com.udp2cal.app.Udp2CalProtocol` 是 Rust 版的 Kotlin 手工移植副本，
 * 每次改协议都要改两处并人工比对。现在行为全部由 Rust 提供（Android 端
 * `libudp2cal_protocol.so`、桌面 JVM 单测 `udp2cal_protocol.dll`，同一套符号），
 * `Udp2CalProtocol` 只剩常量与 DTO 转换。
 *
 * 约定：
 * * 句柄是 `Long`，`0` 表示已释放；`close()` 之后不得再用。
 * * 数组类返回值用「整型数组按字段顺序展开」「`LongArray` 打包多返回值」表达，
 *   避免为每个 DTO 写一遍 JNI 对象构造。
 * * 返回 NaN 的 `Double` 表示「无值」（Kotlin 侧转成 `null`）。
 */
internal object ProtocolNative {

    init {
        System.loadLibrary("udp2cal_protocol")
        // 建立 Java System.nanoTime() ↔ Rust Instant 的基准；必须紧随 load 之后
        clockSync(System.nanoTime())
    }

    // ── 时钟桥 ────────────────────────────────────────────────────────────

    external fun clockSync(javaNanos: Long)

    // ── 常量（防两端漂移，见 Udp2CalProtocolTest） ────────────────────────

    external fun constants(): LongArray

    external fun constantsReal(): DoubleArray

    external fun waterTargetMs(): Int

    // ── 控制面 ────────────────────────────────────────────────────────────

    /** `[msgType, payloadLen, id0..id7]`，解析失败为 null */
    external fun decodeControlHeader(buf: ByteArray, len: Int): LongArray?

    /** 同 [decodeControlHeader]，但要求数据报长度精确匹配 */
    external fun parseControl(buf: ByteArray, len: Int): LongArray?

    external fun buildControl(msgType: Int, deviceId: ByteArray, payload: ByteArray): ByteArray?

    /** `[reversePort, forwardSsrc, payloadType, frameMs, complexity, signal, bandwidth, opusFlags, bitrateKbps, packetLoss]` */
    external fun connectDecode(buf: ByteArray, offset: Int, len: Int): IntArray?

    external fun connectEncode(
        reversePort: Int,
        forwardSsrc: Int,
        payloadType: Int,
        frameMs: Int,
        complexity: Int,
        signal: Int,
        bandwidth: Int,
        opusFlags: Int,
        bitrateKbps: Int,
        packetLoss: Int,
    ): ByteArray?

    /** `[reverseSsrc, payloadType, frameMs, forwardSsrc]` */
    external fun ackDecode(buf: ByteArray, offset: Int, len: Int): IntArray?

    external fun ackEncode(
        reverseSsrc: Int,
        payloadType: Int,
        frameMs: Int,
        forwardSsrc: Int,
    ): ByteArray?

    /** DISCOVER_REPLY 的监听端口；非法为 0 */
    external fun discoverPort(buf: ByteArray, offset: Int, len: Int): Int

    /** DISCOVER_REPLY 的设备名；非法为 null（空名已由 Rust 回落成默认名） */
    external fun discoverName(buf: ByteArray, offset: Int, len: Int): String?

    external fun clampBitrate(kbps: Int): Int

    external fun snapBitrate(kbps: Int): Int

    // ── RTP ──────────────────────────────────────────────────────────────

    /** `[marker, seq, timestamp, ssrc, payloadOffset, payloadLength]` */
    external fun parseRtp(buf: ByteArray, len: Int): LongArray?

    external fun writeRtpHeader(
        dest: ByteArray,
        offset: Int,
        marker: Boolean,
        seq: Int,
        timestamp: Int,
        ssrc: Int,
    ): Boolean

    external fun extendSeq(highest: Long, seq: Int): Long

    external fun extendTs(highest: Long, ts: Long): Long

    // ── JitterBuffer ─────────────────────────────────────────────────────

    external fun jbCreate(): Long

    external fun jbDestroy(h: Long)

    external fun jbReset(h: Long)

    external fun jbInsert(
        h: Long,
        seq: Int,
        ts: Long,
        marker: Boolean,
        buf: ByteArray,
        offset: Int,
        length: Int,
    ): Boolean

    /** 返回值即 [com.udp2cal.app.Udp2CalProtocol.Pull] 的判别值 */
    external fun jbPull(h: Long, ringMs: Int, pipeMs: Int, out: ByteArray): Int

    /** 取一帧用于阶跃裁剪；调用方必须解码后丢弃 PCM。 */
    external fun jbPullTrim(h: Long, out: ByteArray): Int

    /** 起播所需的连续媒体水位（毫秒）。 */
    external fun jbSetStartPrebufferMs(h: Long, ms: Int)

    external fun jbNextSeq(h: Long): Long

    external fun jbNextTs(h: Long): Long

    external fun jbHighestTs(h: Long): Long

    external fun jbStarted(h: Long): Boolean

    external fun jbPlaying(h: Long): Boolean

    external fun jbPlcRun(h: Long): Int

    external fun jbEpoch(h: Long): Long

    external fun jbLastRebaseReason(h: Long): String?

    external fun jbPulledLength(h: Long): Int

    external fun jbPulledTs(h: Long): Long

    external fun jbWaterMs(h: Long): Int

    external fun jbContiguousFrames(h: Long): Int

    external fun jbContiguousMs(h: Long): Int

    external fun jbOccupancyFrames(h: Long): Int

    external fun jbOccupancyMs(h: Long): Int

    external fun jbArrivalNanosAt(h: Long, ts: Long): Long

    external fun jbReceived(h: Long): Long

    external fun jbLate(h: Long): Long

    external fun jbDuplicate(h: Long): Long

    external fun jbResyncs(h: Long): Long

    external fun jbSeqRealign(h: Long): Long

    external fun jbOverflow(h: Long): Long

    external fun jbPlcFrames(h: Long): Long

    external fun jbMaxPlcRun(h: Long): Int

    external fun jbStepTrims(h: Long): Long

    // ── StepAbsorber ─────────────────────────────────────────────────────

    external fun saCreate(): Long

    external fun saDestroy(h: Long)

    external fun saReset(h: Long)

    /** 打包返回 `frames shl 32 or (limited ? 1 : 0)` */
    external fun saDecide(h: Long, jbWaterMs: Int, pipeMs: Int, canTrim: Boolean, targetMs: Int): Long

    /** 同 [saDecide]，显式传入 `System.nanoTime()`（单测推进虚拟时间用）。 */
    external fun saDecideAt(
        h: Long,
        jbWaterMs: Int,
        pipeMs: Int,
        canTrim: Boolean,
        targetMs: Int,
        nowNanos: Long,
    ): Long

    external fun saHits(h: Long): Long

    external fun saTrimsInWindow(h: Long): Int

    external fun saTakeLimitedLog(h: Long): Boolean

    // ── RateEstimator ────────────────────────────────────────────────────

    external fun reCreate(): Long

    external fun reDestroy(h: Long)

    external fun reReset(h: Long)

    external fun reObserve(h: Long, ts: Long, arrivedNanos: Long)

    /** 无估计时返回 NaN */
    external fun reFeedforward(h: Long): Double

    external fun reSigmaPpm(h: Long): Double

    external fun reUsedBuckets(h: Long): Int

    external fun reObservations(h: Long): Long

    // ── DeviceRateEstimator（声卡时钟前馈） ─────────────────────────────

    external fun deCreate(): Long

    external fun deDestroy(h: Long)

    external fun deReset(h: Long)

    external fun deObserve(h: Long, deviceFrames: Long, playedNanos: Long)

    /** 无估计时返回 NaN */
    external fun deFeedforward(h: Long): Double

    external fun deSigmaPpm(h: Long): Double

    external fun deBuckets(h: Long): Int

    // ── DriftController ──────────────────────────────────────────────────

    external fun dcCreate(deviceRate: Int, targetMs: Int): Long

    external fun dcDestroy(h: Long)

    external fun dcReset(h: Long)

    external fun dcUpdate(h: Long, waterMs: Int, deviceFrames: Long): Double

    external fun dcAdjust(h: Long): Double

    external fun dcWater(h: Long): Double

    external fun dcIntegral(h: Long): Double

    external fun dcTargetMs(h: Long): Int

    external fun dcRatioUpdates(h: Long): Long
}
