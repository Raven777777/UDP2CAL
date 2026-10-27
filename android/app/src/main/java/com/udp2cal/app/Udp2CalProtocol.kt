package com.udp2cal.app

import com.udp2cal.app.native.ProtocolNative
import java.security.SecureRandom

/**
 * UDP2CAL 协议 v3 的 Kotlin 门面。
 *
 * **行为全部在 Rust 里**（`protocol/protocol.rs` + `protocol/rtp.rs`，经由
 * [ProtocolNative] 的 JNI 绑定）。本文件只剩三类东西：
 *
 * 1. 常量（`const val`：编译期需要，且被 Kotlin 代码直接引用）；
 * 2. DTO 数据类（`ControlHeader` / `ConnectPayload` / …）与原生 `LongArray`/
 *    `IntArray` 的互转；
 * 3. 纯 Kotlin 的小工具（`generateDeviceId` 用 `SecureRandom`、`deviceIdToString`
 *    做十六进制格式化）。
 *
 * 判断"这段逻辑该不该放 Kotlin"的唯一标准：**改了它会不会让 Windows 端行为不一致？**
 * 会 → 放 Rust。不会（格式化、随机数、UI 方便用的 DTO）→ 留这里。
 *
 * 常量与 Rust 的一致性由 `Udp2CalProtocolTest.nativeConstantsMatchKotlinConstants` /
 * `nativeRealConstantsMatchKotlinConstants` 逐项守住，单边改动会立刻失败。
 */
object Udp2CalProtocol {

    // ═══════════════════════ 控制面常量 ═══════════════════════

    const val CONTROL_VERSION: Byte = 3
    const val CONTROL_HEADER_SIZE = 15
    const val CONTROL_MAX_PAYLOAD = 512
    const val DEVICE_ID_SIZE = 8
    const val DISCOVER_PORT = 44043
    const val DEFAULT_LISTEN_PORT = 44044

    const val TYPE_DATA: Byte = 0
    const val TYPE_CONNECT: Byte = 1
    const val TYPE_DISCOVER_REQ: Byte = 2
    const val TYPE_DISCOVER_REPLY: Byte = 3
    const val TYPE_CONNECT_ACK: Byte = 4
    const val TYPE_MAX_KNOWN: Byte = 4

    const val CONNECT_PAYLOAD_SIZE = 15
    const val CONNECT_ACK_PAYLOAD_SIZE = 10
    const val DEVICE_NAME_MAX = 254

    const val OPUS_FLAG_VBR = 0x01
    const val OPUS_FLAG_DTX = 0x02
    const val OPUS_FLAG_VBR_CONSTRAINT = 0x04
    const val OPUS_FLAG_FEC = 0x08

    // ═══════════════════════ 码率 ═══════════════════════

    const val BITRATE_AUTO_KBPS = 512
    const val BITRATE_MAX_KBPS = 512
    val BITRATE_STEPS_KBPS = intArrayOf(64, 128, 256, 384, 512)

    // ═══════════════════════ RTP / 抖动缓冲常量 ═══════════════════════
    //
    // 全部与 `protocol/rtp.rs` 同值；改动必须两边同步（测试会守住）。

    const val RTP_HEADER_SIZE = 12
    const val OPUS_PAYLOAD_TYPE = 111
    const val OPUS_SAMPLE_RATE = 48000
    const val OPUS_FRAME_MS = 10
    const val OPUS_FRAME_SAMPLES = 480
    const val OPUS_MAX_PAYLOAD = 1275
    const val RTP_MAX_PACKET = RTP_HEADER_SIZE + OPUS_MAX_PAYLOAD
    const val UDP_IP_MAX_PACKET = RTP_MAX_PACKET + 8 + 20

    const val MAX_REORDER_PACKETS = 64
    const val ARRIVAL_TRACE = 128

    const val JITTER_START_MS = 30

