package com.udp2cal.app.service

import com.udp2cal.app.Udp2CalProtocol
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 反向接收空闲计时：只有**实际入队**的包才刷新计时。
 *
 * 发送端线程重启沿用同一 reverse SSRC，但可能重置 RTP 时间戳。新时间线的包若被
 * 持续判为 late，就不能延长空闲，否则 1 s 重置永不触发，反向播放会一直静音。
 */
class ReverseLinkTest {

    @Test
    fun droppedPackets_doNotExtendIdle_thenNextPacketResets() {
        val acceptedAt = 10_000L
        var lastAt = acceptedAt
        var now = acceptedAt

        repeat(REVERSE_IDLE_RESET_MS.toInt() / 10) {
            now += 10
            lastAt = nextReverseIdleAt(lastAt, now, accepted = false)
            assertEquals(acceptedAt, lastAt)
        }
        assertFalse(reverseIdleExpired(lastAt, now))

        now += 10
        assertTrue(reverseIdleExpired(lastAt, now))
        lastAt = nextReverseIdleAt(lastAt, now, accepted = true)
        assertEquals(now, lastAt)
        assertFalse(reverseIdleExpired(lastAt, now))
    }

    @Test
    fun acceptedPackets_refreshIdle() {
        var lastAt = 0L
        var now = 5_000L
        lastAt = nextReverseIdleAt(lastAt, now, accepted = true)
        assertEquals(now, lastAt)

        now += REVERSE_IDLE_RESET_MS
        assertFalse(reverseIdleExpired(lastAt, now))
        now += 1
        assertTrue(reverseIdleExpired(lastAt, now))
    }

    @Test
    fun noAcceptedMediaYet_isNeverIdle() {
        assertFalse(reverseIdleExpired(0L, Long.MAX_VALUE / 2))
    }

    @Test
    fun realJitterBuffer_backwardNewTimelineIsDropped_untilReset() {
        val jb = Udp2CalProtocol.JitterBuffer()
        val out = ByteArray(Udp2CalProtocol.OPUS_MAX_PAYLOAD)
        out[0] = 1
        try {
            assertTrue(jb.insert(0, 0L, true, out, 0, 1))
            assertTrue(jb.insert(1, 480L, false, out, 0, 1))
            assertTrue(jb.insert(2, 960L, false, out, 0, 1))
            assertEquals(Udp2CalProtocol.Pull.PACKET, jb.pull(0, 0, out))
            assertEquals(Udp2CalProtocol.Pull.PACKET, jb.pull(0, 0, out))
            assertEquals(960L, jb.nextTs)

            assertFalse(jb.insert(77, 0L, true, out, 0, 1))
            assertFalse(jb.insert(78, 480L, false, out, 0, 1))
            assertTrue(reverseIdleExpired(1_000L, 2_001L))
            jb.reset()
            assertTrue(jb.insert(79, 0L, true, out, 0, 1))
        } finally {
            jb.close()
        }
    }

    @Test
    fun tailHoldPlc_doesNotAllowFec() {
        assertFalse(plcAllowsFec(48_000L, 48_000L))
    }

    @Test
    fun middleHolePlc_advancesOneFrame_allowsFec() {
        assertTrue(
            plcAllowsFec(
                48_000L,
                48_000L + Udp2CalProtocol.OPUS_FRAME_SAMPLES,
            )
        )
    }
}
