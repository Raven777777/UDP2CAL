package com.udp2cal.app

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class DiscoveryManagerTest {

    @Test
    fun hexStringToByteArray_parsesStoredDeviceId() {
        assertArrayEquals(
            byteArrayOf(0x00, 0x11, 0xaa.toByte(), 0xff.toByte()),
            DiscoveryManager.hexStringToByteArray("0011aaff")
        )
        assertArrayEquals(
            byteArrayOf(0xde.toByte(), 0xad.toByte(), 0xbe.toByte(), 0xef.toByte()),
            DiscoveryManager.hexStringToByteArray("DEADBEEF")
        )
    }

    @Test
    fun hexStringToByteArray_emptyAndOddLength() {
        assertArrayEquals(ByteArray(0), DiscoveryManager.hexStringToByteArray(""))
        // 奇数长度没有完整字节，读取 s[i+1] 越界，调用方会回退到新生成 id
        assertThrows(StringIndexOutOfBoundsException::class.java) {
            DiscoveryManager.hexStringToByteArray("abc")
        }
    }
}