    /**
     * 总延迟目标（毫秒）。组成：PCM 环 ~44 + 声卡 ~10 + 抖动缓冲余量。
     *
     * 这个值**由网络环境决定**，不是固定值：v1.3.0 用 260 ms 是因为手机全频段
     * 扫描（每次 1.85 s 静默）会让链路停摆 600–1100 ms；关掉扫描后 150 ms 就够。
     * 桌面端可用 `UDP2CAL_WATER_TARGET_MS` 环境变量覆盖；Android 没有环境变量，
     * 但 [DriftController] 保留了可注入的 `targetMs` 通道。
     */
    const val WATER_TARGET_MS = 150
    const val WATER_TARGET_MIN_MS = 80
    const val WATER_TARGET_MAX_MS = 600

    const val DECODE_LIMIT_MS = 40
    const val PLC_LEAD_MS = 20
    const val PLC_MAX_MS = 120
    const val RESYNC_GAP_MS = 200

    /** 仅用于诊断：单 tick 涨幅不再直接触发裁剪。 */
    const val STEP_TRIM_GROWTH_MS = 80
    const val STEP_TRIM_EXCESS_MS = 100
    /** 总水位必须**持续**超限这么久，才证明是真实积压而非一次网络补齐。 */
    const val STEP_TRIM_HOLD_MS = 1500
    const val STEP_TRIM_MAX_PER_MIN = 3

    const val RESAMPLE_MAX_ADJUST = 0.005
    const val RATIO_SLEW_STEP = 0.0001
    const val PI_KP_PER_MS = 0.00002
    const val PI_TI_S = 180.0
    const val PI_INTEGRAL_MAX = 0.001
    const val PI_MIN_UPDATE_MS = 5.0
    /** 水位低通时间常数：只让“长期残余”进入反馈，不追每次网络突发。 */
    const val WATER_SMOOTH_TAU_MS = 4000.0
    const val RATE_WINDOW_S = 30.0
    const val RATE_BUCKETS = 30
    const val RATE_MIN_BUCKETS = 6
    const val RATE_JUMP_MAX_SAMPLES = 96000
    const val RATE_FEEDFORWARD_MAX = 0.02

    // ═══════════════════════ 控制包头 ═══════════════════════

    class ControlHeader(val msgType: Int, val payloadLen: Int, val deviceId: ByteArray) {
        val isKnownType: Boolean get() = msgType in TYPE_CONNECT.toInt()..TYPE_MAX_KNOWN.toInt()
    }

    private fun headerFrom(arr: LongArray): ControlHeader {
        val id = ByteArray(DEVICE_ID_SIZE) { arr[2 + it].toByte() }
        return ControlHeader(arr[0].toInt(), arr[1].toInt(), id)
    }

    fun decodeControlHeader(buf: ByteArray, len: Int): ControlHeader? =
        ProtocolNative.decodeControlHeader(buf, len)?.let { headerFrom(it) }

    fun parseControl(buf: ByteArray, len: Int): ControlHeader? =
        ProtocolNative.parseControl(buf, len)?.let { headerFrom(it) }

    fun buildControl(msgType: Byte, deviceId: ByteArray, payload: ByteArray): ByteArray =
        ProtocolNative.buildControl(msgType.toInt(), deviceId, payload) ?: ByteArray(0)

    // ═══════════════════════ CONNECT（类型 1） ═══════════════════════

