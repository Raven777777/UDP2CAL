package com.udp2cal.app

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

object DiscoveryManager {
    private const val BROADCAST_PORT = 44043

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
            val reqPacket = Udp2CalProtocol.buildPacket(
                isAudio = false,
                msgType = Udp2CalProtocol.TYPE_DISCOVER_REQ,
                sampleRate = 0,
                seqNum = 0,
                deviceId = deviceId,
                payload = ByteArray(0)
            )
            val broadcastAddr = InetAddress.getByName("255.255.255.255")

            repeat(3) {
                val sendPacket = DatagramPacket(reqPacket, reqPacket.size, broadcastAddr, BROADCAST_PORT)
                socket.send(sendPacket)

                val recvBuf = ByteArray(1024)
                while (true) {
                    try {
                        val recvPacket = DatagramPacket(recvBuf, recvBuf.size)
                        socket.receive(recvPacket)
                        val data = recvPacket.data.copyOfRange(0, recvPacket.length)
                        val hdr = Udp2CalProtocol.decodeHeader(data)
                        if (hdr != null && hdr.msgType == Udp2CalProtocol.TYPE_DISCOVER_REPLY) {
                            val pcIp = recvPacket.address.hostAddress ?: continue
                            if (results.any { it.ip == pcIp }) continue
                            val payloadStart = Udp2CalProtocol.HEADER_SIZE
                            val payloadEnd = payloadStart + hdr.payloadLen
                            if (payloadEnd > data.size) continue
                            val port = if (payloadEnd - payloadStart >= 2) {
                                ((data[payloadStart].toInt() and 0xFF) shl 8) or (data[payloadStart + 1].toInt() and 0xFF)
                            } else 44044
                            val deviceName = if (payloadEnd - payloadStart > 2) {
                                String(data, payloadStart + 2, payloadEnd - payloadStart - 2)
                            } else "UDP2CAL PC"
                            val resultDeviceId = ByteArray(Udp2CalProtocol.DEVICE_ID_SIZE)
                            System.arraycopy(hdr.deviceId, 0, resultDeviceId, 0, Udp2CalProtocol.DEVICE_ID_SIZE)
                            results.add(DiscoverResult(pcIp, port, resultDeviceId, deviceName))
                        }
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
