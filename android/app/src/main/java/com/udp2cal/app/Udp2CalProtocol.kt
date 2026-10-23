package com.udp2cal.app

/**
 * UDP2CAL UDP私有协议编解码 (Kotlin实现)
 *
 * ## v2 统一协议 (15字节包头 + 负载)
 * 音频数据包和控制消息共用同一包头格式，所有包携带设备 ID 实现 1对1 P2P 过滤。
 *
 * Byte 0:       PROTO_VERSION = 0x02
 * Byte 1:       [1bit is_audio][3bit codec][4bit sample_rate]
 * Byte 2:       [8bit msg_type]
 * Byte 3-4:     [16bit payload_len BE]
 * Byte 5:       [8bit bitrate_kbps] (0=auto)
 * Byte 6:       [8bit seq_num]
 * Byte 7-14:    [8 bytes device_id]
 * Byte 15+:     [payload]
 *
 * 与Rust端 protocol/protocol.rs 100%对齐
 */
object Udp2CalProtocol {
    const val HEADER_SIZE = 15
    const val MAX_PAYLOAD = 1472
    const val MAX_PACKET = HEADER_SIZE + MAX_PAYLOAD
    const val PROTO_VERSION: Byte = 2
    const val DEVICE_ID_SIZE = 8
    const val DISCOVER_PORT = 44043

    // 消息类型
    const val TYPE_DATA: Byte = 0          // 音频数据
    const val TYPE_CONNECT: Byte = 1       // 连接请求
    const val TYPE_DISCOVER_REQ: Byte = 2  // 发现请求
    const val TYPE_DISCOVER_REPLY: Byte = 3 // 发现回复
    const val TYPE_CONNECT_ACK: Byte = 4   // 连接确认

    // 采样率
    const val SAMPLE_RATE_8K: Byte = 0
    const val SAMPLE_RATE_12K: Byte = 1
    const val SAMPLE_RATE_16K: Byte = 2
    const val SAMPLE_RATE_24K: Byte = 3
    const val SAMPLE_RATE_48K: Byte = 4
    const val BITRATE_AUTO: Byte = 0

    @Suppress("ArrayInDataClass")
    data class PacketHeader(
        val version: Byte = PROTO_VERSION,
        val isAudio: Boolean = false,
        val codec: Byte = 1,
        val sampleRate: Byte = SAMPLE_RATE_48K,
        val msgType: Byte = 0,
        val payloadLen: Int = 0,
        val bitrate: Byte = BITRATE_AUTO,
        val seqNum: Byte = 0,
        val deviceId: ByteArray = ByteArray(DEVICE_ID_SIZE)
    )

    // ═══════ 检测 ═══════
    // （v1.1.2 起协议只有单一版本：首字节 != PROTO_VERSION 即丢弃，
    //   历史上用于兼容旧包的 isV2Packet() 已删除）

    // ═══════ 编解码 ═══════

    fun encodeHeader(header: PacketHeader, buf: ByteArray) {
        require(buf.size >= HEADER_SIZE)
        buf[0] = PROTO_VERSION
        buf[1] = (((if (header.isAudio) 1 else 0) and 0x01) shl 7
                or ((header.codec.toInt() and 0x07) shl 4)
                or (header.sampleRate.toInt() and 0x0F)).toByte()
        buf[2] = header.msgType
        buf[3] = (header.payloadLen shr 8).toByte()
        buf[4] = header.payloadLen.toByte()
        buf[5] = header.bitrate
        buf[6] = header.seqNum
        System.arraycopy(header.deviceId, 0, buf, 7, DEVICE_ID_SIZE)
    }

    fun decodeHeader(buf: ByteArray): PacketHeader? {
        if (buf.size < HEADER_SIZE) return null
        if (buf[0] != PROTO_VERSION) return null
        val b1 = buf[1].toInt() and 0xFF
        val isAudio = (b1 shr 7) and 0x01 == 1
        val codec = (b1 shr 4) and 0x07
        val sampleRate = (b1 and 0x0F).toByte()
        if (sampleRate > SAMPLE_RATE_48K) return null
        val msgType = buf[2]
        if (msgType < 0 || msgType > TYPE_CONNECT_ACK) return null
        val payloadLen = ((buf[3].toInt() and 0xFF) shl 8) or (buf[4].toInt() and 0xFF)
        if (payloadLen > MAX_PAYLOAD) return null
        val bitrate = buf[5]
        val seqNum = buf[6]
        val deviceId = ByteArray(DEVICE_ID_SIZE)
        System.arraycopy(buf, 7, deviceId, 0, DEVICE_ID_SIZE)
        return PacketHeader(
            version = PROTO_VERSION,
            isAudio = isAudio,
            codec = codec.toByte(),
            sampleRate = sampleRate,
            msgType = msgType,
            payloadLen = payloadLen,
            bitrate = bitrate,
            seqNum = seqNum,
            deviceId = deviceId
        )
    }

    /** 构建完整包（分配新数组） */
    fun buildPacket(
        isAudio: Boolean, msgType: Byte, sampleRate: Byte, seqNum: Byte,
        deviceId: ByteArray, payload: ByteArray, bitrate: Byte = BITRATE_AUTO
    ): ByteArray {
        val len = minOf(payload.size, MAX_PAYLOAD)
        val packet = ByteArray(HEADER_SIZE + len)
        val header = PacketHeader(
            isAudio = isAudio,
            msgType = msgType,
            sampleRate = sampleRate,
            seqNum = seqNum,
            payloadLen = len,
            bitrate = bitrate,
            deviceId = deviceId.copyOf()
        )
        encodeHeader(header, packet)
        System.arraycopy(payload, 0, packet, HEADER_SIZE, len)
        return packet
    }