    class ConnectPayload(
        val reversePort: Int,
        val forwardSsrc: Int,
        val payloadType: Int = OPUS_PAYLOAD_TYPE,
        val frameMs: Int = OPUS_FRAME_MS,
        val complexity: Int = 0,
        val signal: Int = 0,
        val bandwidth: Int = 4,
        val opusFlags: Int = OPUS_FLAG_VBR,
        val bitrateKbps: Int = 0,
        val packetLoss: Int = 0,
    ) {
        fun encode(): ByteArray =
            ProtocolNative.connectEncode(
                reversePort,
                forwardSsrc,
                payloadType,
                frameMs,
                complexity,
                signal,
                bandwidth,
                opusFlags,
                bitrateKbps,
                packetLoss,
            ) ?: ByteArray(CONNECT_PAYLOAD_SIZE)

        fun isValid(): Boolean =
            payloadType == OPUS_PAYLOAD_TYPE && frameMs == OPUS_FRAME_MS && forwardSsrc != 0

        val vbr: Boolean get() = opusFlags and OPUS_FLAG_VBR != 0
        val dtx: Boolean get() = opusFlags and OPUS_FLAG_DTX != 0
        val vbrConstraint: Boolean get() = opusFlags and OPUS_FLAG_VBR_CONSTRAINT != 0
        val fec: Boolean get() = opusFlags and OPUS_FLAG_FEC != 0

        /** 生效码率：`0`=自动→上限，超过上限钳制 */
        fun effectiveBitrateKbps(): Int = clampBitrateKbps(bitrateKbps)

        companion object {
            fun decode(buf: ByteArray, offset: Int, len: Int): ConnectPayload? {
                val a = ProtocolNative.connectDecode(buf, offset, len) ?: return null
                return ConnectPayload(
                    reversePort = a[0],
                    forwardSsrc = a[1],
                    payloadType = a[2],
                    frameMs = a[3],
                    complexity = a[4],
                    signal = a[5],
                    bandwidth = a[6],
                    opusFlags = a[7],
                    bitrateKbps = a[8],
                    packetLoss = a[9],
                )
            }
        }
    }

    // ═══════════════════════ CONNECT_ACK（类型 4） ═══════════════════════

    class ConnectAckPayload(
        val reverseSsrc: Int,
        val payloadType: Int,
        val frameMs: Int,
        val forwardSsrc: Int,
    ) {
        fun isValid(): Boolean = payloadType == OPUS_PAYLOAD_TYPE && frameMs == OPUS_FRAME_MS

        fun encode(): ByteArray =
            ProtocolNative.ackEncode(reverseSsrc, payloadType, frameMs, forwardSsrc)
                ?: ByteArray(CONNECT_ACK_PAYLOAD_SIZE)

        companion object {
            fun decode(buf: ByteArray, offset: Int, len: Int): ConnectAckPayload? {
                val a = ProtocolNative.ackDecode(buf, offset, len) ?: return null
                if (a.size != 4) return null
                return ConnectAckPayload(a[0], a[1], a[2], a[3])
            }
        }
    }

    // ═══════════════════════ 码率归一 ═══════════════════════

    /** `<=0` → 自动（上限），`>512` → 上限。 */
    fun clampBitrateKbps(kbps: Int): Int = ProtocolNative.clampBitrate(kbps)

    /** 吸附到最近的手动档位；`0`（自动）原样返回，等距时取较小档。 */
    fun snapBitrateKbps(kbps: Int): Int = ProtocolNative.snapBitrate(kbps)

    // ═══════════════════════ DISCOVER（类型 2/3） ═══════════════════════

    class DiscoverReply(val listenPort: Int, val deviceName: String) {
        companion object {
            fun decode(buf: ByteArray, offset: Int, len: Int): DiscoverReply? {
                val port = ProtocolNative.discoverPort(buf, offset, len)
                if (port <= 0) return null
                val name = ProtocolNative.discoverName(buf, offset, len) ?: return null
                return DiscoverReply(port, name)
            }
        }
    }

    // ═══════════════════════ RTP ═══════════════════════

    class RtpPacket(
        val marker: Boolean,
        val seq: Int,
        val timestamp: Long,
        val ssrc: Int,
        val payloadOffset: Int,
        val payloadLength: Int,
    )

    fun writeRtpHeader(
        dest: ByteArray,
        offset: Int,
        marker: Boolean,
        seq: Int,
        timestamp: Int,
        ssrc: Int,
    ) {
        ProtocolNative.writeRtpHeader(dest, offset, marker, seq, timestamp, ssrc)
    }

    fun parseRtp(buf: ByteArray, len: Int): RtpPacket? {
        val a = ProtocolNative.parseRtp(buf, len) ?: return null
        return RtpPacket(
            marker = a[0] != 0L,
            seq = a[1].toInt(),
            timestamp = a[2],
            ssrc = a[3].toInt(),
            payloadOffset = a[4].toInt(),
            payloadLength = a[5].toInt(),
        )
    }

    fun extendSeq(highest: Long, seq: Int): Long = ProtocolNative.extendSeq(highest, seq)

