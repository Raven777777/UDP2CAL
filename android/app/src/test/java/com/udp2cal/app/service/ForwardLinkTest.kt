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

    @Test
    fun captureStall_onlyReportedForLiveSession() {
        // AudioRecord.read 的负返回：-3 ERROR_INVALID_OPERATION / -6 ERROR_DEAD_OBJECT
        assertTrue(shouldReportCaptureStall(-3, stillActive = true))
        assertTrue(shouldReportCaptureStall(-6, stillActive = true))
        // 停止采集时 stop() 唤醒阻塞读也会返回负值，但协程已取消 → 必须安静
        assertFalse(shouldReportCaptureStall(-3, stillActive = false))
        assertFalse(shouldReportCaptureStall(-6, stillActive = false))
        // 正常读（含不足一帧的短读/0）永远不是设备失效
        assertFalse(shouldReportCaptureStall(0, stillActive = true))
        assertFalse(shouldReportCaptureStall(480, stillActive = true))
    }
}
