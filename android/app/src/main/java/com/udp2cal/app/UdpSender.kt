package com.udp2cal.app

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

/**
 * UDP 发送器，支持非阻塞 ACK 漏极检测
 * 正向：发送麦克风音频 + 接收 ACK
 * 反向：独立 socket 接收 PC 端来的扬声器音频（在 CaptureService 中单独协程处理）
 */
class UdpSender(
    private val host: String,
    private val port: Int
) {
    private var socket: DatagramSocket? = null
    private var deviceId: ByteArray? = null
    @Volatile private var lastAckTime: Long = 0L

    /** 反向音频专用 socket（在 connect 时创建） */
    @Volatile var reverseSock: DatagramSocket? = null; private set
    @Volatile var reversePort: Int = 0; private set

    suspend fun connect(): Boolean = withContext(Dispatchers.IO) {
        try {
            socket = DatagramSocket()
            deviceId = DiscoveryManager.getOrCreateDeviceId()

            // 创建反向后门 socket 并获取端口
            val revSock = DatagramSocket()
            val revPort = revSock.localPort
            reverseSock = revSock
            reversePort = revPort

            // CONNECT payload: 2字节 reverse port (big-endian)
            val payload = byteArrayOf((revPort shr 8).toByte(), revPort.toByte())

            val connectPacket = Udp2CalProtocol.buildPacket(
                isAudio = false, msgType = Udp2CalProtocol.TYPE_CONNECT,
                sampleRate = 0, seqNum = 0,
                deviceId = deviceId!!, payload = payload
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
            val buf = ByteArray(Udp2CalProtocol.HEADER_SIZE)
            val pkt = DatagramPacket(buf, buf.size)
            sock.receive(pkt)
            val hdr = Udp2CalProtocol.decodeHeader(buf.copyOfRange(0, pkt.length.coerceAtMost(Udp2CalProtocol.HEADER_SIZE)))
            if (hdr != null && hdr.msgType == Udp2CalProtocol.TYPE_CONNECT_ACK) {
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
        try { reverseSock?.close() } catch (_: Exception) {}
        reverseSock = null
        socket = null
    }
}
