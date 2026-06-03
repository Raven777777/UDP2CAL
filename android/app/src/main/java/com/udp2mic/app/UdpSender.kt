package com.udp2mic.app

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

class UdpSender(
    private val host: String,
    private val port: Int
) {
    private var socket: DatagramSocket? = null

    suspend fun connect(): Boolean = withContext(Dispatchers.IO) {
        try {
            socket = DatagramSocket()
            true
        } catch (_: Exception) {
            false
        }
    }

    suspend fun send(data: ByteArray): Boolean = withContext(Dispatchers.IO) {
        try {
            val sock = socket ?: return@withContext false
            val addr = InetAddress.getByName(host)
            sock.send(DatagramPacket(data, data.size, addr, port))
            true
        } catch (_: Exception) {
            false
        }
    }

    fun close() {
        try { socket?.close() } catch (_: Exception) {}
        socket = null
    }
}
