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

        /** ACK 有效期：超过该窗口未收到新 ACK 即视为断连（与 CaptureService 的超时判定一致） */
        const val ACK_WINDOW_MS = 3000L
    }

    // 跨线程访问：connect()/close() 在 IO 线程，send() 在音频消费者线程，
    // ACK 线程回调 lastAckTime。全部加 @Volatile 保证可见性。
    @Volatile private var socket: DatagramSocket? = null
    @Volatile private var deviceId: ByteArray? = null
    @Volatile private var remoteAddr: InetSocketAddress? = null
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

    /**
     * 发送一帧。刻意**不做** `withContext(Dispatchers.IO)`：
     * 该方法由音频消费者协程（后台线程）直接调用，UDP `send` 是微秒级非阻塞系统调用，
     * 每帧额外一次协程调度只会在 10ms 预算里平白引入抖动。
     */
    fun send(data: ByteArray, offset: Int, length: Int): Boolean {
        return try {
            val sock = socket ?: return false
            val addr = remoteAddr ?: return false
            sock.send(DatagramPacket(data, offset, length, addr))
            true
        } catch (_: Exception) {
            false
        }
    }

    /**
     * ACK 是否"新鲜"。
     *
     * 历史实现是 `lastAckTime > 0`——只表示"历史上收到过 ACK"，永不复位。
     * 一旦超时把 p2pConnected 置 false，下一帧又会被它判成 true，形成 100Hz 抖动。
     */
    fun isAckFresh(nowMs: Long = System.currentTimeMillis(), windowMs: Long = ACK_WINDOW_MS): Boolean {
        val t = lastAckTime
        return t > 0L && nowMs - t <= windowMs
    }

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
