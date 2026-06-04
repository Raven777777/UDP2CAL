package com.udp2mic.app

/**
 * UDP2Mic UDP私有协议编解码 (Kotlin实现)
 * v1: 6字节包头 (4字节v0包头 + 码率 + 扩展标记) + Opus负载
 * 与Rust端 protocol/protocol.rs 100%对齐
 */
object Udp2MicProtocol {
    const val HEADER_SIZE = 6
    const val MAX_PAYLOAD = 1472
    const val MAX_PACKET = HEADER_SIZE + MAX_PAYLOAD
    // 采样率常量
    const val SAMPLE_RATE_8K: Byte = 0
    const val SAMPLE_RATE_12K: Byte = 1
    const val SAMPLE_RATE_16K: Byte = 2
    const val SAMPLE_RATE_24K: Byte = 3
    const val SAMPLE_RATE_48K: Byte = 4

    // 码率常量
    const val BITRATE_AUTO: Byte = 0  // 让接收端自动选择默认码率

    fun sampleRateToHz(sr: Byte): Int = when (sr) {
        SAMPLE_RATE_8K -> 8000
        SAMPLE_RATE_12K -> 12000
        SAMPLE_RATE_24K -> 24000
        SAMPLE_RATE_48K -> 48000
        else -> 16000
    }

    fun hzToSampleRate(hz: Int): Byte = when (hz) {
        8000 -> SAMPLE_RATE_8K
        12000 -> SAMPLE_RATE_12K
        16000 -> SAMPLE_RATE_16K
        24000 -> SAMPLE_RATE_24K
        48000 -> SAMPLE_RATE_48K
        else -> SAMPLE_RATE_48K
    }

    data class PacketHeader(
        val version: Byte = 1,
        val codec: Byte = 1,
        val sampleRate: Byte = SAMPLE_RATE_48K,
        val seqNum: Byte = 0,
        val payloadLen: Int = 0,
        val bitrate: Byte = BITRATE_AUTO,   // kbps, 0=auto
        val flags: Byte = 0                  // 保留/扩展
    )

    fun encodeHeader(header: PacketHeader, buf: ByteArray) {
        require(buf.size >= HEADER_SIZE)
        buf[0] = (((header.version.toInt() and 0x01) shl 7)
                or ((header.codec.toInt() and 0x07) shl 4)
                or (header.sampleRate.toInt() and 0x0F)).toByte()
        buf[1] = header.seqNum
        buf[2] = (header.payloadLen shr 8).toByte()
        buf[3] = header.payloadLen.toByte()
        buf[4] = header.bitrate
        buf[5] = header.flags
    }

    fun decodeHeader(buf: ByteArray): PacketHeader? {
        if (buf.size < HEADER_SIZE) return null
        val b0 = buf[0].toInt() and 0xFF
        val version = (b0 shr 7) and 0x01
        if (version != 1) return null
        val codec = (b0 shr 4) and 0x07
        if (codec != 1) return null
        val sampleRate = (b0 and 0x0F).toByte()
        if (sampleRate > SAMPLE_RATE_48K) return null
        val seqNum = buf[1]
        val payloadLen = ((buf[2].toInt() and 0xFF) shl 8) or (buf[3].toInt() and 0xFF)
        if (payloadLen > MAX_PAYLOAD) return null
        val bitrate = buf[4]
        val flags = buf[5]
        return PacketHeader(
            version = version.toByte(),
            codec = codec.toByte(),
            sampleRate = sampleRate,
            seqNum = seqNum,
            payloadLen = payloadLen,
            bitrate = bitrate,
            flags = flags
        )
    }

    /**
     * 构建UDP音频包（分配新数组）
     */
    fun buildPacket(sampleRate: Byte, seqNum: Byte, payload: ByteArray, bitrate: Byte = BITRATE_AUTO): ByteArray {
        val len = minOf(payload.size, MAX_PAYLOAD)
        val packet = ByteArray(HEADER_SIZE + len)
        val header = PacketHeader(
            sampleRate = sampleRate,
            seqNum = seqNum,
            payloadLen = len,
            bitrate = bitrate
        )
        encodeHeader(header, packet)
        System.arraycopy(payload, 0, packet, HEADER_SIZE, len)
        return packet
    }

    /**
     * 构建UDP音频包（写入预分配缓冲区，零分配）
     * @return 写入的字节数
     */
    fun buildPacketTo(dest: ByteArray, offset: Int, sampleRate: Byte, seqNum: Byte, payload: ByteArray, bitrate: Byte = BITRATE_AUTO): Int {
        val len = minOf(payload.size, MAX_PAYLOAD)
        require(offset + HEADER_SIZE + len <= dest.size) { "目标缓冲区不足" }
        dest[offset] = (((1 and 0x01) shl 7) or ((1 and 0x07) shl 4) or (sampleRate.toInt() and 0x0F)).toByte()
        dest[offset + 1] = seqNum
        dest[offset + 2] = (len shr 8).toByte()
        dest[offset + 3] = len.toByte()
        dest[offset + 4] = bitrate
        dest[offset + 5] = 0
        System.arraycopy(payload, 0, dest, offset + HEADER_SIZE, len)
        return HEADER_SIZE + len
    }

    /**
     * 在预分配缓冲区写入 UDP 包头（负载已在 dest[payloadOffset] 中）
     * 与 encoderEncodeTo 搭配使用，实现 Pipeline 完全零分配
     * @return 包头 + 负载总字节数
     */
    fun writeHeader(dest: ByteArray, headerOffset: Int, payloadLen: Int, sampleRate: Byte, seqNum: Byte, bitrate: Byte = BITRATE_AUTO): Int {
        require(headerOffset + HEADER_SIZE + payloadLen <= dest.size) { "目标缓冲区不足" }
        dest[headerOffset] = (((1 and 0x01) shl 7) or ((1 and 0x07) shl 4) or (sampleRate.toInt() and 0x0F)).toByte()
        dest[headerOffset + 1] = seqNum
        dest[headerOffset + 2] = (payloadLen shr 8).toByte()
        dest[headerOffset + 3] = payloadLen.toByte()
        dest[headerOffset + 4] = bitrate
        dest[headerOffset + 5] = 0
        return HEADER_SIZE + payloadLen
    }
}
