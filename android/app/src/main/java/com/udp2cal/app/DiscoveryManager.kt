package com.udp2cal.app

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

/**
 * 局域网发现（v3）
 *
 * Android 从临时 socket 广播 DISCOVER_REQ 到 255.255.255.255:44043；
 * Windows 单播回复 DISCOVER_REPLY，负载里带 `listen_port` 与设备名——
 * **后续 CONNECT 的目标端口取自该字段**，而不是 44044。
 */
object DiscoveryManager {
    private const val BROADCAST_PORT = Udp2CalProtocol.DISCOVER_PORT

    @Suppress("ArrayInDataClass")
    data class DiscoverResult(
        val ip: String,
        val port: Int,
        val deviceId: ByteArray,
        val deviceName: String
    )

    /**
     * 发起局域网广播搜索 PC 端，收集所有回复
     */
    suspend fun discoverServers(): List<DiscoverResult> = withContext(Dispatchers.IO) {
        val results = mutableListOf<DiscoverResult>()
        var socket: DatagramSocket? = null
        try {
            socket = DatagramSocket()
            socket.broadcast = true
            socket.soTimeout = 500

            val deviceId = getOrCreateDeviceId()
            val reqPacket = Udp2CalProtocol.buildControl(
                Udp2CalProtocol.TYPE_DISCOVER_REQ, deviceId, ByteArray(0)
            )
            val broadcastAddr = InetAddress.getByName("255.255.255.255")

            repeat(3) {
                socket.send(
                    DatagramPacket(reqPacket, reqPacket.size, broadcastAddr, BROADCAST_PORT)
                )

                val recvBuf = ByteArray(1024)
                while (true) {
                    try {
                        val recvPacket = DatagramPacket(recvBuf, recvBuf.size)
                        socket.receive(recvPacket)
                        val len = recvPacket.length
                        val hdr = Udp2CalProtocol.parseControl(recvBuf, len) ?: continue
                        if (hdr.msgType != Udp2CalProtocol.TYPE_DISCOVER_REPLY.toInt()) continue

                        val pcIp = recvPacket.address?.hostAddress ?: continue
                        if (results.any { it.ip == pcIp }) continue

                        val reply = Udp2CalProtocol.DiscoverReply.decode(
                            recvBuf, Udp2CalProtocol.CONTROL_HEADER_SIZE, hdr.payloadLen
                        ) ?: continue

                        results.add(
                            DiscoverResult(pcIp, reply.listenPort, hdr.deviceId, reply.deviceName)
                        )
                    } catch (_: java.net.SocketTimeoutException) {
                        break
                    }
                }
            }
        } catch (e: Exception) {
            e.printStackTrace()
        } finally {
            socket?.close()
        }
        return@withContext results
    }

    fun getOrCreateDeviceId(): ByteArray {
        val storedId = Prefs.deviceId
        if (storedId.isNotEmpty() && storedId.length == Udp2CalProtocol.DEVICE_ID_SIZE * 2) {
            try { return hexStringToByteArray(storedId) } catch (_: Exception) {}
        }
        val newId = Udp2CalProtocol.generateDeviceId()
        Prefs.deviceId = Udp2CalProtocol.deviceIdToString(newId)
        return newId
    }

    private fun hexStringToByteArray(s: String): ByteArray {
        val len = s.length
        val data = ByteArray(len / 2)
        var i = 0
        while (i < len) {
            data[i / 2] = ((Character.digit(s[i], 16) shl 4) + Character.digit(s[i + 1], 16)).toByte()
            i += 2
        }
        return data
    }
}