    fun extendTs(highest: Long, ts: Long): Long = ProtocolNative.extendTs(highest, ts)

    // ═══════════════════════ 设备 ID（纯 Kotlin） ═══════════════════════

    fun generateDeviceId(): ByteArray {
        val id = ByteArray(DEVICE_ID_SIZE)
        SecureRandom().nextBytes(id)
        if (id.all { it == 0.toByte() }) id[0] = 1
        return id
    }

    fun deviceIdToString(id: ByteArray): String {
        if (id.size < DEVICE_ID_SIZE) return "invalid"
        val sb = StringBuilder(DEVICE_ID_SIZE * 2)
        for (i in 0 until DEVICE_ID_SIZE) sb.append(String.format("%02x", id[i]))
        return sb.toString()
    }

    // ═══════════════════════ 抖动缓冲（Rust 实现） ═══════════════════════

    /** [JitterBuffer.pull] 的返回值。 */
    object Pull {
        const val WAIT = 0
        const val PACKET = 1
        const val PLC = 2
        const val SILENCE = 3
    }

    /**
     * 重排序 / 去抖缓冲。
     *
     * 状态机、水位计算、PLC 与重同步逻辑全在 `protocol/rtp.rs`；这里只是
     * 句柄 + 属性读取。`insert` 在接收线程、`pull` 在播放线程，Rust 侧用互斥锁
     * 串行化（纯 Kotlin 版本是数据竞争，锁上之后语义更严格）。
     */
    class JitterBuffer : AutoCloseable {
        private var handle: Long = ProtocolNative.jbCreate()

        val nextSeq: Long get() = ProtocolNative.jbNextSeq(handle)
        val nextTs: Long get() = ProtocolNative.jbNextTs(handle)
        fun highestTsValue(): Long = ProtocolNative.jbHighestTs(handle)
        val started: Boolean get() = ProtocolNative.jbStarted(handle)
        val playing: Boolean get() = ProtocolNative.jbPlaying(handle)
        val plcRun: Int get() = ProtocolNative.jbPlcRun(handle)
        val epoch: Long get() = ProtocolNative.jbEpoch(handle)
        val lastRebaseReason: String get() = ProtocolNative.jbLastRebaseReason(handle) ?: ""
        val pulledLength: Int get() = ProtocolNative.jbPulledLength(handle)
        val pulledTs: Long get() = ProtocolNative.jbPulledTs(handle)
        val received: Long get() = ProtocolNative.jbReceived(handle)
        val late: Long get() = ProtocolNative.jbLate(handle)
        val duplicate: Long get() = ProtocolNative.jbDuplicate(handle)
        val resyncs: Long get() = ProtocolNative.jbResyncs(handle)
        val seqRealign: Long get() = ProtocolNative.jbSeqRealign(handle)
        val overflow: Long get() = ProtocolNative.jbOverflow(handle)
        val plcFrames: Long get() = ProtocolNative.jbPlcFrames(handle)
        val maxPlcRun: Int get() = ProtocolNative.jbMaxPlcRun(handle)
        val stepTrims: Long get() = ProtocolNative.jbStepTrims(handle)

        fun reset() {
            ProtocolNative.jbReset(handle)
        }

        /** 起播所需的连续媒体水位；默认 30 ms，可提到目标水位避免起播后追。 */
        fun setStartPrebufferMs(ms: Int) {
            ProtocolNative.jbSetStartPrebufferMs(handle, ms)
        }

        /** 该时间戳样本的到达时刻（`System.nanoTime()` 口径）；查不到返回 -1。 */
        fun arrivalNanosAt(ts: Long): Long = ProtocolNative.jbArrivalNanosAt(handle, ts)

        /** 抖动缓冲里还有多少毫秒可播。 */
        fun waterMs(): Int = ProtocolNative.jbWaterMs(handle)

        /** 从 `nextTs` 起连续有多少帧（用于起播判定）。 */
        fun contiguousFrames(): Int = ProtocolNative.jbContiguousFrames(handle)

        /** 从 `nextTs` 起连续可解码的毫秒数。 */
        fun contiguousMs(): Int = ProtocolNative.jbContiguousMs(handle)

