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
     * 构建UDP音频包
     * @param bitrate 编码码率(kbps)，0=auto让接收端使用默认码率显示
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
}
