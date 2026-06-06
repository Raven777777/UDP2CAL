package com.udp2mic.app

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

/**
 * UDP 发送器，支持非阻塞 ACK 漏极检测
 */
class UdpSender(
    private val host: String,
    private val port: Int
) {
    private var socket: DatagramSocket? = null
    private var deviceId: ByteArray? = null
    @Volatile private var lastAckTime: Long = 0L

    suspend fun connect(): Boolean = withContext(Dispatchers.IO) {
        try {
            socket = DatagramSocket()
            deviceId = DiscoveryManager.getOrCreateDeviceId()
            val connectPacket = Udp2MicProtocol.buildPacket(
                isAudio = false, msgType = Udp2MicProtocol.TYPE_CONNECT,
                sampleRate = 0, seqNum = 0,
                deviceId = deviceId!!, payload = ByteArray(0)
            )
            val addr = InetAddress.getByName(host)
            socket!!.send(DatagramPacket(connectPacket, connectPacket.size, addr, port))
            true
        } catch (_: Exception) { false }
    }

    suspend fun send(data: ByteArray, offset: Int, length: Int): Boolean = withContext(Dispatchers.IO) {
        try {
            val sock = socket ?: return@withContext false
            val addr = InetAddress.getByName(host)
            sock.send(DatagramPacket(data, offset, length, addr, port))
            true
        } catch (_: Exception) { false }
    }

    /**
     * 非阻塞漏极：用 1ms 超时尝试接收 ACK，不阻塞音频流水线
     * 应在每帧调用(20ms周期)，极低开销
     * @return true 如果收到有效的 CONNECT_ACK
     */
    fun drainAck(): Boolean {
        val sock = socket ?: return false
        return try {
            sock.soTimeout = 1
            val buf = ByteArray(Udp2MicProtocol.HEADER_SIZE)
            val pkt = DatagramPacket(buf, buf.size)
            sock.receive(pkt)
            val hdr = Udp2MicProtocol.decodeHeader(buf.copyOfRange(0, pkt.length.coerceAtMost(Udp2MicProtocol.HEADER_SIZE)))
            if (hdr != null && hdr.msgType == Udp2MicProtocol.TYPE_CONNECT_ACK) {
                lastAckTime = System.currentTimeMillis()
                true
            } else {
                false
            }
        } catch (_: Exception) { false }
    }

    fun getLastAckTime(): Long = lastAckTime
    fun getDeviceId(): ByteArray? = deviceId

    fun close() {
        try { socket?.close() } catch (_: Exception) {}
        socket = null
    }
}