        /** 实际占用的帧数（不含空洞）。 */
        fun occupancyFrames(): Int = ProtocolNative.jbOccupancyFrames(handle)

        /** 实际占用的毫秒数（不含空洞）——比 waterMs 更接近“能播多久”。 */
        fun occupancyMs(): Int = ProtocolNative.jbOccupancyMs(handle)

        fun insert(seq: Int, ts: Long, marker: Boolean, buf: ByteArray, offset: Int, length: Int): Boolean =
            ProtocolNative.jbInsert(handle, seq, ts, marker, buf, offset, length)

        /**
         * 取一帧到 [out]（返回 [Pull.PACKET] 时有效），或给出 PLC/静音/等待。
         * [ringMs] 是 PCM 环占用、[pipeMs] 是"环 + 声卡"的播放管线延迟。
         */
        fun pull(ringMs: Int, pipeMs: Int, out: ByteArray): Int =
            ProtocolNative.jbPull(handle, ringMs, pipeMs, out)

        /**
         * 阶跃裁剪：按播放顺序取一帧。调用方必须把它交给解码器并丢弃 PCM，
         * 以保持有状态 Opus 解码器与发送端同步（不能只推进缓冲游标）。
         */
        fun pullTrim(out: ByteArray): Int = ProtocolNative.jbPullTrim(handle, out)

        fun destroyed(): Boolean = handle == 0L

        override fun close() {
            if (handle != 0L) {
                ProtocolNative.jbDestroy(handle)
                handle = 0L
            }
        }

        @Suppress("deprecation")
        protected fun finalize() {
            close()
        }
    }

    // ═══════════════════════ 阶跃吸收（方案 C，Rust 实现） ═══════════════════════

    class StepDecision(val frames: Int, val limited: Boolean) {
        companion object {
            val NONE = StepDecision(0, false)
        }
    }

    /**
     * 一次性积压（Wi-Fi 重传、调度抖动）的整帧吸收器。
     *
     * 判据：总水位超过目标 + [STEP_TRIM_EXCESS_MS] 且**持续** [STEP_TRIM_HOLD_MS]，
     * 单次涨幅 [STEP_TRIM_GROWTH_MS] 只做诊断；每分钟最多 [STEP_TRIM_MAX_PER_MIN] 次。
     */
    class StepAbsorber : AutoCloseable {
        private var handle: Long = ProtocolNative.saCreate()

        val hits: Long get() = ProtocolNative.saHits(handle)

        fun reset() {
            ProtocolNative.saReset(handle)
        }

        /** 当前 60 s 窗口内已执行的裁剪次数。 */
        fun trimsInWindow(): Int = ProtocolNative.saTrimsInWindow(handle)

        /** 限额日志节流：每个 60 s 窗口只放行一次。 */
        fun takeLimitedLog(): Boolean = ProtocolNative.saTakeLimitedLog(handle)

        fun decide(jbWaterMs: Int, pipeMs: Int, canTrim: Boolean, targetMs: Int): StepDecision {
            val packed = ProtocolNative.saDecide(handle, jbWaterMs, pipeMs, canTrim, targetMs)
            val frames = (packed ushr 32).toInt()
            val limited = (packed and 1L) != 0L
            return if (frames == 0 && !limited) StepDecision.NONE else StepDecision(frames, limited)
        }

        /** 单测用：显式传入 `System.nanoTime()` 时刻。 */
        internal fun decideAt(
            jbWaterMs: Int,
            pipeMs: Int,
            canTrim: Boolean,
            targetMs: Int,
            nowNanos: Long,
        ): StepDecision {
            val packed = ProtocolNative.saDecideAt(
                handle, jbWaterMs, pipeMs, canTrim, targetMs, nowNanos
            )
            val frames = (packed ushr 32).toInt()
            val limited = (packed and 1L) != 0L
            return if (frames == 0 && !limited) StepDecision.NONE else StepDecision(frames, limited)
        }

        override fun close() {
            if (handle != 0L) {
                ProtocolNative.saDestroy(handle)
                handle = 0L
            }
        }

