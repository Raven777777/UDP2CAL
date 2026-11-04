package com.udp2cal.app

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class UdpSenderTest {

    private val deviceId = byteArrayOf(1, 2, 3, 4, 5, 6, 7, 8)

    @Test
    fun isExpectedAckPeer_acceptsReplyFromExpectedDevice() {
        assertTrue(
            isExpectedAckPeer(
                "192.168.1.20", 44044,
                "192.168.1.20", 44044,
                deviceId, deviceId
            )
        )
    }

    @Test
    fun isExpectedAckPeer_rejectsAnotherDeviceAtSameEndpoint() {
        assertFalse(
            isExpectedAckPeer(
                "192.168.1.20", 44044,
                "192.168.1.20", 44044,
                byteArrayOf(9, 9, 9, 9, 9, 9, 9, 9), deviceId
            )
        )
    }

    @Test
    fun isExpectedAckPeer_rejectsWrongEndpointAndEmptyDeviceId() {
        assertFalse(
            isExpectedAckPeer(
                "192.168.1.21", 44044,
                "192.168.1.20", 44044,
                deviceId, deviceId
            )
        )
        assertFalse(
            isExpectedAckPeer(
                "192.168.1.20", 44045,
                "192.168.1.20", 44044,
                deviceId, deviceId
            )
        )
        assertFalse(
            isExpectedAckPeer(
                "192.168.1.20", 44044,
                "192.168.1.20", 44044,
                ByteArray(0), deviceId
            )
        )
        assertFalse(
            isExpectedAckPeer(
                "192.168.1.20", 44044,
                "192.168.1.20", 44044,
                byteArrayOf(1, 2, 3), deviceId
            )
        )
    }

    @Test
    fun isExpectedAckPeer_onlyNullExpectedIdIsWildcard() {
        assertTrue(
            isExpectedAckPeer(
                "192.168.1.20", 44044,
                "192.168.1.20", 44044,
                ByteArray(0), null
            )
        )
        // 只有 null 是通配；空 ByteArray 仍要求对端 device_id 也是空
        assertFalse(
            isExpectedAckPeer(
                "192.168.1.20", 44044,
                "192.168.1.20", 44044,
                deviceId, ByteArray(0)
            )
        )
        assertFalse(
            isExpectedAckPeer(
                null, 44044,
                "192.168.1.20", 44044,
                deviceId, null
            )
        )
    }
}
