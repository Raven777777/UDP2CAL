package com.udp2cal.app

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 协议层 JVM 单元测试（v3）。
 *
 * 覆盖三类易错点：
 *   1. 控制头/RTP 头的字节级正确性（与 Rust 端共用同一组向量）
 *   2. 抖动缓冲的时序（乱序、重复、迟到、缺口、重同步、回绕）
 *   3. 跨端常量一致性（Kotlin 是镜像实现，必须逐项锁死）
 *
 * 运行：`./gradlew :app:testDebugUnitTest`
 */
class Udp2CalProtocolTest {

    private val deviceId = byteArrayOf(1, 2, 3, 4, 5, 6, 7, 8)

    private fun bytes(vararg v: Int): ByteArray = ByteArray(v.size) { v[it].toByte() }

    // ═══════ 控制面 ═══════

    @Test
    fun controlHeader_matchesSpecVectorB4() {
        val packet = Udp2CalProtocol.buildControl(
            Udp2CalProtocol.TYPE_CONNECT, deviceId, ByteArray(15)
        )
        val expected = bytes(
            0x03, 0x00, 0x01, 0x00, 0x0F, 0x00, 0x00,
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08
        )
        assertArrayEquals(expected, packet.copyOfRange(0, Udp2CalProtocol.CONTROL_HEADER_SIZE))
    }

    @Test
    fun connectPayload_matchesSpecVectorB5() {
        val p = Udp2CalProtocol.ConnectPayload(
            reversePort = 50000,
            forwardSsrc = 0x11223344,
            complexity = 3,
            signal = 1,
            bandwidth = 4,
            opusFlags = Udp2CalProtocol.OPUS_FLAG_VBR or Udp2CalProtocol.OPUS_FLAG_DTX,
            bitrateKbps = 0,
            packetLoss = 5,
        )
        val expected = bytes(
            0xC3, 0x50, 0x11, 0x22, 0x33, 0x44, 0x6F,
            0x0A, 0x03, 0x01, 0x04, 0x03, 0x00, 0x00, 0x05
        )
        assertArrayEquals(expected, p.encode())
        assertTrue(p.isValid())

        val decoded = Udp2CalProtocol.ConnectPayload.decode(p.encode(), 0, 15)!!
        assertEquals(50000, decoded.reversePort)
        assertEquals(0x11223344, decoded.forwardSsrc)
        assertEquals(3, decoded.complexity)
        assertEquals(1, decoded.signal)
        assertEquals(4, decoded.bandwidth)
    }

    @Test
    fun connectAck_matchesSpecVectorB6() {
        val raw = bytes(0xAA, 0xBB, 0xCC, 0xDD, 0x6F, 0x0A)
        val ack = Udp2CalProtocol.ConnectAckPayload.decode(raw, 0, raw.size)!!
        assertEquals(0xAABBCCDD.toInt(), ack.reverseSsrc)
        assertEquals(111, ack.payloadType)
        assertEquals(10, ack.frameMs)
        assertTrue(ack.isValid())
    }

    @Test
    fun connectPayload_rejectsBadParams() {
        assertNull(Udp2CalProtocol.ConnectPayload.decode(ByteArray(14), 0, 14))
        assertNull(Udp2CalProtocol.ConnectPayload.decode(ByteArray(16), 0, 16))
        assertFalse(Udp2CalProtocol.ConnectPayload(1, 1, payloadType = 110).isValid())
        assertFalse(Udp2CalProtocol.ConnectPayload(1, 1, frameMs = 20).isValid())
        assertFalse(Udp2CalProtocol.ConnectPayload(1, 0).isValid())
    }

    @Test
    fun ackPayload_rejectsBadParams() {
        assertNull(Udp2CalProtocol.ConnectAckPayload.decode(bytes(0, 0, 0, 1, 0x60, 0x0A), 0, 6))
        assertNull(Udp2CalProtocol.ConnectAckPayload.decode(bytes(0, 0, 0, 1, 0x6F, 0x14), 0, 6))
        assertNull(Udp2CalProtocol.ConnectAckPayload.decode(ByteArray(5), 0, 5))
    }

