package com.udp2cal.app

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

/**
 * UDP 发送器 — 正向/反向使用独立 socket
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

            // CONNECT payload: 2字节 reverse port (big-endian) + 1字节低性能标志
            val payload = byteArrayOf(
                (revPort shr 8).toByte(), revPort.toByte(),
                1 // 低性能模式（适配低端设备）
            )

            val cp = Udp2CalProtocol.buildPacket(
                isAudio = false, msgType = Udp2CalProtocol.TYPE_CONNECT,
                sampleRate = 0, seqNum = 0,
                deviceId = deviceId!!, payload = payload
            )
            val addr = InetAddress.getByName(host)
            socket!!.send(DatagramPacket(cp, cp.size, addr, port))
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

    /** 只读 ACK（不读音频），用 5ms 超时降低空轮询 CPU */
    fun drainAck(): Boolean {
        val sock = socket ?: return false
        return try {
            sock.soTimeout = 5
            val buf = ByteArray(Udp2CalProtocol.HEADER_SIZE)
            val pkt = DatagramPacket(buf, buf.size)
            sock.receive(pkt)
            val hdr = Udp2CalProtocol.decodeHeader(
                buf.copyOfRange(0, pkt.length.coerceAtMost(Udp2CalProtocol.HEADER_SIZE))
            )
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
