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
    /** 预缓冲启动阈值（3 帧 = 30 ms）。离散丢/插让位给受控重采样后可下探到 30 ms */
    const val JITTER_START_MS = 30
    const val JITTER_TARGET_MS = 60
    const val JITTER_HIGH_MS = 120
    const val JITTER_LOW_MS = 20
    const val PLC_LEAD_MS = 20
    const val PLC_MAX_MS = 120
    const val RESYNC_GAP_MS = 200

    // ═══════ 漂移校正参数（与 Rust rtp.rs 一一对应）═══════
    /** 有界回落的单次量：1 ms */
    const val SLIP_SAMPLES = 48
    /** 交叉淡化长度：1 ms，丢/插都只能在淡化区内完成 */
    const val SLIP_FADE_SAMPLES = 48
    /** 回落高带：高于目标 30 ms */
    const val SLIP_HIGH_MARGIN_MS = 30
    /** 回落低带：低于目标 10 ms */
    const val SLIP_LOW_MARGIN_MS = 10
    /** 回落最小间隔：1 秒音频样本时间 —— 每秒最多 1 ms */
    const val SLIP_MIN_INTERVAL_SAMPLES = 48000L
    /** 回落评估周期：100 ms 音频样本时间 */
    const val SLIP_EVAL_SAMPLES = 4800L
    /** 高带"稳定"判据：连续 5 次评估 */
    const val SLIP_STABLE_ROUNDS = 5
    /** 启动宽限：前 2 秒只做比率微调 */
    const val SLIP_WARMUP_SAMPLES = 96000L
    /** 重采样比率微调上限（±0.5%） */
    const val RESAMPLE_MAX_ADJUST = 0.005
    /** 比率更新周期：每 10 ms（1 帧） */
    const val RATIO_UPDATE_SAMPLES = 480L
    /** 单次比率更新最大步进（1%/s 的斜坡） */
    const val RATIO_SLEW_STEP = 0.0001
    const val RATIO_DEADBAND_MS = 5.0
    const val RATIO_FULL_MS = 20.0
    /** 水位一阶低通系数（τ ≈ 150 ms） */
    const val WATER_SMOOTH_ALPHA = 0.065

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

        fun waterMs(): Int = contiguousFrames() * OPUS_FRAME_MS

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

        // 播放线程校正 PCM 后的媒体时间偏移只由 PlayheadClock.notePcmCorrection 承担：
        // pull 的缺口判定用的是播放头（nextTs <= playheadTs + lead），把偏移加在
        // 播放头上即可让整条媒体时间线平移。抖动缓冲不再保存副本——历史上那份
        // 副本只写不读，纯属误导。

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
         * 播放线程调用：按播放头拉取下一帧。
         * @return [Pull] 常量；[pulledLength]/[pulledTs] 描述本帧
         */
        fun pull(playheadTs: Long, out: ByteArray): Int {
            pulledLength = 0
            if (!started) return Pull.WAIT
            if (!playing) {
                if (contiguousFrames() < JITTER_START_MS / OPUS_FRAME_MS) return Pull.WAIT
                playing = true
            }
            val idx = findTs(nextTs)
            if (idx >= 0) {
                val len = slotLen[idx]
                System.arraycopy(slotData[idx], 0, out, 0, len)
                slotValid[idx] = false
                pulledLength = len
                pulledTs = nextTs
                advance()
                plcRun = 0
                return Pull.PACKET
            }
            val lead = (PLC_LEAD_MS * OPUS_FRAME_SAMPLES / OPUS_FRAME_MS).toLong()
            if (nextTs <= playheadTs + lead) {
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
     * 漂移 / 延迟校正器（与 Rust `rtp::DriftController` 同语义）。
     *
     * **第一级：受控重采样（主力）** —— 水位偏差经低通后按比例映射到重采样
     * 比率的 ±0.5%，每 10 ms 更新一次，单次步进 ≤0.0001。比率是连续量，
     * 不会在 PCM 里留下任何接缝。
     *
     * **第二级：有界回落（安全网）** —— 只有水位持续跑出回落带（高于目标
     * 30 ms 且稳定 / 低于目标 10 ms）才做一次 1 ms 的丢/插，每秒最多一次，
     * 且必须交叉淡化。
     *
     * @return [DriftUpdate]：`adjust` 为比率微调；`slipSamples > 0` 表示丢弃
     *         该样本数、`< 0` 表示插入该样本数、`0` 表示不动作。
     */
    class DriftController {
        private var water = 0.0
        private var adjust = 0.0
        private var primed = false
        private var lastRatio = 0L
        private var lastEval = 0L
        private var lastSlip = 0L
        private var streak = 0

        var ratioUpdates = 0L; private set
        var slipDrops = 0L; private set
        var slipInserts = 0L; private set
        var slippedSamples = 0L; private set

        fun reset() {
            water = 0.0
            adjust = 0.0
            primed = false
            lastRatio = 0
            lastEval = 0
            lastSlip = 0
            streak = 0
        }

        fun adjust(): Double = adjust

        fun update(waterMs: Int, mediaSamples: Long): DriftUpdate {
            if (!primed) {
                primed = true
                water = waterMs.toDouble()
                lastRatio = mediaSamples
                lastEval = mediaSamples
                lastSlip = mediaSamples
                return DriftUpdate.NONE
            }

            // ── 第一级：比率微调（每 10 ms 一次）──
            if (mediaSamples - lastRatio >= RATIO_UPDATE_SAMPLES) {
                lastRatio = mediaSamples
                water += WATER_SMOOTH_ALPHA * (waterMs - water)
                val err = water - JITTER_TARGET_MS
                val mag = kotlin.math.abs(err)
                val want = if (mag <= RATIO_DEADBAND_MS) {
                    0.0
                } else {
                    val k = ((mag - RATIO_DEADBAND_MS) / (RATIO_FULL_MS - RATIO_DEADBAND_MS))
                        .coerceAtMost(1.0)
                    kotlin.math.sign(err) * RESAMPLE_MAX_ADJUST * k
                }
                val step = (want - adjust).coerceIn(-RATIO_SLEW_STEP, RATIO_SLEW_STEP)
                adjust = (adjust + step).coerceIn(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST)
                ratioUpdates++
            }

            // ── 第二级：有界回落 ──
            return DriftUpdate(adjust, evaluateSlip(waterMs, mediaSamples))
        }

        private fun evaluateSlip(waterMs: Int, mediaSamples: Long): Int {
            if (mediaSamples - lastEval < SLIP_EVAL_SAMPLES) return 0
            lastEval = mediaSamples

            if (mediaSamples < SLIP_WARMUP_SAMPLES) {
                streak = 0
                return 0
            }

            val high = JITTER_TARGET_MS + SLIP_HIGH_MARGIN_MS
            val low = JITTER_TARGET_MS - SLIP_LOW_MARGIN_MS
            if (waterMs > high) streak++ else streak = 0

            // 速率闸门：每秒最多 1 ms，绝不连续触发
            if (mediaSamples - lastSlip < SLIP_MIN_INTERVAL_SAMPLES) return 0

            if (streak >= SLIP_STABLE_ROUNDS) {
                streak = 0
                lastSlip = mediaSamples
                slipDrops++
                slippedSamples += SLIP_SAMPLES
                return SLIP_SAMPLES
            }
            if (waterMs < low) {
                streak = 0
                lastSlip = mediaSamples
                slipInserts++
                slippedSamples += SLIP_SAMPLES
                return -SLIP_SAMPLES
            }
            return 0
        }
    }

    /** 一次漂移校正评估的产物 */
    class DriftUpdate(val adjust: Double, val slipSamples: Int) {
        companion object {
            val NONE = DriftUpdate(0.0, 0)
        }
    }

    /**
     * 播放头：把 AudioTrack 已播放的设备帧数映射到 48 kHz RTP 时间线。
     *
     * AudioTrack 固定以 48 kHz 创建（设备内部重采样由系统完成），
     * 因此设备帧与媒体样本 1:1；换算在这里仍然按 7.10 的公式保留余数处理。
     */
    class PlayheadClock(private val deviceRate: Int = OPUS_SAMPLE_RATE) {
        private var baseTs = 0L
        private var lastDeviceFrames = 0L
        private var lastValue = 0L
        private var remainder = 0L
        private var offset = 0L
        /** 比率微调造成的不足一个样本的残余（逐帧取整会吃掉近 17% 的校正量） */
        private var offsetFrac = 0.0
        var isStarted = false; private set

        fun start(baseTs: Long, playedFrames: Long) {
            this.baseTs = baseTs
            this.lastValue = baseTs
            this.lastDeviceFrames = playedFrames
            this.remainder = 0
            this.offset = 0
            this.offsetFrac = 0.0
            isStarted = true
        }

        fun update(playedFrames: Long): Long? {
            if (!isStarted) return null
            if (playedFrames < lastDeviceFrames) return lastValue + offset
            val delta = playedFrames - lastDeviceFrames
            lastDeviceFrames = playedFrames
            if (delta > 0) {
                val num = delta * OPUS_SAMPLE_RATE + remainder
                lastValue += num / deviceRate
                remainder = num % deviceRate
            }
            return lastValue + offset
        }

        fun value(): Long? = if (isStarted) lastValue + offset else null

        /** 有界回落：丢 PCM → delta > 0 让播放头前移 */
        fun notePcmCorrection(deltaSamples: Long) {
            offset += deltaSamples
        }

        /**
         * 重采样比率微调造成的媒体时间偏移（**连续量**，每帧调用）。
         * 比率乘 `(1+u)` 后每个设备帧要多算 `u` 个媒体样本，必须用小数累积。
         */
        fun noteRatioCorrection(samples: Double) {
            if (!samples.isFinite()) return
            offsetFrac += samples
            val whole = offsetFrac.toLong()
            if (whole != 0L) {
                offset += whole
                offsetFrac -= whole
            }
        }

        fun reset() {
            isStarted = false
            baseTs = 0
            lastValue = 0
            lastDeviceFrames = 0
            remainder = 0
            offset = 0
            offsetFrac = 0.0
        }
    }
}