    @Test
    fun discoverReply_parsesListenPortAndName() {
        val payload = bytes(0xAC, 0x0C) + "UDP2CAL".toByteArray()
        val reply = Udp2CalProtocol.DiscoverReply.decode(payload, 0, payload.size)!!
        assertEquals(44044, reply.listenPort)
        assertEquals("UDP2CAL", reply.deviceName)
        assertNull(Udp2CalProtocol.DiscoverReply.decode(bytes(0x01), 0, 1))
        // listen_port = 0 视为非法
        assertNull(Udp2CalProtocol.DiscoverReply.decode(bytes(0, 0), 0, 2))
    }

    @Test
    fun controlHeader_rejectsMismatchedLength() {
        val packet = Udp2CalProtocol.buildControl(
            Udp2CalProtocol.TYPE_CONNECT, deviceId, ByteArray(15)
        )
        assertNull(Udp2CalProtocol.parseControl(packet, packet.size - 1))
        assertNotNull(Udp2CalProtocol.parseControl(packet, packet.size))

        val bad = packet.copyOf()
        bad[0] = 0x02
        assertNull(Udp2CalProtocol.parseControl(bad, bad.size))

        val big = ByteArray(Udp2CalProtocol.CONTROL_HEADER_SIZE)
        big[0] = Udp2CalProtocol.CONTROL_VERSION
        big[3] = 0x02
        big[4] = 0x01 // 声明 513 > 512
        assertNull(Udp2CalProtocol.parseControl(big, big.size))
    }

    @Test
    fun controlTypeZero_isNotKnown() {
        val packet = Udp2CalProtocol.buildControl(Udp2CalProtocol.TYPE_DATA, deviceId, ByteArray(0))
        val hdr = Udp2CalProtocol.parseControl(packet, packet.size)!!
        assertEquals(0, hdr.msgType)
        assertFalse(hdr.isKnownType)

        val reserved = Udp2CalProtocol.buildControl(9.toByte(), deviceId, ByteArray(0))
        assertFalse(Udp2CalProtocol.parseControl(reserved, reserved.size)!!.isKnownType)
    }

    // ═══════ RTP ═══════

    @Test
    fun rtpHeader_matchesSpecVectorB1() {
        val buf = ByteArray(Udp2CalProtocol.RTP_MAX_PACKET)
        Udp2CalProtocol.writeRtpHeader(buf, 0, true, 0x1234, 0x000004B0, 0xDEADBEEF.toInt())
        buf[12] = 0xF8.toByte()
        buf[13] = 0xFF.toByte()
        buf[14] = 0xFE.toByte()

        val expected = bytes(
            0x80, 0xEF, 0x12, 0x34, 0x00, 0x00, 0x04, 0xB0,
            0xDE, 0xAD, 0xBE, 0xEF,
            0xF8, 0xFF, 0xFE
        )
        assertArrayEquals(expected, buf.copyOfRange(0, 15))

        val p = Udp2CalProtocol.parseRtp(buf, 15)!!
        assertTrue(p.marker)
        assertEquals(0x1234, p.seq)
        assertEquals(0x4B0L, p.timestamp)
        assertEquals(0xDEADBEEF.toInt(), p.ssrc)
        assertEquals(3, p.payloadLength)

        // M=0 时第二字节为 0x6F
        Udp2CalProtocol.writeRtpHeader(buf, 0, false, 1, 0, 1)
        assertEquals(0x6F, buf[1].toInt() and 0xFF)
    }