    /** 在预分配缓冲区写入音频数据协议头（与 encoderEncodeTo 搭配使用，完全零分配）*/
    fun writeAudioHeader(dest: ByteArray, headerOffset: Int, payloadLen: Int, sampleRate: Byte, seqNum: Byte, deviceId: ByteArray, bitrate: Byte = BITRATE_AUTO): Int {
        require(headerOffset + HEADER_SIZE + payloadLen <= dest.size) { "目标缓冲区不足" }
        dest[headerOffset] = PROTO_VERSION
        dest[headerOffset + 1] = ((1 shl 7) or ((1 and 0x07) shl 4) or (sampleRate.toInt() and 0x0F)).toByte()
        dest[headerOffset + 2] = TYPE_DATA
        dest[headerOffset + 3] = (payloadLen shr 8).toByte()
        dest[headerOffset + 4] = payloadLen.toByte()
        dest[headerOffset + 5] = bitrate
        dest[headerOffset + 6] = seqNum
        System.arraycopy(deviceId, 0, dest, headerOffset + 7, DEVICE_ID_SIZE)
        return HEADER_SIZE + payloadLen
    }

    // ═══════ 工具函数 ═══════

    fun sampleRateToHz(sr: Byte): Int = when (sr) {
        SAMPLE_RATE_8K -> 8000; SAMPLE_RATE_12K -> 12000
        SAMPLE_RATE_16K -> 16000; SAMPLE_RATE_24K -> 24000
        SAMPLE_RATE_48K -> 48000; else -> 48000
    }

    fun hzToSampleRate(hz: Int): Byte = when (hz) {
        8000 -> SAMPLE_RATE_8K; 12000 -> SAMPLE_RATE_12K
        16000 -> SAMPLE_RATE_16K; 24000 -> SAMPLE_RATE_24K
        48000 -> SAMPLE_RATE_48K; else -> SAMPLE_RATE_48K
    }

    /**
     * 生成 8 字节设备 ID。
     *
     * 设备 ID 是 P2P 独占唯一的准入凭据，必须使用系统 CSPRNG。
     * 历史实现用 `System.nanoTime()` 位运算拼凑，同网段主机可推算候选区间。
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

    // ═══════ 重排缓冲 ═══════

    /**
     * 轻量重排缓冲，语义与 Rust 端 `protocol.rs::ReorderBuffer` 对齐：
     * - 首包作为基准（发送端从 seq=1 起步时不会被误判为乱序）
     * - 落后 `nextSeq` 不超过 `maxLate` 视为迟到/重复，直接丢弃
     * - 窗口内乱序先入槽，等 expected 到达后统一排空
     * - 大步超前视为跳变（丢包/发送端重启），重置基准并立即输出
     *
     * PC → 手机的返回流此前完全没有序号处理：乱序包会被直接解码成噪音。
     *
     * @param sink `(sampleRate, buffer, offset, length)`；入槽后的包会拷贝到独立数组，
     *             连序包直接引用原始接收缓冲，不产生拷贝。
     */
    class ReorderBuffer(
        private val maxLate: Int = 8,
        private val window: Int = 2,
        private val sink: (Byte, ByteArray, Int, Int) -> Unit,
    ) {
        private val slotSeq = IntArray(window) { -1 }
        private val slotRate = ByteArray(window)
        private val slotData = arrayOfNulls<ByteArray>(window)
        private var writeIdx = 0
        private var nextSeq = 0
        private var lateStreak = 0
        private var started = false

        fun reset() {
            for (i in 0 until window) {
                slotSeq[i] = -1
                slotData[i] = null
            }
            writeIdx = 0
            nextSeq = 0
            lateStreak = 0
            started = false
        }

        fun insert(seq: Int, sampleRate: Byte, buf: ByteArray, offset: Int, length: Int) {
            if (length <= 0) return
            val s = seq and 0xFF

            if (!started) {
                started = true
                nextSeq = (s + 1) and 0xFF
                sink(sampleRate, buf, offset, length)
                return
            }

            val dist = (s - nextSeq) and 0xFF
            if (dist >= 256 - maxLate) {
                lateStreak++
                if (lateStreak < maxLate * 4) return
                // 连续迟到 → 实为发送端大跨度回退，强制重同步
                lateStreak = 0
                clearSlots()
                nextSeq = (s + 1) and 0xFF
                sink(sampleRate, buf, offset, length)
                return
            }
            lateStreak = 0

            when {
                dist == 0 -> {
                    sink(sampleRate, buf, offset, length)
                    nextSeq = (nextSeq + 1) and 0xFF
                }
                dist < window -> {
                    slotSeq[writeIdx] = s
                    slotRate[writeIdx] = sampleRate
                    slotData[writeIdx] = buf.copyOfRange(offset, offset + length)
                    writeIdx = (writeIdx + 1) % window
                }
                else -> {
                    clearSlots()
                    nextSeq = (s + 1) and 0xFF
                    sink(sampleRate, buf, offset, length)
                    return
                }
            }

            var progressed = true
            while (progressed) {
                progressed = false
                for (i in 0 until window) {
                    if (slotSeq[i] == nextSeq) {
                        val data = slotData[i] ?: continue
                        sink(slotRate[i], data, 0, data.size)
                        slotSeq[i] = -1
                        slotData[i] = null
                        nextSeq = (nextSeq + 1) and 0xFF
                        progressed = true
                        break
                    }
                }
            }
        }

        private fun clearSlots() {
            for (i in 0 until window) {
                slotSeq[i] = -1
                slotData[i] = null
            }
            writeIdx = 0
        }
    }
}
