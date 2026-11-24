package com.udp2cal.app.service

import com.udp2cal.app.DiscoveryManager
import com.udp2cal.app.EncoderCfg
import com.udp2cal.app.R
import com.udp2cal.app.Udp2CalProtocol
import com.udp2cal.app.UdpSender
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import java.net.DatagramPacket
import kotlin.coroutines.coroutineContext

/**
 * 控制面：CONNECT 保活 + ACK 状态 + 反向开关。
 *
 * 只读 UdpSender 的 ACK/SSRC 状态，把反向动作全部交给 [ReverseLink]；
 * 自身不持有任何跨协程状态。
 */
internal class ControlLoop(
    private val sender: UdpSender,
    private val status: StatusBus,
    private val reverse: ReverseLink,
    /** Wi-Fi 断网等待恢复期间保留更具指导性的提示，不被通用“已断开”覆盖 */
    private val isWifiAwaitingReconnect: () -> Boolean,
) {
    companion object {
        /** ACK 轮询周期：只用于检测连接状态与发送保活 */
        private const val TICK_MS = 200L
    }

    suspend fun run() {
        var connected = false
        var sinceConnect = 0L
        // CONNECT 每秒按当前 reversePort/forwardSsrc 重建负载；包对象复用，
        // 只换数据与长度（发送时 send 会补上目标地址）。
        val connectPacket = DatagramPacket(ByteArray(0), 0)
        while (coroutineContext.isActive) {
            if (sinceConnect <= 0) {
                val payload = buildConnectPayload()
                val packet = Udp2CalProtocol.buildControl(
                    Udp2CalProtocol.TYPE_CONNECT,
                    DiscoveryManager.getOrCreateDeviceId(),
                    payload
                )
                connectPacket.data = packet
                connectPacket.length = packet.size
                sender.send(connectPacket)
                sinceConnect = 1000
            }
            sinceConnect -= TICK_MS

            val fresh = sender.isAckFresh()
            if (fresh != connected) {
                connected = fresh
                if (fresh) {
                    status.update {
                        it.copy(connected = true, errorRes = 0, errorArgs = emptyList())
                    }
                } else {
                    status.update {
                        it.copy(
                            connected = false, reverseAudio = false,
                            errorRes = if (isWifiAwaitingReconnect()) {
                                R.string.status_wifi_lost
                            } else {
                                R.string.status_disconnected_waiting
                            },
                            errorArgs = emptyList(),
                        )
                    }
                    reverse.resetSession()
                    // ACK 超时说明旧会话已不可用：换一个正向 SSRC，让对端在
                    // 下一次 CONNECT 时走显式新会话路径（清抖动缓冲/重建反向流）。
                    sender.regenerateForwardSsrc()
                }
            }

            if (connected) reverse.applyRemoteSsrc(sender.reverseSsrc)

            delay(TICK_MS)
        }
    }

    /** CONNECT 负载固定 15 字节；Opus 参数只描述**反向**（Windows 编码器） */
    private fun buildConnectPayload(): ByteArray {
        // 自动档已移除：始终发送具体档位（0 仅作跨版本兼容保留在协议里）
        val cfg = EncoderCfg.fromPrefs()
        var flags = 0
        if (cfg.vbr == 1) flags = flags or Udp2CalProtocol.OPUS_FLAG_VBR
        if (cfg.dtx == 1) flags = flags or Udp2CalProtocol.OPUS_FLAG_DTX
        if (cfg.vbrConstraint == 1) flags = flags or Udp2CalProtocol.OPUS_FLAG_VBR_CONSTRAINT
        if (cfg.fec > 0) flags = flags or Udp2CalProtocol.OPUS_FLAG_FEC

        return Udp2CalProtocol.ConnectPayload(
            reversePort = sender.reversePort,
            forwardSsrc = sender.forwardSsrc,
            complexity = cfg.complexity,
            signal = mapSignal(cfg.signal),
            bandwidth = mapBandwidth(cfg.bandwidth),
            opusFlags = flags,
            bitrateKbps = cfg.bitrateKbps,
            packetLoss = cfg.packetLoss,
        ).encode()
    }

    private fun mapSignal(sig: Int): Int = when (sig) {
        3001 -> 1 // OPUS_SIGNAL_VOICE
        else -> 2 // OPUS_SIGNAL_MUSIC（3002/3003/3004 均按音乐信号提示编码器）
    }

    private fun mapBandwidth(bw: Int): Int = when (bw) {
        1101 -> 0
        1102 -> 1
        1103 -> 2
        1104 -> 3
        else -> 4
    }
}