    @Test
    fun rtp_rejectsInvalidPackets() {
        val buf = ByteArray(Udp2CalProtocol.RTP_MAX_PACKET)
        Udp2CalProtocol.writeRtpHeader(buf, 0, false, 1, 0, 12345)
        buf[12] = 0x01
        assertNotNull(Udp2CalProtocol.parseRtp(buf, 13))

        val bad = buf.copyOf()
        bad[0] = 0x90.toByte() // X=1
        assertNull(Udp2CalProtocol.parseRtp(bad, 13))

        val badPt = buf.copyOf()
        badPt[1] = 0x60 // PT=96
        assertNull(Udp2CalProtocol.parseRtp(badPt, 13))

        val noSsrc = buf.copyOf()
        for (i in 8..11) noSsrc[i] = 0
        assertNull(Udp2CalProtocol.parseRtp(noSsrc, 13))

        assertNull(Udp2CalProtocol.parseRtp(buf, 12))
        assertNull(Udp2CalProtocol.parseRtp(buf, Udp2CalProtocol.RTP_MAX_PACKET + 1))
    }

    @Test
    fun packetSizeLimits_matchSpec() {
        assertEquals(12, Udp2CalProtocol.RTP_HEADER_SIZE)
        assertEquals(1275, Udp2CalProtocol.OPUS_MAX_PAYLOAD)
        assertEquals(1287, Udp2CalProtocol.RTP_MAX_PACKET)
        assertEquals(1315, Udp2CalProtocol.UDP_IP_MAX_PACKET)
        assertEquals(15, Udp2CalProtocol.CONTROL_HEADER_SIZE)
        assertEquals(512, Udp2CalProtocol.CONTROL_MAX_PAYLOAD)
    }

    // ═══════ 扩展序号 / 时间戳 ═══════

    @Test
    fun extendSeq_handlesWrap() {
        assertEquals(65536L, Udp2CalProtocol.extendSeq(65535, 0))
        assertEquals(65537L, Udp2CalProtocol.extendSeq(65535, 1))
        assertEquals(65534L, Udp2CalProtocol.extendSeq(65535, 65534))
        assertEquals(97L, Udp2CalProtocol.extendSeq(100, 97))
    }

    @Test
    fun extendTs_handlesWrap() {
        assertEquals(0x1000000E0L, Udp2CalProtocol.extendTs(0xFFFFFF00L, 0xE0L))
        assertEquals(0x1000001DFL, Udp2CalProtocol.extendTs(0xFFFFFFFFL, 0x1DFL))
        assertEquals(960L, Udp2CalProtocol.extendTs(480L, 960L))
    }

    // ═══════ 抖动缓冲 ═══════

    private fun insert(
        jb: Udp2CalProtocol.JitterBuffer,
        seq: Int,
        ts: Long,
        marker: Boolean,
        tag: Int
    ): Boolean {
        val buf = bytes(tag)
        return jb.insert(seq, ts, marker, buf, 0, 1)
    }

    private fun pullTags(
        jb: Udp2CalProtocol.JitterBuffer,
        playhead: Long,
        count: Int
    ): List<Int> {
        val out = ByteArray(Udp2CalProtocol.OPUS_MAX_PAYLOAD)
        val tags = mutableListOf<Int>()
        repeat(count) {
            when (jb.pull(playhead, out)) {
                Udp2CalProtocol.Pull.PACKET -> tags.add(out[0].toInt() and 0xFF)
                Udp2CalProtocol.Pull.PLC -> tags.add(0xFE)
                Udp2CalProtocol.Pull.SILENCE -> tags.add(0xFF)
                else -> return tags
            }
        }
        return tags
    }

    @Test
    fun jitter_prebuffersFourFrames() {
        val jb = Udp2CalProtocol.JitterBuffer()
        insert(jb, 100, 4800L, true, 1)
        assertEquals(Udp2CalProtocol.Pull.WAIT, jb.pull(4800L, ByteArray(1275)))
        insert(jb, 101, 5280L, false, 2)
        insert(jb, 102, 5760L, false, 3)
        assertEquals(Udp2CalProtocol.Pull.WAIT, jb.pull(4800L, ByteArray(1275)))
        insert(jb, 103, 6240L, false, 4)
        assertEquals(40, jb.waterMs())
        assertEquals(listOf(1, 2, 3, 4), pullTags(jb, 4800L, 4))
        assertEquals(0, jb.waterMs())
    }

