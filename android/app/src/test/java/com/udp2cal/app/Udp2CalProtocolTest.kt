package com.udp2cal.app

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 协议层 JVM 单元测试。
 *
 * 覆盖三类易错点：
 *   1. 包头解析的边界/非法输入（这些分支一旦漏判就是越界读）
 *   2. 反向流的乱序/重复/跳变时序（与 Rust 端 ReorderBuffer 行为对齐）
 *   3. 跨端常量一致性（Kotlin 是手动对齐实现，必须逐项锁死）
 *
 * 运行：`./gradlew :app:testDebugUnitTest`
 */
class Udp2CalProtocolTest {

    private val deviceId = byteArrayOf(1, 2, 3, 4, 5, 6, 7, 8)

    // ═══════ 编解码 ═══════

    @Test
    fun audioPacket_roundTrip() {
        val payload = ByteArray(100) { 0x5A }
        val packet = Udp2CalProtocol.buildPacket(
            isAudio = true,
            msgType = Udp2CalProtocol.TYPE_DATA,
            sampleRate = Udp2CalProtocol.SAMPLE_RATE_48K,
            seqNum = 42.toByte(),
            deviceId = deviceId,
            payload = payload,
            bitrate = 255.toByte()
        )
        assertEquals(Udp2CalProtocol.HEADER_SIZE + payload.size, packet.size)

        val hdr = Udp2CalProtocol.decodeHeader(packet)!!
        assertTrue(hdr.isAudio)
        assertEquals(Udp2CalProtocol.TYPE_DATA.toInt(), hdr.msgType.toInt())
        assertEquals(Udp2CalProtocol.SAMPLE_RATE_48K.toInt(), hdr.sampleRate.toInt())
        assertEquals(42, hdr.seqNum.toInt())
        assertEquals(payload.size, hdr.payloadLen)
        assertEquals(255, hdr.bitrate.toInt() and 0xFF)
        assertArrayEquals(deviceId, hdr.deviceId)
    }

    @Test
    fun controlPacket_hasNoPayload() {
        val packet = Udp2CalProtocol.buildPacket(
            isAudio = false,
            msgType = Udp2CalProtocol.TYPE_CONNECT,
            sampleRate = 0.toByte(),
            seqNum = 0.toByte(),
            deviceId = deviceId,
            payload = ByteArray(0)
        )
        assertEquals(Udp2CalProtocol.HEADER_SIZE, packet.size)
        val hdr = Udp2CalProtocol.decodeHeader(packet)!!
        assertFalse(hdr.isAudio)
        assertEquals(Udp2CalProtocol.TYPE_CONNECT.toInt(), hdr.msgType.toInt())
        assertEquals(0, hdr.payloadLen)
    }

    // ═══════ 非法包拒绝 ═══════

    @Test
    fun rejects_shortBuffer() {
        assertNull(Udp2CalProtocol.decodeHeader(ByteArray(Udp2CalProtocol.HEADER_SIZE - 1)))
    }

    @Test
    fun rejects_unknownVersion() {
        val buf = ByteArray(Udp2CalProtocol.HEADER_SIZE)
        buf[0] = 0x03
        assertNull(Udp2CalProtocol.decodeHeader(buf))
    }

    @Test
    fun rejects_invalidSampleRate() {
        val buf = ByteArray(Udp2CalProtocol.HEADER_SIZE)
        buf[0] = Udp2CalProtocol.PROTO_VERSION
        buf[1] = 0x0F // sample_rate = 15
        assertNull(Udp2CalProtocol.decodeHeader(buf))
    }

    @Test
    fun rejects_invalidMsgType() {
        val buf = ByteArray(Udp2CalProtocol.HEADER_SIZE)
        buf[0] = Udp2CalProtocol.PROTO_VERSION
        buf[2] = 0xFF.toByte()
        assertNull(Udp2CalProtocol.decodeHeader(buf))
    }

    @Test
    fun rejects_oversizedPayload() {
        val buf = ByteArray(Udp2CalProtocol.HEADER_SIZE)
        buf[0] = Udp2CalProtocol.PROTO_VERSION
        buf[2] = Udp2CalProtocol.TYPE_DATA
        buf[3] = 0x06
        buf[4] = 0x00 // 声明 1536 > MAX_PAYLOAD
        assertNull(Udp2CalProtocol.decodeHeader(buf))
    }

    // ═══════ 与 Rust 端的一致性 ═══════

    @Test
    fun constants_matchRustImplementation() {
        assertEquals(15, Udp2CalProtocol.HEADER_SIZE)
        assertEquals(1472, Udp2CalProtocol.MAX_PAYLOAD)
        assertEquals(2.toByte(), Udp2CalProtocol.PROTO_VERSION)
        assertEquals(44043, Udp2CalProtocol.DISCOVER_PORT)
        assertEquals(0, Udp2CalProtocol.TYPE_DATA.toInt())
        assertEquals(4, Udp2CalProtocol.TYPE_CONNECT_ACK.toInt())
        assertEquals(8000, Udp2CalProtocol.sampleRateToHz(Udp2CalProtocol.SAMPLE_RATE_8K))
        assertEquals(48000, Udp2CalProtocol.sampleRateToHz(Udp2CalProtocol.SAMPLE_RATE_48K))
        // 未知采样率必须回落到 48k（与 Rust sample_rate_to_hz 一致）
        assertEquals(48000, Udp2CalProtocol.sampleRateToHz(9))
    }

