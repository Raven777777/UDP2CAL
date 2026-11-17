package com.udp2cal.app.service

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ForwardLinkTest {

    private fun cfg(signal: Int) = EncoderCfg(
        bitrateKbps = 256,
        complexity = 10,
        signal = signal,
        bandwidth = 1105,
        dtx = 0,
        vbr = 0,
        vbrConstraint = 0,
        fec = 0,
        packetLoss = 0,
    )

    @Test
    fun voiceMode_comesFromEncoderSnapshot() {
        assertTrue(cfg(3001).voiceMode())
        // 3002/3003/3004 是音乐/原始/识别：不得走 VOICE_COMMUNICATION 与 AEC
        assertFalse(cfg(3002).voiceMode())
        assertFalse(cfg(3003).voiceMode())
        assertFalse(cfg(3004).voiceMode())
    }
}