    @Test
    fun jitter_reassemblesOutOfOrder() {
        val jb = Udp2CalProtocol.JitterBuffer()
        insert(jb, 0, 0L, true, 10)
        insert(jb, 2, 960L, false, 12)
        insert(jb, 3, 1440L, false, 13)
        insert(jb, 1, 480L, false, 11)
        assertEquals(listOf(10, 11, 12, 13), pullTags(jb, 0L, 4))
    }

    @Test
    fun jitter_dropsDuplicateAndLate() {
        val jb = Udp2CalProtocol.JitterBuffer()
        for (i in 0 until 4) insert(jb, i, 480L * i, i == 0, 20 + i)
        assertEquals(listOf(20, 21), pullTags(jb, 0L, 2)) // nextTs = 960

        assertFalse(insert(jb, 2, 960L, false, 99)) // 重复
        assertFalse(insert(jb, 1, 480L, false, 98)) // 迟到
        assertEquals(listOf(22, 23), pullTags(jb, 960L, 2))
    }

    @Test
    fun jitter_plcOnlyWhenPlayheadReachesGap() {
        val jb = Udp2CalProtocol.JitterBuffer()
        insert(jb, 0, 0L, true, 1)
        insert(jb, 1, 480L, false, 2)
        insert(jb, 2, 960L, false, 3)
        insert(jb, 3, 1440L, false, 4)
        // 1920 丢失，2400/2880 正常
        insert(jb, 5, 2400L, false, 6)
        insert(jb, 6, 2880L, false, 7)

        assertEquals(listOf(1, 2, 3, 4), pullTags(jb, 0L, 4)) // nextTs = 1920
        assertEquals(Udp2CalProtocol.Pull.WAIT, jb.pull(0L, ByteArray(1275)))
        // 播放头走到 20 ms（PLC 提前量）→ 缺口到期
        assertEquals(Udp2CalProtocol.Pull.PLC, jb.pull(960L, ByteArray(1275)))
        assertEquals(Udp2CalProtocol.Pull.PACKET, jb.pull(960L, ByteArray(1275)))
    }

    @Test
    fun jitter_plcRunCappedThenSilence() {
        val jb = Udp2CalProtocol.JitterBuffer()
        for (i in 0 until 4) insert(jb, i, 480L * i, i == 0, i)
        pullTags(jb, 0L, 4)

        var plc = 0
        var silence = 0
        for (step in 0 until 30L) {
            when (jb.pull(480L * step, ByteArray(1275))) {
                Udp2CalProtocol.Pull.PLC -> plc++
                Udp2CalProtocol.Pull.SILENCE -> silence++
                else -> {}
            }
        }
        assertEquals(Udp2CalProtocol.PLC_MAX_MS / Udp2CalProtocol.OPUS_FRAME_MS, plc)
        assertTrue(silence > 0)
        assertTrue(jb.playing)
    }

    @Test
    fun jitter_markerWithBigJumpResyncs() {
        val jb = Udp2CalProtocol.JitterBuffer()
        for (i in 0 until 4) insert(jb, i, 480L * i, i == 0, i)
        val epoch = jb.epoch
        assertTrue(insert(jb, 50, 480L * 50, true, 99))
        assertTrue(jb.epoch > epoch)
        assertFalse(jb.playing)
        assertEquals(1L, jb.resyncs)
        assertEquals(480L * 50, jb.nextTs)
    }

