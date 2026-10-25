package com.udp2cal.app

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.net.InetSocketAddress
import java.security.SecureRandom

/**
 * UDP 发送器（v3）
 *
 * 主 socket 同时承担两件事：
 *   * 发送 CONNECT 控制包与**正向 RTP**；
 *   * 接收 CONNECT_ACK（控制面）。
 *
 * 反向音频使用独立 socket，**只接收 RTP**，端口在 CONNECT 的 `reverse_port` 中通告。
 *
 * 设计要点：
 *   * 目标地址只解析一次，避免每帧 `InetAddress.getByName`；
 *   * ACK 由独立线程阻塞接收，音频线程只读 `lastAckTime`/`reverseSsrc`；
 *   * `send` 不做协程切换：UDP send 是微秒级系统调用，每帧额外调度只会引入抖动。
 */
class UdpSender(
    private val host: String,
    private val port: Int
) {
    private companion object {
        const val TAG = "UdpSender"

        /** ACK 有效期：超过该窗口未收到新 ACK 即视为断连 */
        const val ACK_WINDOW_MS = 3000L
    }

    // 跨线程访问：connect()/close() 在 IO 线程，send() 在采集线程，
    // ACK 线程回调 lastAckTime。全部 @Volatile 保证可见性。
    @Volatile private var socket: DatagramSocket? = null
    @Volatile private var deviceId: ByteArray? = null
    @Volatile private var remoteAddr: InetSocketAddress? = null
    @Volatile private var remoteIp: String? = null
    private var ackThread: Thread? = null

    @Volatile private var running = false
    @Volatile private var lastAckTime: Long = 0L

    /** 反向音频专用 socket（只收 RTP） */
    @Volatile var reverseSock: DatagramSocket? = null; private set
    @Volatile var reversePort: Int = 0; private set

    /** 本次会话的正向 SSRC：每次 connect 重新生成，重连必须换新值 */
    @Volatile var forwardSsrc: Int = 0; private set

    /** ACK 下发的反向 SSRC；0 表示反向未启用 */
    @Volatile var reverseSsrc: Int = 0; private set

    /** ACK 代际：每收到一个合法 ACK 自增，供上层检测"SSRC 变化" */
    @Volatile var ackGeneration: Long = 0L; private set

    suspend fun connect(): Boolean = withContext(Dispatchers.IO) {
        var sockLocal: DatagramSocket? = null
        var revSockLocal: DatagramSocket? = null
        try {
            // 只解析一次；之后每帧发送不再触碰 DNS / IP 解析
            val addr = InetSocketAddress(InetAddress.getByName(host), port)
            val sock = DatagramSocket()
            sockLocal = sock
            val revSock = DatagramSocket()
            revSockLocal = revSock
            reverseSock = revSock
            reversePort = revSock.localPort
            forwardSsrc = newSsrc()
            socket = sock
            remoteAddr = addr
            remoteIp = addr.address?.hostAddress
            deviceId = DiscoveryManager.getOrCreateDeviceId()
            startAckLoop()
            Log.i(TAG, "已连接 $addr，正向 SSRC=${"%08x".format(forwardSsrc)}，反向端口=$reversePort")
            true
        } catch (e: Exception) {
            // 失败路径必须回收已创建的 socket（第二个 DatagramSocket 抛错时第一个会泄漏）
            Log.e(TAG, "connect 失败", e)
            try { sockLocal?.close() } catch (_: Exception) {}
            try { revSockLocal?.close() } catch (_: Exception) {}
            socket = null
            reverseSock = null
            remoteAddr = null
            false
        }
    }

    /** 发送任意数据报（控制包或 RTP 包）。音频线程直接调用，不切协程。 */
    fun send(data: ByteArray, offset: Int, length: Int): Boolean {
        if (length <= 0) return false
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
     * ACK 是否"新鲜"。只表示"最近 3 秒内收到过 ACK"，
     * 不能退化成"历史上收到过 ACK"——否则超时判定会与它互相打脸。
     */
    fun isAckFresh(nowMs: Long = System.currentTimeMillis(), windowMs: Long = ACK_WINDOW_MS): Boolean {
        val t = lastAckTime
        return t > 0L && nowMs - t <= windowMs
    }

    fun getLastAckTime(): Long = lastAckTime

    fun getDeviceId(): ByteArray? = deviceId

    /** 目标 IP（已解析），供反向流的来源校验 */
    fun getRemoteIp(): String? = remoteIp

    fun getRemotePort(): Int = port

    private fun newSsrc(): Int {
        val rnd = SecureRandom()
        while (true) {
            val v = rnd.nextInt()
            if (v != 0) return v
        }
    }

    /**
     * ACK 接收线程：**阻塞** receive，socket 被 close 时抛异常退出，
     * 不需要靠超时轮询唤醒。
     */
    private fun startAckLoop() {
        if (ackThread != null) return
        running = true
        ackThread = Thread({
            val buf = ByteArray(2048)
            val pkt = DatagramPacket(buf, buf.size)
            while (running) {
                val sock = socket ?: break
                try {
                    pkt.length = buf.size
                    sock.receive(pkt)
                } catch (e: Exception) {
                    if (!running || sock.isClosed) break
                    continue
                }
                val len = pkt.length
                val hdr = Udp2CalProtocol.parseControl(buf, len) ?: continue
                if (hdr.msgType != Udp2CalProtocol.TYPE_CONNECT_ACK.toInt()) continue
                // 源地址必须是目标主机
                if (remoteIp != null && pkt.address?.hostAddress != remoteIp) continue
                val ack = Udp2CalProtocol.ConnectAckPayload.decode(
                    buf, Udp2CalProtocol.CONTROL_HEADER_SIZE, hdr.payloadLen
                ) ?: continue
                reverseSsrc = ack.reverseSsrc
                lastAckTime = System.currentTimeMillis()
                ackGeneration++
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
        reverseSsrc = 0
        forwardSsrc = 0
    }
}