    @Test
    fun deviceId_isEightBytesAndNotAllZero() {
        val id = Udp2CalProtocol.generateDeviceId()
        assertEquals(Udp2CalProtocol.DEVICE_ID_SIZE, id.size)
        assertTrue(id.any { it != 0.toByte() })
        assertFalse(Udp2CalProtocol.generateDeviceId().contentEquals(id))
    }

    @Test
    fun deviceIdToString_isLowerCaseHex() {
        val id = byteArrayOf(0x0A, 0x1B, 0x2C, 0x3D, 0x4E, 0x5F, 0x60, 0x7F)
        assertEquals("0a1b2c3d4e5f607f", Udp2CalProtocol.deviceIdToString(id))
    }

    // ═══════ 重排缓冲（与 Rust 端 ReorderBuffer 行为对齐） ═══════

    /** 收集被放行包的标记字节（测试里用载荷首字节代表序号） */
    private class Collector {
        val tags = mutableListOf<Int>()
        val sink: (Byte, ByteArray, Int, Int) -> Unit = { _, buf, off, _ ->
            tags.add(buf[off].toInt() and 0xFF)
        }
    }

    private fun run(rb: Udp2CalProtocol.ReorderBuffer, seq: Int, tag: Int) {
        val buf = byteArrayOf(tag.toByte())
        rb.insert(seq, Udp2CalProtocol.SAMPLE_RATE_48K, buf, 0, buf.size)
    }

    @Test
    fun reorder_inOrder_passesThrough() {
        val c = Collector()
        val rb = Udp2CalProtocol.ReorderBuffer(sink = c.sink)
        run(rb, 0, 10); run(rb, 1, 11); run(rb, 2, 12)
        assertEquals(listOf(10, 11, 12), c.tags)
    }

    @Test
    fun reorder_firstPacketIsBaseline() {
        // 发送端从 seq=1 起步时，首包必须立即输出，不能等一个永远不会来的 0
        val c = Collector()
        val rb = Udp2CalProtocol.ReorderBuffer(sink = c.sink)
        run(rb, 1, 21); run(rb, 2, 22)
        assertEquals(listOf(21, 22), c.tags)
    }

    @Test
    fun reorder_outOfOrder_isReassembled() {
        val c = Collector()
        val rb = Udp2CalProtocol.ReorderBuffer(sink = c.sink)
        run(rb, 0, 30)          // 基准
        run(rb, 2, 32)          // 入槽等待 1
        assertEquals(listOf(30), c.tags)
        run(rb, 1, 31)          // 触发排空：1, 2
        assertEquals(listOf(30, 31, 32), c.tags)
    }

    @Test
    fun reorder_duplicateIsDropped() {
        val c = Collector()
        val rb = Udp2CalProtocol.ReorderBuffer(sink = c.sink)
        run(rb, 0, 40); run(rb, 1, 41); run(rb, 1, 99); run(rb, 2, 42)
        assertEquals(listOf(40, 41, 42), c.tags)
    }

    @Test
    fun reorder_bigJumpResyncs() {
        val c = Collector()
        val rb = Udp2CalProtocol.ReorderBuffer(sink = c.sink)
        run(rb, 0, 50)
        run(rb, 200, 51)        // 大步超前 → 重置基准并立即输出
        run(rb, 201, 52)
        assertEquals(listOf(50, 51, 52), c.tags)
    }

    @Test
    fun reorder_lateStreakForcesResync() {
        val c = Collector()
        val rb = Udp2CalProtocol.ReorderBuffer(sink = c.sink)
        run(rb, 100, 60); run(rb, 101, 61)
        // nextSeq = 102，seq=94 属于"落后 8"：单次应被丢弃
        repeat(31) { run(rb, 94, 94) }
        assertEquals(listOf(60, 61), c.tags)
        // 连续迟到达到阈值（maxLate × 4 = 32）→ 判定为发送端回退，强制重同步
        run(rb, 94, 94)
        assertEquals(listOf(60, 61, 94), c.tags)
    }

    @Test
    fun reorder_seqWrapsAround() {
        val c = Collector()
        val rb = Udp2CalProtocol.ReorderBuffer(sink = c.sink)
        for (seq in 250..255) run(rb, seq, seq)
        for (seq in 0..3) run(rb, seq, seq)   // 序号回绕
        assertEquals(listOf(250, 251, 252, 253, 254, 255, 0, 1, 2, 3), c.tags)
    }
}