    @Test
    fun jitter_markerWithSmallJumpKeepsBuffer() {
        val jb = Udp2CalProtocol.JitterBuffer()
        for (i in 0 until 6) insert(jb, i, 480L * i, i == 0, i)
        pullTags(jb, 0L, 4)
        val epoch = jb.epoch
        // M=1 但时间戳只跳 20 ms → 不重置
        assertTrue(insert(jb, 6, 2880L, true, 99))
        assertEquals(epoch, jb.epoch)
        assertTrue(jb.playing)
    }

    @Test
    fun jitter_fullWindow_dropsNewestWithoutResync() {
        val jb = Udp2CalProtocol.JitterBuffer()
        for (i in 0 until 4) insert(jb, i, 480L * i, i == 0, i)
        pullTags(jb, 0L, 2) // nextSeq = 2，窗口 2..=65
        for (i in 4..(Udp2CalProtocol.MAX_REORDER_PACKETS + 1)) {
            insert(jb, i, 480L * i, false, i)
        }
        assertEquals(0L, jb.overflow)
        val epoch = jb.epoch

        // 序号 66 已超出窗口：丢弃这一包，不得重同步
        assertFalse(insert(jb, 66, 480L * 66, false, 66))
        assertEquals(1L, jb.overflow)
        assertEquals("缓冲不得被整块清空", epoch, jb.epoch)
        assertEquals(0L, jb.resyncs)

        // 已缓冲的连续帧照常播放
        assertEquals(listOf(2, 3), pullTags(jb, 480L, 2))
    }

    @Test
    fun jitter_handlesSeqAndTsWrap() {
        val jb = Udp2CalProtocol.JitterBuffer()
        val baseTs = 0xFFFFFF00L
        val baseSeq = 0xFFFE
        for (i in 0 until 8) {
            val ts = (baseTs + 480L * i) and 0xFFFFFFFFL
            val seq = (baseSeq + i) and 0xFFFF
            insert(jb, seq, ts, i == 0, i + 1)
        }
        assertEquals(baseTs, jb.nextTs)
        assertEquals((1..8).toList(), pullTags(jb, baseTs, 8))
        assertEquals(baseTs + 480L * 8, jb.nextTs)
    }

    // ═══════ 漂移校正与播放头 ═══════

    @Test
    fun drift_zeroInBand() {
        val d = Udp2CalProtocol.DriftController()
        d.update(60, 0L)
        for (i in 0 until 1000L) assertEquals(0, d.update(60, 480L * i))
    }

    @Test
    fun drift_keepsNormalWaterUntouched() {
        val d = Udp2CalProtocol.DriftController()
        var media = 0L
        d.update(60, media)
        repeat(600) {
            media += Udp2CalProtocol.DRIFT_INTERVAL_SAMPLES
            assertEquals(0, d.update(60, media))
        }
        assertEquals(0L, d.corrections)
    }

    @Test
    fun drift_doesNotTrimBelowFloor() {
        val d = Udp2CalProtocol.DriftController()
        var media = 0L
        d.update(150, media)
        repeat(600) {
            media += Udp2CalProtocol.DRIFT_INTERVAL_SAMPLES
            assertEquals(0, d.update(150, media))
        }
        assertEquals(0L, d.corrections)
    }

    @Test
    fun drift_drainsExcessWaterAtBoundedRate() {
        val d = Udp2CalProtocol.DriftController()
        var media = 0L
        d.update(400, media)
        var drops = 0
        repeat(20) {
            media += Udp2CalProtocol.DRIFT_INTERVAL_SAMPLES
            if (d.update(400, media) == Udp2CalProtocol.DRIFT_MAX_SAMPLES) drops++
        }
        assertEquals(20, drops)
    }

    @Test
    fun drift_drainsSlowlyNearFloor() {
        val d = Udp2CalProtocol.DriftController()
        var media = 0L
        d.update(220, media)
        var drops = 0
        repeat(20) {
            media += Udp2CalProtocol.DRIFT_INTERVAL_SAMPLES
            if (d.update(220, media) == Udp2CalProtocol.DRIFT_MAX_SAMPLES) drops++
        }
        assertEquals(5, drops)
    }

