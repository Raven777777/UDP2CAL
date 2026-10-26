package com.udp2cal.app

/**
 * UDP2CAL 协议 v3（Kotlin 镜像实现）
 *
 * 与 Rust 端 `protocol/protocol.rs` + `protocol/rtp.rs` **字节级对齐**：
 * 控制包固定 15 字节头，媒体走标准 RTP（PT=111，48 kHz，固定 10 ms）。
 *
 * 控制头：
 * ```
 * Byte 0:       0x03                     控制协议版本
 * Byte 1:       0x00                     保留
 * Byte 2:       msg_type
 * Byte 3-4:     payload_len (u16 BE)
 * Byte 5-6:     0x00                     保留
 * Byte 7-14:    device_id[8]
 * Byte 15+:     payload
 * ```
 *
 * 控制包与 RTP 共用一个 socket，接收端必须先看首字节：
 * `0x80..0xBF` → RTP，`0x03` → 控制包，其余丢弃。
 */
object Udp2CalProtocol {
    // ═══════ 控制协议 ═══════
    const val CONTROL_VERSION: Byte = 3
    const val CONTROL_HEADER_SIZE = 15
    const val CONTROL_MAX_PAYLOAD = 512
    const val DEVICE_ID_SIZE = 8
    const val DISCOVER_PORT = 44043
    const val DEFAULT_LISTEN_PORT = 44044

    const val TYPE_DATA: Byte = 0          // v3 废弃：媒体不走控制类型
    const val TYPE_CONNECT: Byte = 1
    const val TYPE_DISCOVER_REQ: Byte = 2
    const val TYPE_DISCOVER_REPLY: Byte = 3
    const val TYPE_CONNECT_ACK: Byte = 4
    const val TYPE_MAX_KNOWN = 4

    const val CONNECT_PAYLOAD_SIZE = 15
    const val CONNECT_ACK_PAYLOAD_SIZE = 6
    const val DEVICE_NAME_MAX = 254

    // ═══════ RTP ═══════
    const val RTP_HEADER_SIZE = 12
    const val OPUS_PAYLOAD_TYPE = 111
    const val OPUS_SAMPLE_RATE = 48000
    const val OPUS_FRAME_MS = 10
    const val OPUS_FRAME_SAMPLES = 480
    const val OPUS_MAX_PAYLOAD = 1275
    const val RTP_MAX_PACKET = RTP_HEADER_SIZE + OPUS_MAX_PAYLOAD
    const val UDP_IP_MAX_PACKET = RTP_MAX_PACKET + 8 + 20

    // ═══════ 码率 ═══════
    /**
     * 线上码率 `0` 的映射值（跨版本兼容）。
     *
     * 本版客户端**不再发送 0**：设置页只有具体档位，自动档已移除。
     * 保留它只为兼容旧客户端/旧配置——收到 0 时按"用上限"处理。
     * 因此该值必须恒等于 [BITRATE_MAX_KBPS]。
     */
    const val BITRATE_AUTO_KBPS = 512
    /** 码率硬上限。两端必须同值：接收端用它钳制对端下发的值 */
    const val BITRATE_MAX_KBPS = 512
    /** 手动档位（UI 滑块的五档），64 / 128 / 256 / 384 / 512 */
    val BITRATE_STEPS_KBPS = intArrayOf(64, 128, 256, 384, 512)

    // ═══════ 抖动缓冲参数（与 Rust rtp.rs 一致）═══════
    const val MAX_REORDER_PACKETS = 64
    /** 到达时刻追踪深度：128 × 10 ms ≈ 1.28 s，远大于任何可用水位 */
    const val ARRIVAL_TRACE = 128
    /** 预缓冲启动阈值（3 帧 = 30 ms）。去掉离散丢/插后可下探到 30 ms */
    const val JITTER_START_MS = 30
    /**
     * 目标总水位（16.5 帧）——**单一口径**：抖动缓冲 + PCM 环 + 声卡缓冲。
     *
     * 旧实现把声卡缓冲排除在外，实际总水位因此被悄悄抬到 70–80 ms，
     * "目标 60 ms"这句话失去意义。现在三者同口径相加，这个数就是
     * "从可听点往后还压着多少音频"，也是控制器唯一的输入。
     *
     * 为什么是 165 而不是 70：实机（Wi-Fi + Android 发送端）测到包成批交付——
     * 单次到位 ≥6 帧的突发 229 次，中位 9 帧（90 ms），约每 1.2 s 一次，
     * 而长期到达率刚好 100.00 帧/s。排掉 90 ms 富余只靠比率（±0.5% = 5 ms/s）
     * 要 18 s，所以水位会在 120–200 ms 游走：目标设 70 时 PI 永久顶满 +0.5%
     * （实测中位 +5006 ppm），成了"常年变调 + 慢锯"而不是闭环。
     * 165 ≈ 实测可达稳态（解码环 44 + 声卡 10 + 抖动缓冲 ~110）。
     */
    const val WATER_TARGET_MS = 260
    /** 解码缓冲深度上限：超过就暂停生产（生产节奏必须由声卡消费节奏约束） */
    const val DECODE_LIMIT_MS = 40
    const val PLC_LEAD_MS = 20
    const val PLC_MAX_MS = 120
    const val RESYNC_GAP_MS = 200

