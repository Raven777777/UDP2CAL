package com.udp2mic.app

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.net.SocketTimeoutException

object DiscoveryManager {
    private const val BROADCAST_PORT = 44043
    private const val DISCOVER_REQ = "UDP2MIC_DISCOVER"
    private const val REPLY_PREFIX = "UDP2MIC_REPLY:"

    /**
     * 发起局域网广播搜索 PC 端
     * @return 成功则返回 Pair(PC_IP, PC_PORT)，失败或超时返回 null
     */
    suspend fun discoverServer(): Pair<String, Int>? = withContext(Dispatchers.IO) {
        var socket: DatagramSocket? = null
        try {
            socket = DatagramSocket()
            socket.broadcast = true // 关键：允许发送广播包
            socket.soTimeout = 2000 // 关键：设置2秒超时，防止协程无限挂起

            // 1. 发送广播暗号到 255.255.255.255
            val reqData = DISCOVER_REQ.toByteArray()
            val broadcastAddr = InetAddress.getByName("255.255.255.255")
            val sendPacket = DatagramPacket(reqData, reqData.size, broadcastAddr, BROADCAST_PORT)
            socket.send(sendPacket)

            // 2. 准备接收 PC 的单播回复
            val recvBuf = ByteArray(1024)
            val recvPacket = DatagramPacket(recvBuf, recvBuf.size)

            socket.receive(recvPacket) // 此处会阻塞，直到收到回复或2秒超时

            val replyStr = String(recvPacket.data, 0, recvPacket.length).trim()
            if (replyStr.startsWith(REPLY_PREFIX)) {
                val pcIp = recvPacket.address.hostAddress ?: return@withContext null
                val pcPort = replyStr.substring(REPLY_PREFIX.length).toIntOrNull() ?: 44044
                return@withContext Pair(pcIp, pcPort)
            }
        } catch (e: SocketTimeoutException) {
            // 超时未搜到
        } catch (e: Exception) {
            e.printStackTrace()
        } finally {
            socket?.close()
        }
        return@withContext null
    }
}
