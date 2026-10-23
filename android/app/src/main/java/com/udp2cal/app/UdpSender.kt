package com.udp2cal.app

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.net.InetSocketAddress

/**
 * UDP 发送器
 * 正向：发送麦克风音频 + 接收 CONNECT_ACK
 * 反向：独立 socket 接收 PC 端来的扬声器音频（在 CaptureService 中单独协程处理）
 *
 * 设计要点：
 *   - 目标地址在 connect() 时解析一次并缓存，避免每帧 `InetAddress.getByName`；
 *   - ACK 由独立线程接收，不再在音频流水线上用 1ms 超时 + 每秒上百次
 *     SocketTimeoutException 做流控（异常构造与 1ms 阻塞都会直接计入播放抖动）。
 */
class UdpSender(
    private val host: String,
    private val port: Int
) {
    private companion object {
        const val TAG = "UdpSender"
    }

    private var socket: DatagramSocket? = null
    private var deviceId: ByteArray? = null
    private var remoteAddr: InetSocketAddress? = null
    private var ackThread: Thread? = null

    @Volatile private var running = false
    @Volatile private var lastAckTime: Long = 0L

    /** 反向音频专用 socket（在 connect 时创建） */
    @Volatile var reverseSock: DatagramSocket? = null; private set
    @Volatile var reversePort: Int = 0; private set

    suspend fun connect(): Boolean = withContext(Dispatchers.IO) {
        try {
            // 只解析一次；之后每帧发送不再触碰 DNS / IP 解析
            val addr = InetSocketAddress(InetAddress.getByName(host), port)
            val sock = DatagramSocket()
            socket = sock
            remoteAddr = addr
            deviceId = DiscoveryManager.getOrCreateDeviceId()

            // 创建反向接收 socket 并获取端口
            val revSock = DatagramSocket()
            reverseSock = revSock
            reversePort = revSock.localPort

            // CONNECT payload: 2字节 reverse port (big-endian)
            val payload = byteArrayOf((reversePort shr 8).toByte(), reversePort.toByte())
            val connectPacket = Udp2CalProtocol.buildPacket(
                isAudio = false, msgType = Udp2CalProtocol.TYPE_CONNECT,
                sampleRate = 0, seqNum = 0,
                deviceId = deviceId!!, payload = payload
            )
            sock.send(DatagramPacket(connectPacket, connectPacket.size, addr))

            startAckLoop()
            true
        } catch (e: Exception) {
            Log.e(TAG, "connect 失败", e)
            false
        }
    }

    suspend fun send(data: ByteArray, offset: Int, length: Int): Boolean =
        withContext(Dispatchers.IO) {
            try {
                val sock = socket ?: return@withContext false
                val addr = remoteAddr ?: return@withContext false
                sock.send(DatagramPacket(data, offset, length, addr))
                true
            } catch (_: Exception) {
                false
            }
        }

    /** 是否已收到过 CONNECT_ACK */
    fun hasAck(): Boolean = lastAckTime > 0L

    fun getLastAckTime(): Long = lastAckTime

    fun getDeviceId(): ByteArray? = deviceId

    /** ACK 接收线程：500ms 超时轮询，只在收到 ACK 时更新 lastAckTime */
    private fun startAckLoop() {
        if (ackThread != null) return
        running = true
        ackThread = Thread({
            val buf = ByteArray(Udp2CalProtocol.HEADER_SIZE)
            val pkt = DatagramPacket(buf, buf.size)
            while (running) {
                val sock = socket
                if (sock == null) break
                try {
                    sock.soTimeout = 500
                    pkt.length = buf.size
                    sock.receive(pkt)
                    if (pkt.length < Udp2CalProtocol.HEADER_SIZE) continue
                    val hdr = Udp2CalProtocol.decodeHeader(buf) ?: continue
                    if (hdr.msgType == Udp2CalProtocol.TYPE_CONNECT_ACK) {
                        lastAckTime = System.currentTimeMillis()
                    }
                } catch (_: java.net.SocketTimeoutException) {
                    // 正常：没有 ACK 到达
                } catch (e: Exception) {
                    if (!running) break
                    // socket 被关闭/异常：短暂退避，避免忙等
                    try {
                        Thread.sleep(20)
                    } catch (_: InterruptedException) {
                        break
                    }
                }
            }
        }, "udp2cal-ack").apply {
            isDaemon = true
            start()
        }
    }

    fun close() {
        running = false
        try { socket?.close() } catch (_: Exception) {}
        try { reverseSock?.close() } catch (_: Exception) {}
        ackThread?.let { t ->
            try { t.join(200) } catch (_: InterruptedException) {}
        }
        ackThread = null
        reverseSock = null
        socket = null
        remoteAddr = null
        lastAckTime = 0L
    }
}