    @Test
    fun drift_insertsOnlyWhenFalling() {
        val d = Udp2CalProtocol.DriftController()
        var media = 0L
        d.update(120, media)
        var action = 0
        for (k in 1..29L) {
            media += Udp2CalProtocol.DRIFT_INTERVAL_SAMPLES
            val water = (120 - k * 20).coerceAtLeast(0).toInt()
            val a = d.update(water, media)
            if (a != 0) {
                action = a
                break
            }
        }
        assertEquals(-Udp2CalProtocol.DRIFT_MAX_SAMPLES, action)
    }

    @Test
    fun drift_ignoresStableLowWater() {
        val d = Udp2CalProtocol.DriftController()
        var media = 0L
        d.update(10, media)
        repeat(600) {
            media += Udp2CalProtocol.DRIFT_INTERVAL_SAMPLES
            assertEquals(0, d.update(10, media))
        }
        assertEquals(0L, d.corrections)
    }

    @Test
    fun playheadClock_mapsDeviceFrames() {
        val clock = Udp2CalProtocol.PlayheadClock()
        assertNull(clock.update(0L))
        clock.start(9600L, 1000L)
        assertEquals(9600L, clock.value())
        assertEquals(9600L + 480L, clock.update(1480L))
        clock.notePcmCorrection(240L)
        assertEquals(9600L + 480L + 240L, clock.value())
    }

    // ═══════ 常量与工具 ═══════

    @Test
    fun constants_matchRustImplementation() {
        assertEquals(3.toByte(), Udp2CalProtocol.CONTROL_VERSION)
        assertEquals(15, Udp2CalProtocol.CONTROL_HEADER_SIZE)
        assertEquals(512, Udp2CalProtocol.CONTROL_MAX_PAYLOAD)
        assertEquals(12, Udp2CalProtocol.RTP_HEADER_SIZE)
        assertEquals(111, Udp2CalProtocol.OPUS_PAYLOAD_TYPE)
        assertEquals(480, Udp2CalProtocol.OPUS_FRAME_SAMPLES)
        assertEquals(1275, Udp2CalProtocol.OPUS_MAX_PAYLOAD)
        assertEquals(1287, Udp2CalProtocol.RTP_MAX_PACKET)
        assertEquals(44043, Udp2CalProtocol.DISCOVER_PORT)
        assertEquals(44044, Udp2CalProtocol.DEFAULT_LISTEN_PORT)
        assertEquals(40, Udp2CalProtocol.JITTER_START_MS)
        assertEquals(60, Udp2CalProtocol.JITTER_TARGET_MS)
        assertEquals(120, Udp2CalProtocol.JITTER_HIGH_MS)
        assertEquals(20, Udp2CalProtocol.JITTER_LOW_MS)
        assertEquals(20, Udp2CalProtocol.PLC_LEAD_MS)
        assertEquals(120, Udp2CalProtocol.PLC_MAX_MS)
        assertEquals(64, Udp2CalProtocol.MAX_REORDER_PACKETS)
        assertEquals(200, Udp2CalProtocol.RESYNC_GAP_MS)
    }

    @Test
    fun bitrateClampedTo1000() {
        assertEquals(512, Udp2CalProtocol.clampBitrateKbps(0))
        assertEquals(512, Udp2CalProtocol.clampBitrateKbps(512))
        assertEquals(960, Udp2CalProtocol.clampBitrateKbps(960))
        assertEquals(1000, Udp2CalProtocol.clampBitrateKbps(1024))
        assertEquals(1000, Udp2CalProtocol.clampBitrateKbps(99999))
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
        val id = bytes(0x0A, 0x1B, 0x2C, 0x3D, 0x4E, 0x5F, 0x60, 0x7F)
        assertEquals("0a1b2c3d4e5f607f", Udp2CalProtocol.deviceIdToString(id))
    }
}