    // ═══════ ASRC 参数（与 Rust rtp.rs 一一对应）═══════
    /** 重采样比率微调上限（±0.5%）。超出范围只能靠整帧重同步 */
    const val RESAMPLE_MAX_ADJUST = 0.005
    /** 单次比率更新最大步进（1%/s 的斜坡，防变调台阶） */
    const val RATIO_SLEW_STEP = 0.0001
    /** 比例增益：每 1 ms 水位偏差给 50 ppm（20 ms → 1000 ppm） */
    const val PI_KP_PER_MS = 0.00005
    /** 积分时间常数（秒）：必须 ≥5 s，否则会把网络抖动积成变调 */
    const val PI_TI_S = 10.0
    /** 单次评估的最小时间跨度（ms）：更密的调用只累计时间，不评估 */
    const val PI_MIN_UPDATE_MS = 5.0
    /** 水位一阶低通系数（τ ≈ 150 ms，带宽 ≈1 Hz） */
    const val WATER_SMOOTH_ALPHA = 0.065
    /** 速率前馈观测窗（秒）与桶数（每桶 1 s，桶内只留最小单向时延） */
    const val RATE_WINDOW_S = 30.0
    const val RATE_BUCKETS = 30
    /** 起算所需的最少桶数：没攒够宁可不出估计 */
    const val RATE_MIN_BUCKETS = 6
    /**
     * 单次观测允许的最大媒体跳变：超过它说明时间线被重建（重同步、发送端
     * 重启、断流后补齐），旧窗口的 t_local 与 media 已不可比，必须作废重算。
     * 正常相邻两包只差 480 样本，这里给 2 s 的宽松门限。Rust 侧逐字同构。
     */
    const val RATE_JUMP_MAX_SAMPLES = OPUS_SAMPLE_RATE * 2L
    /** 前馈钳位（±2%）：超出说明不是钟差，而是别的问题 */
    const val RATE_FEEDFORWARD_MAX = 0.02

    // ═══════ 控制头 ═══════

    class ControlHeader(val msgType: Int, val payloadLen: Int, val deviceId: ByteArray) {
        val isKnownType: Boolean get() = msgType in 1..TYPE_MAX_KNOWN
    }

    /** 解析控制头（只校验头部自身：长度 ≥15、版本、负载上限） */
    fun decodeControlHeader(buf: ByteArray, len: Int): ControlHeader? {
        if (len < CONTROL_HEADER_SIZE) return null
        if (buf[0] != CONTROL_VERSION) return null
        val payloadLen = ((buf[3].toInt() and 0xFF) shl 8) or (buf[4].toInt() and 0xFF)
        if (payloadLen > CONTROL_MAX_PAYLOAD) return null
        val id = ByteArray(DEVICE_ID_SIZE)
        System.arraycopy(buf, 7, id, 0, DEVICE_ID_SIZE)
        return ControlHeader(buf[2].toInt() and 0xFF, payloadLen, id)
    }

    /** 解析完整控制数据报：长度必须精确等于 15 + payload_len */
    fun parseControl(buf: ByteArray, len: Int): ControlHeader? {
        val h = decodeControlHeader(buf, len) ?: return null
        if (len != CONTROL_HEADER_SIZE + h.payloadLen) return null
        return h
    }

    /** 构建控制包；返回总长度 */
    fun buildControl(msgType: Byte, deviceId: ByteArray, payload: ByteArray): ByteArray {
        val out = ByteArray(CONTROL_HEADER_SIZE + payload.size)
        out[0] = CONTROL_VERSION
        out[1] = 0
        out[2] = msgType
        out[3] = (payload.size ushr 8).toByte()
        out[4] = payload.size.toByte()
        out[5] = 0
        out[6] = 0
        System.arraycopy(deviceId, 0, out, 7, DEVICE_ID_SIZE)
        System.arraycopy(payload, 0, out, CONTROL_HEADER_SIZE, payload.size)
        return out
    }

    // ═══════ CONNECT / CONNECT_ACK ═══════
    // opus_flags 位定义（与 Rust protocol.rs 一致）
    const val OPUS_FLAG_VBR = 0x01
    const val OPUS_FLAG_DTX = 0x02
    const val OPUS_FLAG_VBR_CONSTRAINT = 0x04
    const val OPUS_FLAG_FEC = 0x08

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
        fun encode(): ByteArray {
            val b = ByteArray(CONNECT_PAYLOAD_SIZE)
            b[0] = (reversePort ushr 8).toByte()
            b[1] = reversePort.toByte()
            b[2] = (forwardSsrc ushr 24).toByte()
            b[3] = (forwardSsrc ushr 16).toByte()
            b[4] = (forwardSsrc ushr 8).toByte()
            b[5] = forwardSsrc.toByte()
            b[6] = payloadType.toByte()
            b[7] = frameMs.toByte()
            b[8] = complexity.toByte()
            b[9] = signal.toByte()
            b[10] = bandwidth.toByte()
            b[11] = opusFlags.toByte()
            b[12] = (bitrateKbps ushr 8).toByte()
            b[13] = bitrateKbps.toByte()
            b[14] = packetLoss.toByte()
            return b
        }

