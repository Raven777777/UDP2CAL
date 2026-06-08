package com.udp2cal.app

import android.content.Context
import android.content.SharedPreferences

object Prefs {
    private const val NAME = "udp2cal_prefs"
    private lateinit var prefs: SharedPreferences

    fun init(ctx: Context) {
        prefs = ctx.applicationContext.getSharedPreferences(NAME, Context.MODE_PRIVATE)
    }

    var targetIp: String
        get() = prefs.getString("target_ip", "192.168.1.100") ?: "192.168.1.100"
        set(v) = prefs.edit().putString("target_ip", v).apply()

    var targetPort: Int
        get() = prefs.getInt("target_port", 44044)
        set(v) = prefs.edit().putInt("target_port", v).apply()

    var testToneMode: Boolean
        get() = prefs.getBoolean("test_tone", false)
        set(v) = prefs.edit().putBoolean("test_tone", v).apply()

    // ── Opus 编码器设置 ──
    var opusComplexity: Int
        get() = prefs.getInt("opus_complexity", 5)
        set(v) = prefs.edit().putInt("opus_complexity", v.coerceIn(1, 10)).apply()

    var opusSignal: Int
        get() = prefs.getInt("opus_signal", 3001) // OPUS_SIGNAL_VOICE (默认语音)
        set(v) = prefs.edit().putInt("opus_signal", v).apply()

    var opusBandwidth: Int
        get() = prefs.getInt("opus_bandwidth", 1105) // OPUS_BANDWIDTH_FULLBAND
        set(v) = prefs.edit().putInt("opus_bandwidth", v).apply()

    var opusDtx: Int
        get() = prefs.getInt("opus_dtx", 0) // 0=OFF, 1=ON
        set(v) = prefs.edit().putInt("opus_dtx", v).apply()

    var opusVbr: Int
        get() = prefs.getInt("opus_vbr", 0) // 0=CBR, 1=VBR
        set(v) = prefs.edit().putInt("opus_vbr", v).apply()

    var opusBitrateKbps: Int
        get() = prefs.getInt("opus_bitrate_kbps", 256) // 0 = auto (根据采样率自动选择)
        set(v) = prefs.edit().putInt("opus_bitrate_kbps", v.coerceIn(0, 512)).apply() // OPUS 协议上限 510kbps（立体声），单声道理论~255k；512 为未来双声道预留

    var opusFec: Int
        get() {
            val v = prefs.getInt("opus_fec", 2)
            // 自动迁移：旧值 1（强制 SILK）→ 2（允许 CELT + FEC）因为 1 会锁死 300k 码率上限
            if (v == 1) { prefs.edit().putInt("opus_fec", 2).apply(); return 2 }
            return v
        }
        set(v) = prefs.edit().putInt("opus_fec", v.coerceIn(0, 2)).apply()

    var opusPacketLoss: Int
        get() = prefs.getInt("opus_packet_loss", 5) // 0..100 预期丢包率（%）
        set(v) = prefs.edit().putInt("opus_packet_loss", v.coerceIn(0, 100)).apply()

    var opusVbrConstraint: Int
        get() = prefs.getInt("opus_vbr_constraint", 0) // 0=无约束, 1=约束（不超码率）
        set(v) = prefs.edit().putInt("opus_vbr_constraint", v.coerceIn(0, 1)).apply()

    // ═══ P2P 独占通信配置 ═══
    var deviceId: String
        get() = prefs.getString("device_id", "") ?: ""
        set(v) = prefs.edit().putString("device_id", v).apply()

    var connectedDeviceId: String
        get() = prefs.getString("connected_device_id", "") ?: ""
        set(v) = prefs.edit().putString("connected_device_id", v).apply()
}
