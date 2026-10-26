package com.udp2cal.app

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import com.udp2cal.app.native.ProtocolNative
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

    /**
     * 连续拉取（pipeMs = 可听点之前压着的媒体量）。
     *
     * 传 PLC_LEAD_MS 表示"水位已见底"：缺口时生成 PLC/SILENCE；
     * 传更大的值表示"缓冲里还有货"，缺口时只会 WAIT。
     */
    private fun pullTags(
        jb: Udp2CalProtocol.JitterBuffer,
        pipeMs: Int,
        count: Int
    ): List<Int> {
        val out = ByteArray(Udp2CalProtocol.OPUS_MAX_PAYLOAD)
        val tags = mutableListOf<Int>()
        repeat(count) {
            when (jb.pull(0, pipeMs, out)) {
                Udp2CalProtocol.Pull.PACKET -> tags.add(out[0].toInt() and 0xFF)
                Udp2CalProtocol.Pull.PLC -> tags.add(0xFE)
                Udp2CalProtocol.Pull.SILENCE -> tags.add(0xFF)
                else -> return tags
            }
        }
        return tags
    }

    @Test
    fun jitter_prebuffersThreeFrames() {
        val jb = Udp2CalProtocol.JitterBuffer()
        insert(jb, 100, 4800L, true, 1)
        assertEquals(Udp2CalProtocol.Pull.WAIT, jb.pull(0, 50, ByteArray(1275)))
        insert(jb, 101, 5280L, false, 2)
        assertEquals(Udp2CalProtocol.Pull.WAIT, jb.pull(0, 50, ByteArray(1275)))
        insert(jb, 102, 5760L, false, 3)
        // JITTER_START_MS = 30 → 连续 3 帧即起播
        assertEquals(30, jb.waterMs())
        assertEquals(listOf(1, 2, 3), pullTags(jb, 20, 3))
        assertEquals(0, jb.waterMs())
    }

    @Test
    fun jitter_reassemblesOutOfOrder() {
        val jb = Udp2CalProtocol.JitterBuffer()
        insert(jb, 0, 0L, true, 10)
        insert(jb, 2, 960L, false, 12)
        insert(jb, 3, 1440L, false, 13)
        insert(jb, 1, 480L, false, 11)
        assertEquals(listOf(10, 11, 12, 13), pullTags(jb, 0, 4))
    }

    @Test
    fun jitter_dropsDuplicateAndLate() {
        val jb = Udp2CalProtocol.JitterBuffer()
        for (i in 0 until 4) insert(jb, i, 480L * i, i == 0, 20 + i)
        assertEquals(listOf(20, 21), pullTags(jb, 0, 2)) // nextTs = 960

        assertFalse(insert(jb, 2, 960L, false, 99)) // 重复
        assertFalse(insert(jb, 1, 480L, false, 98)) // 迟到
        assertEquals(listOf(22, 23), pullTags(jb, 0, 2))
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

        assertEquals(listOf(1, 2, 3, 4), pullTags(jb, 0, 4)) // nextTs = 1920
        // 水位还有 30 ms：缺口不该补 PLC（补了就是凭空多出音频）
        assertEquals(Udp2CalProtocol.Pull.WAIT, jb.pull(0, 30, ByteArray(1275)))
        // 播放头走到 20 ms（PLC 提前量）→ 缺口到期
        assertEquals(Udp2CalProtocol.Pull.PLC, jb.pull(0, Udp2CalProtocol.PLC_LEAD_MS, ByteArray(1275)))
        assertEquals(Udp2CalProtocol.Pull.PACKET, jb.pull(0, Udp2CalProtocol.PLC_LEAD_MS, ByteArray(1275)))
    }

    @Test
    fun jitter_plcRunCappedThenSilence() {
        val jb = Udp2CalProtocol.JitterBuffer()
        for (i in 0 until 4) insert(jb, i, 480L * i, i == 0, i)
        pullTags(jb, 0, 4)

        var plc = 0
        var silence = 0
        for (step in 0 until 30L) {
            when (jb.pull(0, Udp2CalProtocol.PLC_LEAD_MS, ByteArray(1275))) {
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
        pullTags(jb, 0, 4)
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
        pullTags(jb, 0, 2) // nextSeq = 2，窗口 2..=65
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
        assertEquals(listOf(2, 3), pullTags(jb, 0, 2))
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
        assertEquals((1..8).toList(), pullTags(jb, 0, 8))
        assertEquals(baseTs + 480L * 8, jb.nextTs)
    }

    @Test
    fun jitter_realignsWhenSeqLagsBehindTs() {
        // 回归：发送端在 ACK 中断 / DTX 期间"时间戳前进、序号冻结"，
        // 接收端同时用 PLC/静音把 nextSeq 推进了同样的量。恢复包时间戳连续、
        // 序号却落后——必须按时间戳重新对齐，否则整条后续流会被永久丢弃
        // （M=1 重同步分支在旧实现里永远不可达）。
        val jb = Udp2CalProtocol.JitterBuffer()
        for (i in 0 until 8) insert(jb, i, 480L * i, i == 0, i + 1)
        pullTags(jb, 0, 8)
        for (k in 0 until 50L) jb.pull(0, 0, ByteArray(1275))
        assertEquals(58L, jb.nextSeq)
        assertEquals(480L * 58, jb.nextTs)

        // 恢复包：序号仍是 8（未随静音递增），时间戳正好接上，M=1
        assertTrue(insert(jb, 8, 480L * 58, true, 9))
        assertEquals(1L, jb.seqRealign)
        assertEquals(8L, jb.nextSeq)
        assertEquals(480L * 58, jb.nextTs)

        // 后续包全部正常接纳并出音
        for (i in 1..10) assertTrue(insert(jb, 8 + i, 480L * (58 + i), false, 10 + i))
        val tags = pullTags(jb, 0, 11)
        assertEquals(11, tags.size)
        assertTrue(tags.none { it == 0xFE || it == 0xFF })
    }

    /**
     * VBR 下包长会在 40–100 字节之间跳。曾经的实现把"上一次收包长度"当成
     * 拉取缓冲的容量：紧跟一个短包之后，一帧更长的音频会被判成 WAIT
     * （时间轴不前进，播放线程空转 → 断音）。这里用 1/100 字节交替的载荷守住。
     */
    @Test
    fun jitter_pullSurvivesVariablePayloadLengths() {
        val jb = Udp2CalProtocol.JitterBuffer()
        val out = ByteArray(Udp2CalProtocol.OPUS_MAX_PAYLOAD)
        val short = ByteArray(1) { 0x11 }
        val long = ByteArray(100) { 0x22 }
        // 三帧连续 → 预缓冲满足，第一次 pull 就进 PLAYING
        assertTrue(jb.insert(100, 4800L, false, short, 0, short.size))
        assertTrue(jb.insert(101, 5280L, false, long, 0, long.size))
        assertTrue(jb.insert(102, 5760L, false, short, 0, short.size))

        assertEquals(Udp2CalProtocol.Pull.PACKET, jb.pull(0, 40, out))
        assertEquals(1, jb.pulledLength)
        assertEquals(0x11, out[0].toInt() and 0xFF)

        // 队首换成 100 字节帧：容量必须自动补齐，否则这里返回 WAIT
        assertEquals(Udp2CalProtocol.Pull.PACKET, jb.pull(0, 40, out))
        assertEquals(100, jb.pulledLength)
        assertEquals(0x22, out[0].toInt() and 0xFF)
        jb.close()
    }

    // ═══════ 阶跃吸收（方案 C） ═══════

    @Test
    fun jitter_skipFramesAdvancesTheTimeline() {
        val jb = Udp2CalProtocol.JitterBuffer()
        for (i in 0 until 12) insert(jb, 100 + i, 4800L + 480L * i, i == 0, i + 1)
        assertEquals(120, jb.waterMs())
        assertEquals(listOf(1), pullTags(jb, 0, 1))
        // 跳 4 帧：时间线前移 40 ms，不产生 PLC，不计入 epoch
        val epoch = jb.epoch
        assertEquals(4, jb.skipFrames(4))
        assertEquals(120 - 10 - 40, jb.waterMs())
        assertEquals(4L, jb.stepTrims)
        assertEquals(0L, jb.plcFrames)
        assertEquals(epoch, jb.epoch)
        // 跳过的帧确实不会被再抬出来：下一帧是第 6 个包的 tag
        assertEquals(listOf(6), pullTags(jb, 0, 1))
    }

    @Test
    fun jitter_skipFramesNeverRunsPastArrivedMedia() {
        val jb = Udp2CalProtocol.JitterBuffer()
        for (i in 0 until 4) insert(jb, i, 480L * i, i == 0, i + 1)
        // 先进 PLAYING（ringMs >= DECODE_LIMIT_MS 让 pull 返回 WAIT、不推进播放头）
        val out = ByteArray(Udp2CalProtocol.OPUS_MAX_PAYLOAD)
        assertEquals(Udp2CalProtocol.Pull.WAIT, jb.pull(Udp2CalProtocol.DECODE_LIMIT_MS, 0, out))
        assertTrue(jb.playing)
        // 只到了 4 帧（40 ms），要求跳 12 帧：只能跳到最后一帧之前（最新那帧必须留着）
        assertEquals(480L * 3, jb.highestTsValue())
        val skipped = jb.skipFrames(12)
        assertEquals(3, skipped)
        assertEquals(jb.highestTsValue(), jb.nextTs)
        assertEquals(Udp2CalProtocol.OPUS_FRAME_MS, jb.waterMs())
        assertEquals(3L, jb.stepTrims)
        assertEquals(0L, jb.plcFrames)
    }

    @Test
    fun jitter_skipFramesNeedsAPlayingBuffer() {
        val jb = Udp2CalProtocol.JitterBuffer()
        insert(jb, 0, 0L, true, 1)
        insert(jb, 1, 480L, false, 2)
        // 未进入 PLAYING（不足 JITTER_START_MS）：水位还在爬升，不能当阶跃
        assertTrue(!jb.playing)
        assertEquals(0, jb.skipFrames(1))
        assertEquals(0L, jb.stepTrims)
    }

    @Test
    fun stepAbsorberIgnoresNormalWaterNoise() {
        val a = Udp2CalProtocol.StepAbsorber()
        // 稳态：水位在目标 ±2 帧内跳，总量远低于 target+EXCESS
        for (w in intArrayOf(150, 160, 140, 155, 145, 150)) {
            val d = a.decide(w, 44, true, 150)
            assertEquals(0, d.frames)
            assertTrue(!d.limited)
        }
        assertEquals(0L, a.hits)
    }

    @Test
    fun stepAbsorberTrimsOnASingleBatchGrowth() {
        val a = Udp2CalProtocol.StepAbsorber()
        assertEquals(0, a.decide(150, 44, true, 150).frames)
        // 一次补齐 120 ms → 命中"增长 > 80 ms"，水位总量 314 → 跳上限 12 帧
        val d = a.decide(270, 44, true, 150)
        assertTrue(!d.limited)
        assertEquals(Udp2CalProtocol.PLC_MAX_MS / Udp2CalProtocol.OPUS_FRAME_MS, d.frames)
        assertEquals(1L, a.hits)
    }

    @Test
    fun stepAbsorberTrimsWhenWaterExceedsTargetWindow() {
        val a = Udp2CalProtocol.StepAbsorber()
        // 没有大增长，但总量超过 target + STEP_TRIM_EXCESS_MS
        assertEquals(0, a.decide(190, 60, true, 150).frames)
        assertEquals(12, a.decide(210, 60, true, 150).frames)
    }

    @Test
    fun stepAbsorberRespectsThePerMinuteLimit() {
        val a = Udp2CalProtocol.StepAbsorber()
        var trimmed = 0
        var limited = 0
        repeat(Udp2CalProtocol.STEP_TRIM_MAX_PER_MIN + 2) {
            // 每轮先回到目标附近，再制造一次 120 ms 阶跃
            a.decide(150, 44, true, 150)
            val d = a.decide(270, 44, true, 150)
            if (d.frames == 0) return@repeat
            if (d.limited) limited++ else trimmed++
        }
        assertEquals(Udp2CalProtocol.STEP_TRIM_MAX_PER_MIN, trimmed)
        assertTrue("超限必须只记录不裁剪", limited >= 1)
    }

    @Test
    fun stepAbsorberOnlyActsOncePlaying() {
        val a = Udp2CalProtocol.StepAbsorber()
        // 预缓冲期水位从 0 爬升：不得当成阶跃
        assertEquals(0, a.decide(210, 44, false, 150).frames)
        assertEquals(0L, a.hits)
        // 同一水位在 PLAYING 下也不算"增长"，但总量 254 已过 target+100
        assertEquals(11, a.decide(210, 44, true, 150).frames)
    }
    // ═══════ 漂移校正：抗饱和 PI ═══════

    /** 按 10 ms 一帧推进（计时用**设备帧**，与播放端一致），返回最终比率微调 */
    private fun runPi(
        d: Udp2CalProtocol.DriftController,
        waterMs: Int,
        frames: Int,
        dev: LongArray,
    ): Double {
        repeat(frames) {
            dev[0] += 480
            d.update(waterMs, dev[0])
        }
        return d.adjust()
    }

    @Test
    fun drift_onTargetDoesNothing() {
        // 水位稳定在目标上：比率一动不动
        val d = Udp2CalProtocol.DriftController()
        val dev = longArrayOf(0)
        d.update(Udp2CalProtocol.WATER_TARGET_MS, dev[0])
        val a = runPi(d, Udp2CalProtocol.WATER_TARGET_MS, 1000, dev)
        assertEquals(0.0, a, 1e-12)
        assertEquals(0.0, d.integral(), 1e-12)
    }

    @Test
    fun drift_ratioHasExpectedSignAndStaysBounded() {
        // 水位高出目标 40 ms：比率向上顶，但绝不越过 ±0.5%
        val high = Udp2CalProtocol.WATER_TARGET_MS + 40
        val d = Udp2CalProtocol.DriftController()
        val dev = longArrayOf(0)
        d.update(high, dev[0])
        // KP 只有 20 ppm/ms：40 ms 偏差 → 800 ppm；积分项权限只有 1500 ppm
        // （Ti = 90 s，约 170 s 长满），所以比率实际封顶在 +0.23% 附近。
        repeat(20_000) {
            dev[0] += 480
            val u = d.update(high, dev[0])
            assertTrue("比率越界: $u", kotlin.math.abs(u) <= Udp2CalProtocol.RESAMPLE_MAX_ADJUST + 1e-12)
            assertTrue("水位偏高时 adjust 不得为负: $u", u >= -1e-12)
        }
        assertTrue(
            "高水位应把积分项推到权限上限: ${d.integral()}",
            d.integral() >= Udp2CalProtocol.PI_INTEGRAL_MAX - 1e-9
        )
        assertTrue(
            "高水位应把比率推到积分权限之上: ${d.adjust()}",
            d.adjust() >= Udp2CalProtocol.PI_INTEGRAL_MAX * 0.99
        )
        // 反方向对称
        val low = Udp2CalProtocol.WATER_TARGET_MS - 60
        val d2 = Udp2CalProtocol.DriftController()
        val dev2 = longArrayOf(0)
        d2.update(low, dev2[0])
        val a = runPi(d2, low, 20_000, dev2)
        assertTrue(
            "缺水应把比率推到积分权限之下: $a",
            a <= -Udp2CalProtocol.PI_INTEGRAL_MAX * 0.99
        )
        assertTrue("不得越过钳位: $a", a > -Udp2CalProtocol.RESAMPLE_MAX_ADJUST)
    }

    @Test
    fun drift_ratioSlewIsGradual() {
        // 单次比率更新步进受限：不允许一帧之内跳到满幅（那是可闻的变调台阶）
        val d = Udp2CalProtocol.DriftController()
        val dev = longArrayOf(0)
        d.update(300, dev[0])
        var prev = 0.0
        repeat(60) {
            dev[0] += 480
            val u = d.update(300, dev[0])
            assertTrue(
                "步进过大: $prev → $u",
                kotlin.math.abs(u - prev) <= Udp2CalProtocol.RATIO_SLEW_STEP + 1e-12
            )
            prev = u
        }
        assertTrue(prev > 0.0)
    }

    /** 闭环仿真一帧：水位是"流入 10 ms − 流出 10 ms×(1+adjust)"的积分 */
    private fun plantStep(water: Double, adjust: Double): Double =
        (water - 10.0 * adjust).coerceIn(0.0, 640.0)

    @Test
    fun drift_closedLoopSettlesOnTargetWithoutOffset() {
        // PI 与纯比例的根本区别：纯比例必然留静差（水位永远差一点），
        // 这里要求稳态偏差 → 0，且稳态比率 → 0（不再有任何变调）。
        val d = Udp2CalProtocol.DriftController()
        val dev = longArrayOf(0)
        var water = 20.0 // 起步水位远低于目标
        d.update(water.toInt(), dev[0])
        val tail = ArrayList<Double>()
        for (i in 0 until 40_000) {
            dev[0] += 480
            val a = d.update(water.toInt(), dev[0])
            water = plantStep(water, a)
            if (i >= 35_000) tail.add(water)
        }
        val mean = tail.sum() / tail.size
        assertTrue(
            "稳态水位必须收敛到目标: $mean",
            kotlin.math.abs(mean - Udp2CalProtocol.WATER_TARGET_MS) < 2.0
        )
        assertTrue("稳态比率必须收敛到零: ${d.adjust()}", kotlin.math.abs(d.adjust()) < 3e-4)
    }

    @Test
    fun drift_closedLoopTracksClockSkew() {
        // 闭环里出现 300 ppm 的持续速率差（前馈没跟上的部分）：PI 必须靠积分项
        // 把比率推到 +300 ppm 附近，让水位稳定在目标上（无静差）。
        val d = Udp2CalProtocol.DriftController()
        val dev = longArrayOf(0)
        var water = Udp2CalProtocol.WATER_TARGET_MS.toDouble()
        d.update(water.toInt(), dev[0])
        val skew = 300e-6
        // Ti = 90 s、KP = 20 ppm/ms → ζ≈0.67、ω_n≈0.015 rad/s，
        // 整定时间 ≈4/(ζ·ω_n) ≈ 400 s，所以这里跑 900 s。
        repeat(90_000) {
            dev[0] += 480
            val a = d.update(water.toInt(), dev[0])
            // 发送端比本机快 skew → 每 10 ms 多进来 10×skew 的媒体
            water = plantStep(water + 10.0 * skew, a)
        }
        assertTrue(
            "有速率差时水位仍需钉在目标: $water",
            kotlin.math.abs(water - Udp2CalProtocol.WATER_TARGET_MS) < 2.0
        )
        val total = d.adjust()
        assertTrue(
            "比率必须收敛到 +skew（残余 ${(total - skew) * 1e6} ppm）",
            kotlin.math.abs(total - skew) < 60e-6
        )
    }

    @Test
    fun drift_integralIsAntiWindup() {
        // 长期饱和不得让积分项越界蓄能，否则退饱和时会长时间反向过冲
        val d = Udp2CalProtocol.DriftController()
        val dev = longArrayOf(0)
        d.update(300, dev[0])
        runPi(d, 300, 30_000, dev) // 300 s 极端高水位
        assertTrue(
            "积分项溢出: ${d.integral()}",
            kotlin.math.abs(d.integral()) <= Udp2CalProtocol.RESAMPLE_MAX_ADJUST + 1e-12
        )
        assertTrue(d.adjust() <= Udp2CalProtocol.RESAMPLE_MAX_ADJUST + 1e-12)
    }

    @Test
    fun drift_jitterInBandBarelyModulatesRatio() {
        // 规范 §六 风险 2：比例项不得把网络抖动变成变调。
        // ±4 ms、周期 1 s 的水位抖动 → 比率调制必须远小于 500 ppm。
        val d = Udp2CalProtocol.DriftController()
        val dev = longArrayOf(0)
        d.update(Udp2CalProtocol.WATER_TARGET_MS, dev[0])
        var worst = 0.0
        repeat(3000) { i ->
            dev[0] += 480
            val w = if ((i / 100) % 2 == 0) {
                Udp2CalProtocol.WATER_TARGET_MS + 4
            } else {
                Udp2CalProtocol.WATER_TARGET_MS - 4
            }
            worst = maxOf(worst, kotlin.math.abs(d.update(w, dev[0])))
        }
        assertTrue("抖动被放大成变调: $worst", worst < 0.0005)
    }

    @Test
    fun drift_ignoresRepeatedUpdatesWithoutTimePassing() {
        // 计时用声卡帧数：没有新的设备帧就不评估，避免忙等循环虚增积分
        val d = Udp2CalProtocol.DriftController()
        val dev = longArrayOf(0)
        d.update(Udp2CalProtocol.WATER_TARGET_MS, dev[0])
        runPi(d, 120, 100, dev)
        val updates = d.ratioUpdates
        val adjust = d.adjust()
        repeat(50) { d.update(1000, dev[0]) }
        assertEquals(updates, d.ratioUpdates)
        assertEquals(adjust, d.adjust(), 1e-15)
    }

    @Test
    fun drift_worksAt44k1DeviceRate() {
        // 计时按设备率折算：44.1 kHz 上 441 帧 = 10 ms
        val high = Udp2CalProtocol.WATER_TARGET_MS + 60
        val d = Udp2CalProtocol.DriftController(44100)
        val dev = longArrayOf(0)
        d.update(high, dev[0])
        repeat(2000) {
            dev[0] += 441
            d.update(high, dev[0])
        }
        assertTrue("44.1 kHz 上高水位也必须顶起比率: ${d.adjust()}", d.adjust() > 0.001)
        assertTrue(d.ratioUpdates > 1000)
    }

    // ═══════ 速率前馈估计 ═══════

    /**
     * 合成观测：发送端媒体时钟按 [skew] 快于本地时钟，到达时刻叠加
     * [jitterUs] 的**单向**排队延迟（排队只会让包到得更晚）。
     */
    private fun feedEstimator(
        est: Udp2CalProtocol.RateEstimator,
        seconds: Double,
        skew: Double,
        jitterUs: Double,
    ) {
        val base = System.nanoTime()
        val stepUs = 10_000.0 // 每 10 ms 一个包
        val n = (seconds * 100.0).toInt()
        var seed = 0x1234_5678L
        for (i in 0 until n) {
            seed = seed * 6364136223846793005L + 1442695040888963407L
            val r = (seed ushr 33).toDouble() / (1L shl 31).toDouble() // [0,1)
            val tUs = (i + 1) * stepUs
            val mediaSamples = (tUs * 1e-6 * (1.0 + skew) * 48000.0).toLong()
            val arriveUs = tUs + r * jitterUs
            est.observe(mediaSamples, base + (arriveUs * 1000.0).toLong())
        }
    }

    @Test
    fun rateEstimator_needsWindowBeforePublishing() {
        val est = Udp2CalProtocol.RateEstimator()
        assertNull(est.feedforward())
        feedEstimator(est, 3.0, 0.0, 0.0)
        // 3 s 只有 3 个桶，不够 RATE_MIN_BUCKETS → 不出估计（宁可前馈置 0）
        assertNull(est.feedforward())
        val est2 = Udp2CalProtocol.RateEstimator()
        feedEstimator(est2, 10.0, 0.0, 0.0)
        assertNotNull("窗口攒够后必须出估计", est2.feedforward())
    }

    @Test
    fun rateEstimator_detectsFastSender() {
        // 发送端快 200 ppm：前馈估计必须是 +200 ppm（±15 ppm）
        val est = Udp2CalProtocol.RateEstimator()
        feedEstimator(est, 40.0, 200e-6, 0.0)
        val ppm = est.feedforwardPpm()
        assertNotNull("必须出估计", ppm)
        assertTrue("估计偏差过大: $ppm ppm", kotlin.math.abs(ppm!! - 200.0) < 15.0)
        assertTrue("σ 过大: ${est.sigmaPpm()} ppm", est.sigmaPpm() < 20.0)
        assertTrue(est.usedBuckets() >= Udp2CalProtocol.RATE_MIN_BUCKETS)
    }

    @Test
    fun rateEstimator_detectsSlowSender() {
        val est = Udp2CalProtocol.RateEstimator()
        feedEstimator(est, 40.0, -300e-6, 0.0)
        val ppm = est.feedforwardPpm()
        assertNotNull(ppm)
        assertTrue("估计偏差过大: $ppm ppm", kotlin.math.abs(ppm!! + 300.0) < 15.0)
    }

    @Test
    fun rateEstimator_rejectsQueueingBursts() {
        // 单向排队延迟（0–20 ms 随机）不得把估计带偏：最小延迟滤波要把它吃掉
        val est = Udp2CalProtocol.RateEstimator()
        feedEstimator(est, 40.0, 0.0, 20_000.0)
        val ppm = est.feedforwardPpm()
        assertNotNull(ppm)
        assertTrue("排队抖动污染了估计: $ppm ppm", kotlin.math.abs(ppm!!) < 30.0)
    }

    @Test
    fun rateEstimator_isClamped() {
        val est = Udp2CalProtocol.RateEstimator()
        feedEstimator(est, 40.0, 0.10, 0.0) // 10% 的荒谬钟差
        val ppm = est.feedforwardPpm()
        assertNotNull(ppm)
        assertTrue("前馈未钳位: $ppm ppm", ppm!! <= Udp2CalProtocol.RATE_FEEDFORWARD_MAX * 1e6 + 1.0)
    }

    @Test
    fun rateEstimator_resetRestartsWindow() {
        val est = Udp2CalProtocol.RateEstimator()
        feedEstimator(est, 20.0, 200e-6, 0.0)
        assertNotNull(est.feedforward())
        est.reset()
        assertNull(est.feedforward())
        assertEquals(0, est.usedBuckets())
        // 重同步换了时间线基准：旧窗口绝不能继续参与回归
        feedEstimator(est, 2.0, 200e-6, 0.0)
        assertNull(est.feedforward())
    }

    @Test
    fun rateEstimator_discardsRebuiltTimeline() {
        // 实机：手机重连/断流补齐后 highestTs 会突然跳一大段，而 epoch 未必变。
        // 旧窗口的 (t_local, media) 对不再可比，必须整窗作废。Rust 侧同名测试。
        val est = Udp2CalProtocol.RateEstimator()
        feedEstimator(est, 20.0, 200e-6, 0.0)
        assertNotNull(est.feedforward())
        // 下一包比上一包晚了 10 s 的媒体
        est.observe(30L * 48_000, System.nanoTime())
        assertNull("跳变后旧窗口必须作废", est.feedforward())
        assertEquals(0, est.usedBuckets())
    }

    @Test
    fun rateEstimator_ignoresRepeatedTimestamp() {
        val est = Udp2CalProtocol.RateEstimator()
        val base = System.nanoTime()
        est.observe(0, base)
        est.observe(480, base + 10_000_000L)
        val obs = est.observations
        est.observe(480, base + 12_000_000L)
        assertEquals("同一个时间戳只能记一次", obs, est.observations)
    }

    // ═══════ 多相 FIR 重采样 ═══════

    @Test
    fun resampler_outputsExpectedFrameRate() {
        // 48k → 48k、比率 1：100 块 480 帧应收敛到 48000 附近
        // （首块少半个窗长，群延迟固定 ≈24.5 样本，不是丢样）
        val r = StreamResampler(2)
        r.setSourceRate(48000, 48000)
        val pcm = ShortArray(960) { 1000 }
        var total = 0L
        repeat(100) { total += r.process(pcm, 0, 480) / 2 }
        assertTrue("1 秒产出 $total，期望约 47975", kotlin.math.abs(total - 47975L) <= 3)
    }

    @Test
    fun resampler_ratioAdjustChangesProducedFrames() {
        // 比率 +0.5% → 产出更少；−0.5% → 产出更多，差值约 480 帧
        var fast = 0L
        var slow = 0L
        for ((adjust, target) in listOf(0.005 to 47737L, -0.005 to 48217L)) {
            val r = StreamResampler(2)
            r.setSourceRate(48000, 48000)
            r.setAdjust(adjust)
            val pcm = ShortArray(960) { 1000 }
            var total = 0L
            repeat(100) { total += r.process(pcm, 0, 480) / 2 }
            assertTrue(
                "adjust=$adjust 1 秒产出 $total，期望约 $target",
                kotlin.math.abs(total - target) <= 5
            )
            if (adjust > 0) fast = total else slow = total
        }
        assertTrue("两个方向必须对称: $fast vs $slow", kotlin.math.abs((slow - fast) - 480L) <= 12)
    }

    @Test
    fun resampler_keepsPhaseAcrossBlocks() {
        // 44.1 kHz 设备：10 块 480 帧 → 约 4386 帧（相位跨块连续，不丢不重）
        val r = StreamResampler(2)
        r.setSourceRate(48000, 44100)
        val pcm = ShortArray(960) { 1000 }
        var total = 0L
        repeat(10) { total += r.process(pcm, 0, 480) / 2 }
        assertTrue("产出 $total，期望约 4386", kotlin.math.abs(total - 4386L) <= 6)
    }

    @Test
    fun resampler_passesDcWithoutRipple() {
        // 每相按 DC 增益归一：恒定直流必须原样通过（±1 量化级）
        val r = StreamResampler(2)
        r.setSourceRate(48000, 48000)
        r.setAdjust(0.001)
        val pcm = ShortArray(960) { 500 }
        repeat(20) { block ->
            val n = r.process(pcm, 0, 480)
            if (block > 0) { // 首块在填充历史，跳过
                for (i in 0 until n) {
                    assertTrue("直流被破坏: ${r.buffer()[i]}", kotlin.math.abs(r.buffer()[i] - 500) <= 1)
                }
            }
        }
    }

    @Test
    fun resampler_hasNoClickAtBlockBoundaries() {
        // 1 kHz 正弦（48 样本一个周期）：块边界不得出现台阶
        val r = StreamResampler(2)
        r.setSourceRate(48000, 48000)
        r.setAdjust(0.0005)
        val pcm = ShortArray(960)
        for (i in 0 until 480) {
            val v = (kotlin.math.sin(2.0 * Math.PI * 1000.0 * i / 48000.0) * 16000).toInt()
            pcm[i * 2] = v.toShort()
            pcm[i * 2 + 1] = v.toShort()
        }
        var prev = 0
        var first = true
        repeat(20) {
            val n = r.process(pcm, 0, 480)
            for (i in 0 until n) {
                val v = r.buffer()[i].toInt()
                if (!first) {
                    assertTrue(
                        "样本处有台阶: Δ=${kotlin.math.abs(v - prev)}",
                        kotlin.math.abs(v - prev) <= 2600
                    )
                }
                prev = v
                first = false
            }
        }
    }

    @Test
    fun resampler_clampsAdjust() {
        val r = StreamResampler(2)
        r.setSourceRate(48000, 48000)
        r.setAdjust(9.0)
        assertEquals(1.0 + Udp2CalProtocol.RESAMPLE_MAX_ADJUST, r.ratio, 1e-12)
        r.setAdjust(Double.NaN)
        assertEquals(1.0, r.ratio, 1e-12)
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
        assertEquals(30, Udp2CalProtocol.JITTER_START_MS)
        // 目标水位含声卡缓冲（单一口径），按实机批次交付特征定到 150
        assertEquals(150, Udp2CalProtocol.WATER_TARGET_MS)
        assertEquals(40, Udp2CalProtocol.DECODE_LIMIT_MS)
        assertTrue(Udp2CalProtocol.PLC_LEAD_MS < Udp2CalProtocol.DECODE_LIMIT_MS)
        assertTrue(Udp2CalProtocol.DECODE_LIMIT_MS < Udp2CalProtocol.WATER_TARGET_MS)
        assertEquals(20, Udp2CalProtocol.PLC_LEAD_MS)
        assertEquals(120, Udp2CalProtocol.PLC_MAX_MS)
        assertEquals(64, Udp2CalProtocol.MAX_REORDER_PACKETS)
        assertEquals(200, Udp2CalProtocol.RESYNC_GAP_MS)
        // 码率上限与档位必须与 Rust 端逐项一致：接收端会用它钳制对端下发的值
        assertEquals(512, Udp2CalProtocol.BITRATE_AUTO_KBPS)
        assertEquals(512, Udp2CalProtocol.BITRATE_MAX_KBPS)
        assertArrayEquals(
            intArrayOf(64, 128, 256, 384, 512),
            Udp2CalProtocol.BITRATE_STEPS_KBPS
        )
    }

    @Test
    fun bitrateClampedTo512() {
        assertEquals(512, Udp2CalProtocol.clampBitrateKbps(0))
        assertEquals(64, Udp2CalProtocol.clampBitrateKbps(64))
        assertEquals(512, Udp2CalProtocol.clampBitrateKbps(512))
        // 旧客户端的 768/960/1000/1024 一律压到上限
        assertEquals(512, Udp2CalProtocol.clampBitrateKbps(768))
        assertEquals(512, Udp2CalProtocol.clampBitrateKbps(1024))
        assertEquals(512, Udp2CalProtocol.clampBitrateKbps(99999))
    }

    @Test
    fun bitrateSnapsToManualSteps() {
        for (s in Udp2CalProtocol.BITRATE_STEPS_KBPS) {
            assertEquals(s, Udp2CalProtocol.snapBitrateKbps(s))
        }
        assertEquals("自动不被吸附", 0, Udp2CalProtocol.snapBitrateKbps(0))
        assertEquals(64, Udp2CalProtocol.snapBitrateKbps(1))
        assertEquals("等距时取较小档", 64, Udp2CalProtocol.snapBitrateKbps(96))
        assertEquals(128, Udp2CalProtocol.snapBitrateKbps(97))
        assertEquals(256, Udp2CalProtocol.snapBitrateKbps(200))
        assertEquals(256, Udp2CalProtocol.snapBitrateKbps(300))
        assertEquals(384, Udp2CalProtocol.snapBitrateKbps(330))
        assertEquals("旧档位必须落到上限", 512, Udp2CalProtocol.snapBitrateKbps(768))
        assertEquals(512, Udp2CalProtocol.snapBitrateKbps(1000))
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

    // ═══════ 单一真源：Kotlin 常量必须与 Rust 导出的常量逐项一致 ═══════

    /**
     * 行为已经全部由 Rust 提供，常量却仍要在 Kotlin 里留一份（编译期需要）。
     * 这个测试是防漂移的闸门：任何一边改了数值而另一边没改，这里立刻红。
     */
    @Test
    fun nativeConstantsMatchKotlinConstants() {
        // `UDP2CAL_WATER_TARGET_MS` 会覆盖 Rust 侧的默认目标水位（见 docs/PROTOCOL_V3_RTP.md §7.10.1），
        // 所以期望值按覆盖后的钳位结果算；没设环境变量时就是 Kotlin 常量本身。
        val expectedWaterTarget = System.getenv("UDP2CAL_WATER_TARGET_MS")
            ?.trim()?.toIntOrNull()
            ?.coerceIn(Udp2CalProtocol.WATER_TARGET_MIN_MS, Udp2CalProtocol.WATER_TARGET_MAX_MS)
            ?: Udp2CalProtocol.WATER_TARGET_MS
        val expected = longArrayOf(
            Udp2CalProtocol.CONTROL_VERSION.toLong(),
            Udp2CalProtocol.CONTROL_HEADER_SIZE.toLong(),
            Udp2CalProtocol.CONTROL_MAX_PAYLOAD.toLong(),
            Udp2CalProtocol.DEVICE_ID_SIZE.toLong(),
            Udp2CalProtocol.DISCOVER_PORT.toLong(),
            Udp2CalProtocol.DEFAULT_LISTEN_PORT.toLong(),
            Udp2CalProtocol.TYPE_DATA.toLong(),
            Udp2CalProtocol.TYPE_CONNECT.toLong(),
            Udp2CalProtocol.TYPE_DISCOVER_REQ.toLong(),
            Udp2CalProtocol.TYPE_DISCOVER_REPLY.toLong(),
            Udp2CalProtocol.TYPE_CONNECT_ACK.toLong(),
            Udp2CalProtocol.TYPE_MAX_KNOWN.toLong(),
            Udp2CalProtocol.CONNECT_PAYLOAD_SIZE.toLong(),
            Udp2CalProtocol.CONNECT_ACK_PAYLOAD_SIZE.toLong(),
            Udp2CalProtocol.DEVICE_NAME_MAX.toLong(),
            Udp2CalProtocol.RTP_HEADER_SIZE.toLong(),
            Udp2CalProtocol.OPUS_PAYLOAD_TYPE.toLong(),
            Udp2CalProtocol.OPUS_SAMPLE_RATE.toLong(),
            Udp2CalProtocol.OPUS_FRAME_MS.toLong(),
            Udp2CalProtocol.OPUS_FRAME_SAMPLES.toLong(),
            Udp2CalProtocol.OPUS_MAX_PAYLOAD.toLong(),
            Udp2CalProtocol.RTP_MAX_PACKET.toLong(),
            Udp2CalProtocol.UDP_IP_MAX_PACKET.toLong(),
            Udp2CalProtocol.BITRATE_AUTO_KBPS.toLong(),
            Udp2CalProtocol.BITRATE_MAX_KBPS.toLong(),
            Udp2CalProtocol.BITRATE_STEPS_KBPS[0].toLong(),
            Udp2CalProtocol.BITRATE_STEPS_KBPS[1].toLong(),
            Udp2CalProtocol.BITRATE_STEPS_KBPS[2].toLong(),
            Udp2CalProtocol.BITRATE_STEPS_KBPS[3].toLong(),
            Udp2CalProtocol.BITRATE_STEPS_KBPS[4].toLong(),
            Udp2CalProtocol.MAX_REORDER_PACKETS.toLong(),
            Udp2CalProtocol.ARRIVAL_TRACE.toLong(),
            Udp2CalProtocol.JITTER_START_MS.toLong(),
            expectedWaterTarget.toLong(),
            Udp2CalProtocol.WATER_TARGET_MIN_MS.toLong(),
            Udp2CalProtocol.WATER_TARGET_MAX_MS.toLong(),
            Udp2CalProtocol.DECODE_LIMIT_MS.toLong(),
            Udp2CalProtocol.PLC_LEAD_MS.toLong(),
            Udp2CalProtocol.PLC_MAX_MS.toLong(),
            Udp2CalProtocol.RESYNC_GAP_MS.toLong(),
            Udp2CalProtocol.STEP_TRIM_GROWTH_MS.toLong(),
            Udp2CalProtocol.STEP_TRIM_EXCESS_MS.toLong(),
            Udp2CalProtocol.STEP_TRIM_MAX_PER_MIN.toLong()
        )
        val actual = ProtocolNative.constants()
        assertEquals("常量个数", expected.size, actual.size)
        for (i in expected.indices) {
            assertEquals("第 $i 项常量与 Rust 不一致", expected[i], actual[i])
        }
        // 水位目标走 Rust 的 `water_target_ms()`（环境变量可覆盖），默认值必须相同
        assertEquals(expectedWaterTarget, ProtocolNative.waterTargetMs())
    }

    @Test
    fun nativeRealConstantsMatchKotlinConstants() {
        val expected = doubleArrayOf(
            Udp2CalProtocol.RESAMPLE_MAX_ADJUST,
            Udp2CalProtocol.RATIO_SLEW_STEP,
            Udp2CalProtocol.PI_KP_PER_MS,
            Udp2CalProtocol.PI_TI_S,
            Udp2CalProtocol.PI_INTEGRAL_MAX,
            Udp2CalProtocol.PI_MIN_UPDATE_MS,
            Udp2CalProtocol.WATER_SMOOTH_ALPHA,
            Udp2CalProtocol.RATE_WINDOW_S,
            Udp2CalProtocol.RATE_FEEDFORWARD_MAX
        )
        val actual = ProtocolNative.constantsReal()
        assertEquals("浮点常量个数", expected.size, actual.size)
        for (i in expected.indices) {
            assertEquals("第 $i 项浮点常量与 Rust 不一致", expected[i], actual[i], 0.0)
        }
    }
}