        /** PT 与帧长必须合法，forward_ssrc 必须非零 */
        fun isValid(): Boolean =
            payloadType == OPUS_PAYLOAD_TYPE && frameMs == OPUS_FRAME_MS && forwardSsrc != 0

        companion object {
            fun decode(buf: ByteArray, offset: Int, len: Int): ConnectPayload? {
                if (len != CONNECT_PAYLOAD_SIZE) return null
                fun u(i: Int) = buf[offset + i].toInt() and 0xFF
                return ConnectPayload(
                    reversePort = (u(0) shl 8) or u(1),
                    forwardSsrc = (u(2) shl 24) or (u(3) shl 16) or (u(4) shl 8) or u(5),
                    payloadType = u(6),
                    frameMs = u(7),
                    complexity = u(8),
                    signal = u(9),
                    bandwidth = u(10),
                    opusFlags = u(11),
                    bitrateKbps = (u(12) shl 8) or u(13),
                    packetLoss = u(14),
                )
            }
        }
    }

    class ConnectAckPayload(val reverseSsrc: Int, val payloadType: Int, val frameMs: Int) {
        /** reverseSsrc==0 表示反向未启用；PT/帧长非法时整包丢弃 */
        fun isValid(): Boolean = payloadType == OPUS_PAYLOAD_TYPE && frameMs == OPUS_FRAME_MS

        companion object {
            fun decode(buf: ByteArray, offset: Int, len: Int): ConnectAckPayload? {
                if (len != CONNECT_ACK_PAYLOAD_SIZE) return null
                fun u(i: Int) = buf[offset + i].toInt() and 0xFF
                val p = ConnectAckPayload(
                    reverseSsrc = (u(0) shl 24) or (u(1) shl 16) or (u(2) shl 8) or u(3),
                    payloadType = u(4),
                    frameMs = u(5),
                )
                return if (p.isValid()) p else null
            }
        }
    }

    /** 码率归一：0=自动→上限，其余钳制到 [BITRATE_MAX_KBPS] */
    fun clampBitrateKbps(kbps: Int): Int = when {
        kbps <= 0 -> BITRATE_AUTO_KBPS
        kbps > BITRATE_MAX_KBPS -> BITRATE_MAX_KBPS
        else -> kbps
    }

    /**
     * 把任意码率吸附到最近的手动档位（[BITRATE_STEPS_KBPS]）。
     *
     * 只用于 UI/配置归一，不参与协议：历史配置里可能有 768/960/1000，
     * 直接塞进五档滑块会让滑块停在非法位置。等距时取较小档
     * （宁可少占带宽，也不悄悄把码率抬上去）。`0`（自动）原样返回。
     */
    fun snapBitrateKbps(kbps: Int): Int {
        if (kbps == 0) return 0
        val k = kbps.coerceAtMost(BITRATE_MAX_KBPS)
        var best = BITRATE_STEPS_KBPS[0]
        var bestD = Int.MAX_VALUE
        for (s in BITRATE_STEPS_KBPS) {
            val d = kotlin.math.abs(s - k)
            if (d < bestD) { bestD = d; best = s }
        }
        return best
    }

    /** DISCOVER_REPLY 负载：listen_port(2) + 设备名（UTF-8，不带 NUL，≤254 字节） */
    class DiscoverReply(val listenPort: Int, val deviceName: String) {
        companion object {
            fun decode(buf: ByteArray, offset: Int, len: Int): DiscoverReply? {
                if (len < 2) return null
                val port = ((buf[offset].toInt() and 0xFF) shl 8) or
                    (buf[offset + 1].toInt() and 0xFF)
                if (port <= 0) return null
                if (len - 2 > DEVICE_NAME_MAX) return null
                val name = String(buf, offset + 2, len - 2, Charsets.UTF_8)
                return DiscoverReply(port, name.ifEmpty { "UDP2CAL PC" })
            }
        }
    }

    // ═══════ RTP ═══════

    class RtpPacket(
        val marker: Boolean,
        val seq: Int,
        val timestamp: Long,
        val ssrc: Int,
        val payloadOffset: Int,
        val payloadLength: Int,
    )

    /**
     * 写入 12 字节 RTP 固定头（零分配）。
     * Opus 负载由调用方写在 `offset + RTP_HEADER_SIZE`。
     */
    fun writeRtpHeader(
        dest: ByteArray,
        offset: Int,
        marker: Boolean,
        seq: Int,
        timestamp: Int,
        ssrc: Int,
    ) {
        dest[offset] = 0x80.toByte() // V=2, P=0, X=0, CC=0
        dest[offset + 1] = ((if (marker) 0x80 else 0) or (OPUS_PAYLOAD_TYPE and 0x7F)).toByte()
        dest[offset + 2] = (seq ushr 8).toByte()
        dest[offset + 3] = seq.toByte()
        dest[offset + 4] = (timestamp ushr 24).toByte()
        dest[offset + 5] = (timestamp ushr 16).toByte()
        dest[offset + 6] = (timestamp ushr 8).toByte()
        dest[offset + 7] = timestamp.toByte()
        dest[offset + 8] = (ssrc ushr 24).toByte()
        dest[offset + 9] = (ssrc ushr 16).toByte()
        dest[offset + 10] = (ssrc ushr 8).toByte()
        dest[offset + 11] = ssrc.toByte()
    }

    /** 严格解析 RTP 包；任一校验失败返回 null（调用方只计数） */
    fun parseRtp(buf: ByteArray, len: Int): RtpPacket? {
        if (len < RTP_HEADER_SIZE + 1) return null
        if (len > RTP_MAX_PACKET) return null
        if (buf[0] != 0x80.toByte()) return null
        val b1 = buf[1].toInt() and 0xFF
        if ((b1 and 0x7F) != OPUS_PAYLOAD_TYPE) return null
        val seq = ((buf[2].toInt() and 0xFF) shl 8) or (buf[3].toInt() and 0xFF)
        val ts = ((buf[4].toLong() and 0xFF) shl 24) or ((buf[5].toLong() and 0xFF) shl 16) or
            ((buf[6].toLong() and 0xFF) shl 8) or (buf[7].toLong() and 0xFF)
        val ssrc = ((buf[8].toInt() and 0xFF) shl 24) or ((buf[9].toInt() and 0xFF) shl 16) or
            ((buf[10].toInt() and 0xFF) shl 8) or (buf[11].toInt() and 0xFF)
        if (ssrc == 0) return null
        val payloadLen = len - RTP_HEADER_SIZE
        if (payloadLen <= 0 || payloadLen > OPUS_MAX_PAYLOAD) return null
        return RtpPacket(
            marker = (b1 and 0x80) != 0,
            seq = seq,
            timestamp = ts,
            ssrc = ssrc,
            payloadOffset = RTP_HEADER_SIZE,
            payloadLength = payloadLen,
        )
    }

    // ═══════ 扩展序号 / 时间戳 ═══════

    /** 16 位有符号差值扩展：65535 + 1 → 65536 */
    fun extendSeq(highest: Long, seq: Int): Long {
        val diff = (seq - (highest and 0xFFFFL).toInt()).toShort().toInt()
        return (highest + diff).coerceAtLeast(0)
    }

    /** 32 位有符号差值扩展：0xFFFFFF00 + 480 → 0x1000000E0 */
    fun extendTs(highest: Long, ts: Long): Long {
        val diff = (ts - (highest and 0xFFFFFFFFL)).toInt()
        return (highest + diff).coerceAtLeast(0)
    }

    // ═══════ 设备 ID ═══════

    /**
     * 生成 8 字节设备 ID。设备 ID 只用于控制面绑定/发现，**不是密钥**：
     * 它每秒随 DISCOVER_REPLY 明文广播，只能防误连。
     */
    fun generateDeviceId(): ByteArray {
        val id = ByteArray(DEVICE_ID_SIZE)
        java.security.SecureRandom().nextBytes(id)
        if (id.all { it == 0.toByte() }) id[0] = 1 // 全零视为无效 ID
        return id
    }

    fun deviceIdToString(id: ByteArray): String =
        if (id.size < DEVICE_ID_SIZE) "invalid"
        else id.joinToString("") { "%02x".format(it) }

    // ═══════ 抖动缓冲 ═══════

    object Pull {
        const val WAIT = 0
        const val PACKET = 1
        const val PLC = 2
        const val SILENCE = 3
    }

    /**
     * 单方向接收抖动缓冲（与 Rust `rtp::JitterBuffer` 同语义）：
     * - 首包建立基准；连续 4 帧才进入 PLAYING；
     * - 以时间戳为播放顺序的权威，序号用于重复检测；
     * - 缺口只在**播放头到期**时生成 PLC，连续 12 帧后输出静音但仍留在 PLAYING；
     * - M=1 且跳变 > 200 ms 才重同步。
     *
     * 非线程安全：调用方必须用同一把锁包住 [insert] 与 [pull]。
     */
    class JitterBuffer {
        private val slotValid = BooleanArray(MAX_REORDER_PACKETS)
        private val slotSeq = LongArray(MAX_REORDER_PACKETS)
        private val slotTs = LongArray(MAX_REORDER_PACKETS)
        private val slotLen = IntArray(MAX_REORDER_PACKETS)
        private val slotData = Array(MAX_REORDER_PACKETS) { ByteArray(OPUS_MAX_PAYLOAD) }

        /** 到达时刻环形记录：估算"正在播放的样本有多旧"（总延迟） */
        private val arrivalTs = LongArray(ARRIVAL_TRACE)
        private val arrivalAt = LongArray(ARRIVAL_TRACE) { -1L }
        private var arrivalPos = 0

        var nextSeq = 0L; private set
        var nextTs = 0L; private set
        private var highestSeq = 0L
        private var highestTs = 0L

        /** 已收到的最高媒体时间戳（前馈速率估计的观测点） */
        fun highestTsValue(): Long = highestTs
        var started = false; private set
        var playing = false; private set
        var plcRun = 0; private set
        var epoch = 0L; private set
        /** 最近一次重同步的原因（诊断用，仅 rebase 时写入） */
        var lastRebaseReason = ""; private set

        /** [pull] 的结果：最近一帧的长度与时间戳 */
        var pulledLength = 0; private set
        var pulledTs = 0L; private set

        // 统计
        var received = 0L; private set
        var late = 0L; private set
        var duplicate = 0L; private set
        var resyncs = 0L; private set
        /** 其中因"序号滞后但时间戳合法"触发的重新对齐次数（断流/DTX 自愈计数） */
        var seqRealign = 0L; private set
        var overflow = 0L; private set
        var plcFrames = 0L; private set
        var maxPlcRun = 0; private set

        fun reset() {
            for (i in 0 until MAX_REORDER_PACKETS) slotValid[i] = false
            nextSeq = 0
            nextTs = 0
            highestSeq = 0
            highestTs = 0
            started = false
            playing = false
            plcRun = 0
            // 必须清空：否则新会话的日志会打印上一次会话的重同步原因（诊断误导）
            lastRebaseReason = ""
            // 到达记录随重同步一起作废：留着会让延迟估算瞬间跳成"几百秒"
            arrivalAt.fill(-1L)
            arrivalPos = 0
            epoch++
        }

        /**
         * 查询"时间戳 [ts] 那一帧是什么时候到达的"（`System.nanoTime()`）。
         *
         * 取最新一条 `entryTs <= ts` 的记录——帧按时间戳单调到达，该记录就是
         * 这一帧（或它最近的同刻邻居）。查不到（刚重同步/记录已被覆盖）返回 -1。
         */
        fun arrivalNanosAt(ts: Long): Long {
            for (i in 0 until ARRIVAL_TRACE) {
                val idx = (arrivalPos + ARRIVAL_TRACE - 1 - i) % ARRIVAL_TRACE
                val at = arrivalAt[idx]
                if (at >= 0 && arrivalTs[idx] <= ts) return at
            }
            return -1L
        }

        /**
         * 当前水位（毫秒）：**已到达但还没播出去的媒体量**。
         *
         * 用 `highestTs − nextTs` 而不是 `contiguousFrames()`：后者在"队首那一帧
         * 还没到"（乱序/迟到）时会瞬间塌到 0，而后面明明还压着十几帧——当成控制
         * 信号会让 PI 误以为缓冲空了、反向拉满比率。Rust 侧逐字同构。
         */
        fun waterMs(): Int {
            if (highestTs < nextTs) return 0
            val frames = ((highestTs - nextTs) / OPUS_FRAME_SAMPLES + 1).coerceAtMost(MAX_REORDER_PACKETS.toLong())
            return (frames * OPUS_FRAME_MS).toInt()
        }

        fun contiguousFrames(): Int {
            var n = 0
            var ts = nextTs
            while (n < MAX_REORDER_PACKETS) {
                if (findTs(ts) < 0) break
                n++
                ts += OPUS_FRAME_SAMPLES
            }
            return n
        }

        // 播放决策只看水位：没有播放头、没有媒体时间偏移，也没有任何
        // "设备帧 → 媒体时间戳"的换算（规范 §二）。

        /** 网络线程调用。返回 true 表示已进入缓冲（可用于码率统计） */
        fun insert(seq: Int, ts: Long, marker: Boolean, buf: ByteArray, offset: Int, length: Int): Boolean {
            received++
            if (length <= 0 || length > OPUS_MAX_PAYLOAD) return false
            if (!started) {
                // 首包只建基准：不推进 epoch（epoch 表示需要重建解码器/播放头）
                rebase(seq.toLong() and 0xFFFFL, ts, "", bumpEpoch = false)
                store(seq.toLong() and 0xFFFFL, ts, buf, offset, length)
                return true
            }
            val extSeq = extendSeq(highestSeq, seq)
            val extTs = extendTs(highestTs, ts)

            if (extTs < nextTs) { late++; return false }
            if (findTs(extTs) >= 0) { duplicate++; return false }

            // ── 断点判定（必须先于"序号迟到"判定）──
            // 时间戳合法且未重复 → 说明是真实的新音频。序号落后只可能来自
            // "发送端断流/DTX 期间只推进时间戳不推进序号"或"断流跨过 16 位
            // 有符号差值的 32767 边界"。这两种情况下接收端的 nextSeq 已被
            // PLC/静音推进，滞后量永不闭合；若在此直接丢弃，M=1 重同步分支
            // 永远不可达，整条流会永久静音。因此按时间戳重新对齐。
            val gapMs = (extTs - nextTs) * 1000L / OPUS_SAMPLE_RATE
            if (extSeq < nextSeq) {
                rebase(extSeq, extTs, "序号滞后 ${nextSeq - extSeq} 帧，按时间戳重新对齐")
                store(extSeq, extTs, buf, offset, length)
                resyncs++
                seqRealign++
                return true
            }

            if (marker && gapMs > RESYNC_GAP_MS.toLong()) {
                rebase(extSeq, extTs, "M=1 且时间戳跳变 ${gapMs}ms")
                store(extSeq, extTs, buf, offset, length)
                resyncs++
                return true
            }
            // 超出重排窗口（nextSeq .. nextSeq+63）：只丢这一包。
            // 绝不重同步——那会连同 64 帧已缓冲音频一起丢掉（约 640 ms 空洞，
            // 听感就是"卡塔"）；丢一包只会让播放头走到时产生一次 10 ms PLC。
            if (extSeq >= nextSeq + MAX_REORDER_PACKETS.toLong()) {
                overflow++
                return false
            }
            if (slotValid.count { it } >= MAX_REORDER_PACKETS) {
                dropOldestNonNext()
                overflow++
            }
            store(extSeq, extTs, buf, offset, length)
            if (extTs > highestTs) highestTs = extTs
            if (extSeq > highestSeq) highestSeq = extSeq
            return true
        }

        /**
         * 播放线程调用：按**缓冲水位**拉取下一帧（规范 §二）。
         *
         * 两个入参是同一个量的两种口径：
         * * [ringMs]：已解码、已写进 PCM 环但还没被声卡接手的媒体量；
         * * [pipeMs]：`ringMs` + 声卡已接手还没播完的。
         *
         * 生产节流只约束我们自己可控的那段（PCM 环）；缺口判定必须用
         * `pipeMs`——只有可听点真的逼近缺口时才该补 PLC。
         * Android 端写 AudioTrack 是阻塞的（没有独立 PCM 环），
         * 因此 [ringMs] 恒为 0，节拍由 `writeBlocking` 与水位数共同保证。
         *
         * @return [Pull] 常量；[pulledLength]/[pulledTs] 描述本帧
         */
        fun pull(ringMs: Int, pipeMs: Int, out: ByteArray): Int {
            pulledLength = 0
            if (!started) return Pull.WAIT
            if (!playing) {
                if (contiguousFrames() < JITTER_START_MS / OPUS_FRAME_MS) return Pull.WAIT
                playing = true
            }
            val idx = findTs(nextTs)
            if (idx >= 0) {
                // 解码缓冲已够深：生产节奏必须由声卡消费节奏约束
                if (ringMs >= DECODE_LIMIT_MS) return Pull.WAIT
                val len = slotLen[idx]
                System.arraycopy(slotData[idx], 0, out, 0, len)
                slotValid[idx] = false
                pulledLength = len
                pulledTs = nextTs
                advance()
                plcRun = 0
                return Pull.PACKET
            }
            // 可听点之前还压着的媒体量已经见底，才生成补偿
            if (pipeMs <= PLC_LEAD_MS) {
                pulledTs = nextTs
                advance()
                val maxRun = PLC_MAX_MS / OPUS_FRAME_MS
                return if (plcRun < maxRun) {
                    plcRun++
                    plcFrames++
                    if (plcRun > maxPlcRun) maxPlcRun = plcRun
                    Pull.PLC
                } else {
                    Pull.SILENCE
                }
            }
            return Pull.WAIT
        }

        private fun rebase(seq: Long, ts: Long, reason: String, bumpEpoch: Boolean = true) {
            lastRebaseReason = reason
            for (i in 0 until MAX_REORDER_PACKETS) slotValid[i] = false
            nextSeq = seq
            nextTs = ts
            highestSeq = seq
            highestTs = ts
            started = true
            playing = false
            plcRun = 0
            if (bumpEpoch) epoch++
        }

        private fun advance() {
            nextSeq++
            nextTs += OPUS_FRAME_SAMPLES
        }

        private fun findTs(ts: Long): Int {
            for (i in 0 until MAX_REORDER_PACKETS) {
                if (slotValid[i] && slotTs[i] == ts) return i
            }
            return -1
        }

        private fun store(seq: Long, ts: Long, buf: ByteArray, offset: Int, length: Int) {
            var idx = -1
            for (i in 0 until MAX_REORDER_PACKETS) {
                if (!slotValid[i]) { idx = i; break }
            }
            if (idx < 0) {
                var minSeq = Long.MAX_VALUE
                for (i in 0 until MAX_REORDER_PACKETS) {
                    if (slotSeq[i] < minSeq) { minSeq = slotSeq[i]; idx = i }
                }
            }
            if (idx < 0) idx = 0
            System.arraycopy(buf, offset, slotData[idx], 0, length)
            slotValid[idx] = true
            slotSeq[idx] = seq
            slotTs[idx] = ts
            slotLen[idx] = length
            // 所有"接纳成功"的路径都汇聚到这里：到达时刻一并落账（延迟估算用）
            arrivalTs[arrivalPos] = ts
            arrivalAt[arrivalPos] = System.nanoTime()
            arrivalPos = (arrivalPos + 1) % ARRIVAL_TRACE
        }

        private fun dropOldestNonNext() {
            var victim = -1
            var minSeq = Long.MAX_VALUE
            for (i in 0 until MAX_REORDER_PACKETS) {
                if (!slotValid[i] || slotTs[i] == nextTs) continue
                if (slotSeq[i] < minSeq) { minSeq = slotSeq[i]; victim = i }
            }
            if (victim >= 0) slotValid[victim] = false
        }
    }

    /**
     * 速率前馈估计器（与 Rust `rtp::RateEstimator` 同语义）。
     *
     * 前馈管速率、反馈管相位（规范 §四）。两条支路互不污染，所以各自可以挑
     * 最合适的传感器：
     *
     * * 前馈用 **(RTP 扩展时间戳, 到达时刻)**：时间戳按 480/包推进，丢包不会
     *   让它少算——它是发送端媒体时间线的无偏观测；用"已收样本数"则每丢一包
     *   少 480，估计值被丢包率直接带偏；
     * * 反馈用 **缓冲水位**：闭环量只能留在闭环里。
     *
     * 观测对取"最新到达的那个包"的时间戳与到达时刻，两者来自同一条记录，
     * 因此不含播放/解码节奏的抖动；再用最小延迟滤波（每桶只留最小单向时延）
     * 剔掉排队造成的正向偏移——排队只会让单向时延变大，最小值几乎不受影响
     * （NTP min-filter / WebRTC 自适应最小延迟同源）。
     */
    class RateEstimator {
        private var started = false
        /** 观测窗起点（`System.nanoTime()`） */
        private var t0 = 0L
        private var ts0 = 0L
        private var lastTs = 0L
        private val bucketEpoch = LongArray(RATE_BUCKETS) { Long.MAX_VALUE }
        private val bucketT = DoubleArray(RATE_BUCKETS)
        private val bucketMinU = DoubleArray(RATE_BUCKETS) { Double.POSITIVE_INFINITY }
        private var ff: Double? = null
        private var sigmaPpm = 0.0
        private var used = 0

        /** 累计观测数（诊断） */
        var observations = 0L; private set

        fun reset() {
            started = false
            t0 = 0
            ts0 = 0
            lastTs = 0
            bucketEpoch.fill(Long.MAX_VALUE)
            bucketMinU.fill(Double.POSITIVE_INFINITY)
            ff = null
            sigmaPpm = 0.0
            used = 0
        }

        /** 当前前馈估计（无量纲速率残差）；窗口未攒够时为 null */
        fun feedforward(): Double? = ff

        fun feedforwardPpm(): Double? = ff?.times(1e6)

        /** 估计的 1σ（ppm），供日志判断可信度 */
        fun sigmaPpm(): Double = sigmaPpm

        /** 参与拟合的桶数 */
        fun usedBuckets(): Int = used

        /**
         * 观测一个包：[ts] 是它的 RTP 扩展时间戳，[arrivedNanos] 是它进入
         * 接收端的本地单调时刻。重复喂同一个时间戳会被忽略。
         */
        fun observe(ts: Long, arrivedNanos: Long) {
            if (!started) {
                // 首个观测只建立基准：窗口从"现在"起算
                started = true
                t0 = arrivedNanos
                ts0 = ts
                lastTs = ts
                return
            }
            if (ts == lastTs) return
            // 基准之后又出现更早的时间戳：只可能是重同步换了基准，
            // 调用方本应 reset()；这里直接忽略，不污染现有窗口。
            if (ts < ts0) return
            if (ts - lastTs > RATE_JUMP_MAX_SAMPLES) {
                // 时间线被重建（重同步/发送端重启/断流补齐）：旧窗口的
                // (t_local, media) 对已经不可比，接着用会让回归斜率变成垃圾
                // （实测会出现 ±2000 ppm 的假前馈）。从这一包重新起算。
                reset()
                started = true
                t0 = arrivedNanos
                ts0 = ts
                lastTs = ts
                return
            }
            lastTs = ts
            observations++

            val tLocal = (arrivedNanos - t0).toDouble() / 1e9
            val media = (ts - ts0).toDouble() / OPUS_SAMPLE_RATE
            // u 只随两侧钟差变化：网络排队只会让它变大，最小延迟滤波正为此
            val u = tLocal - media

            val bucketS = RATE_WINDOW_S / RATE_BUCKETS
            val epoch = (tLocal / bucketS).toLong()
            val idx = (epoch % RATE_BUCKETS).toInt()
            if (bucketEpoch[idx] != epoch) {
                bucketEpoch[idx] = epoch
                bucketT[idx] = (epoch + 0.5) * bucketS
                bucketMinU[idx] = u
            } else if (u < bucketMinU[idx]) {
                bucketMinU[idx] = u
            }
            fit(epoch)
        }

        /** 最小二乘拟合桶最小值（只有 30 个点，每次观测重算代价可忽略） */
        private fun fit(curEpoch: Long) {
            var n = 0
            var sx = 0.0
            var sy = 0.0
            for (i in 0 until RATE_BUCKETS) {
                if (!live(i, curEpoch)) continue
                n++
                sx += bucketT[i]
                sy += bucketMinU[i]
            }
            used = n
            if (n < RATE_MIN_BUCKETS) {
                // 窗口还没攒够：宁可不出估计，也不要拿噪声去改比率
                ff = null
                sigmaPpm = 0.0
                return
            }
            val mx = sx / n
            val my = sy / n
            var sxx = 0.0
            var sxy = 0.0
            for (i in 0 until RATE_BUCKETS) {
                if (!live(i, curEpoch)) continue
                val dx = bucketT[i] - mx
                sxx += dx * dx
                sxy += dx * (bucketMinU[i] - my)
            }
            if (sxx <= 0.0) {
                ff = null
                sigmaPpm = 0.0
                return
            }
            val slope = sxy / sxx
            var ss = 0.0
            for (i in 0 until RATE_BUCKETS) {
                if (!live(i, curEpoch)) continue
                val r = bucketMinU[i] - (my + slope * (bucketT[i] - mx))
                ss += r * r
            }
            val dof = (n - 2.0).coerceAtLeast(1.0)
            // 残差标准差 → 斜率标准差：σ_slope = s / √Sxx
            sigmaPpm = kotlin.math.sqrt(ss / dof) / kotlin.math.sqrt(sxx) * 1e6
            // u = 本地时间 − 媒体时间 的斜率：媒体时钟快 → u 变小 → 斜率 < 0
            // → 速率残差 = −斜率
            ff = (-slope).coerceIn(-RATE_FEEDFORWARD_MAX, RATE_FEEDFORWARD_MAX)
        }

        /** 该桶是否仍在当前窗口内（被回收的桶 epoch 落后一整圈） */
        private fun live(i: Int, curEpoch: Long): Boolean =
            bucketEpoch[i] != Long.MAX_VALUE &&
                bucketMinU[i].isFinite() &&
                curEpoch - bucketEpoch[i] < RATE_BUCKETS.toLong()
    }

    /**
     * 漂移校正器：**抗饱和 PI**（与 Rust `rtp::DriftController` 同语义）。
     *
     * 输入是单一口径的总水位（抖动缓冲 + PCM 环 + 声卡缓冲）。为什么是 PI
     * 而不是纯比例：纯比例必然留下静差（水位永远差一点、比率永远差一截），
     * "稳态比率残差 <20 ppm"只有积分项能做到。为什么积分时间常数必须 ≥5 s：
     * 积分项看到的是同一个带网络抖动的水位，时间常数太小会把抖动积成变调。
     *
     * 计时用**声卡已消费帧数**而不是媒体样本数：断流期间媒体样本不前进，
     * 墙钟却在走，用媒体样本计时会让积分项在断流时"冻结"，恢复后补偿过头。
     */
    class DriftController(private val deviceRate: Int = OPUS_SAMPLE_RATE) {
        private var water = 0.0
        private var adjust = 0.0
        private var integral = 0.0
        private var primed = false
        private var lastDevice = 0L

        /** 评估次数（诊断） */
        var ratioUpdates = 0L; private set

        fun reset() {
            water = 0.0
            adjust = 0.0
            integral = 0.0
            primed = false
            lastDevice = 0
        }

        fun adjust(): Double = adjust

        /** 低通后的水位（诊断：与瞬时水位对比可看出控制器有多"钝"） */
        fun water(): Double = water

        /** 积分项（诊断：长期顶满说明比率钳位不够用） */
        fun integral(): Double = integral

        /**
         * [waterMs] 为单一口径总水位，[deviceFrames] 为声卡已消费的设备帧数。
         * 返回新的比率微调量：水位偏高 → 正数 → 每秒产出的设备帧变少。
         */
        fun update(waterMs: Int, deviceFrames: Long): Double {
            val rate = deviceRate.coerceAtLeast(1)
            val dframes = deviceFrames - lastDevice
            if (dframes <= 0L) return adjust
            val dtMs = dframes * 1000.0 / rate
            // 太密的调用只累计时间，不评估：低通与积分都按真实 dt 缩放
            if (dtMs < PI_MIN_UPDATE_MS) return adjust
            lastDevice = deviceFrames

            if (!primed) {
                primed = true
                water = waterMs.toDouble()
                return adjust
            }

            // ── 水面低通（τ≈150 ms，带宽≈1 Hz：滤掉 10 ms 帧量化噪声）──
            val steps = (dtMs / OPUS_FRAME_MS).coerceAtLeast(1e-3)
            val alpha = 1.0 - Math.pow(1.0 - WATER_SMOOTH_ALPHA, steps)
            water += alpha * (waterMs - water)

            val err = water - WATER_TARGET_MS
            // ── 比例项 ──
            val p = (err * PI_KP_PER_MS).coerceIn(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST)
            // ── 积分项（抗饱和：输出顶满且误差还在往同方向顶时冻结）──
            val ki = PI_KP_PER_MS / PI_TI_S
            val iNext = (integral + ki * err * (dtMs / 1000.0))
                .coerceIn(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST)
            val want = p + iNext
            val saturated = kotlin.math.abs(want) >= RESAMPLE_MAX_ADJUST
            if (!saturated || err * iNext < 0.0) integral = iNext

            // ── 斜率限制（1%/s，按真实 dt 缩放）：不让比例项跳变成变调 ──
            val target = want.coerceIn(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST)
            val slew = RATIO_SLEW_STEP * steps.coerceAtLeast(1.0)
            val step = (target - adjust).coerceIn(-slew, slew)
            adjust = (adjust + step).coerceIn(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST)
            ratioUpdates++
            return adjust
        }
    }
}