        @Suppress("deprecation")
        protected fun finalize() {
            close()
        }
    }

    // ═══════════════════════ 速率前馈估计（Rust 实现） ═══════════════════════

    /**
     * 用 (RTP 时间戳, 本地到达时刻) 做最小延迟滤波 + 最小二乘拟合，
     * 得到链路速率的"前馈残差"（无量纲，≈ 时钟偏差比例）。
     */
    class RateEstimator : AutoCloseable {
        private var handle: Long = ProtocolNative.reCreate()

        val observations: Long get() = ProtocolNative.reObservations(handle)

        fun reset() {
            ProtocolNative.reReset(handle)
        }

        fun feedforward(): Double? =
            ProtocolNative.reFeedforward(handle).let { if (it.isNaN()) null else it }

        fun feedforwardPpm(): Double? = feedforward()?.times(1e6)

        fun sigmaPpm(): Double = ProtocolNative.reSigmaPpm(handle)

        fun usedBuckets(): Int = ProtocolNative.reUsedBuckets(handle)

        fun observe(ts: Long, arrivedNanos: Long) {
            ProtocolNative.reObserve(handle, ts, arrivedNanos)
        }

        override fun close() {
            if (handle != 0L) {
                ProtocolNative.reDestroy(handle)
                handle = 0L
            }
        }

        @Suppress("deprecation")
        protected fun finalize() {
            close()
        }
    }

    // ═══════════════════════ 设备时钟前馈（Rust 实现） ═══════════════════════

    /**
     * 用（声卡播放帧数, `System.nanoTime()`）估计设备时钟相对单调时钟的速率。
     * 这是唯一允许进入重采样比率的前馈：网络到达抖动不参与。
     */
    class DeviceRateEstimator : AutoCloseable {
        private var handle: Long = ProtocolNative.deCreate()

        fun reset() {
            ProtocolNative.deReset(handle)
        }

        fun observe(deviceFrames: Long, playedNanos: Long) {
            ProtocolNative.deObserve(handle, deviceFrames, playedNanos)
        }

        fun feedforward(): Double? =
            ProtocolNative.deFeedforward(handle).let { if (it.isNaN()) null else it }

        fun feedforwardPpm(): Double? = feedforward()?.times(1e6)

        fun sigmaPpm(): Double = ProtocolNative.deSigmaPpm(handle)

        fun buckets(): Int = ProtocolNative.deBuckets(handle)

        override fun close() {
            if (handle != 0L) {
                ProtocolNative.deDestroy(handle)
                handle = 0L
            }
        }

        @Suppress("deprecation")
        protected fun finalize() {
            close()
        }
    }

    // ═══════════════════════ 漂移控制（Rust 实现） ═══════════════════════

    /**
     * 抗饱和 PI：把总水位钉在 [targetMs] 上，输出重采样比率修正。
     *
     * 只处理长期残余（4 s 低通），不再追每次网络突发；`adjust` 直接喂给
     * [StreamResampler]，`±RESAMPLE_MAX_ADJUST` 是硬上限。
     */
    class DriftController(
        private val deviceRate: Int = OPUS_SAMPLE_RATE,
        targetMs: Int = WATER_TARGET_MS,
    ) : AutoCloseable {
        private var handle: Long = ProtocolNative.dcCreate(deviceRate, targetMs)

        val ratioUpdates: Long get() = ProtocolNative.dcRatioUpdates(handle)

        fun reset() {
            ProtocolNative.dcReset(handle)
        }

        fun adjust(): Double = ProtocolNative.dcAdjust(handle)

        fun targetMs(): Int = ProtocolNative.dcTargetMs(handle)

        fun water(): Double = ProtocolNative.dcWater(handle)

        fun integral(): Double = ProtocolNative.dcIntegral(handle)

        fun update(waterMs: Int, deviceFrames: Long): Double =
            ProtocolNative.dcUpdate(handle, waterMs, deviceFrames)

        override fun close() {
            if (handle != 0L) {
                ProtocolNative.dcDestroy(handle)
                handle = 0L
            }
        }

        @Suppress("deprecation")
        protected fun finalize() {
            close()
        }
    }
}
