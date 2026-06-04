package com.udp2mic.app

import android.content.Context
import android.content.SharedPreferences

object Prefs {
    private const val NAME = "udp2mic_prefs"
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

    var noiseGate: Boolean
        get() = prefs.getBoolean("noise_gate", true)
        set(v) = prefs.edit().putBoolean("noise_gate", v).apply()

    // ── Opus 编码器设置 ──
    var opusComplexity: Int
        get() = prefs.getInt("opus_complexity", 10)
        set(v) = prefs.edit().putInt("opus_complexity", v.coerceIn(1, 10)).apply()

    var opusSignal: Int
        get() = prefs.getInt("opus_signal", 3002) // OPUS_SIGNAL_MUSIC
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
        get() = prefs.getInt("opus_bitrate_kbps", 0) // 0 = auto (根据采样率自动选择)
        set(v) = prefs.edit().putInt("opus_bitrate_kbps", v.coerceIn(0, 512)).apply()

    // ── AGC 设置 ──
    var agcEnabled: Boolean
        get() = prefs.getBoolean("agc_enabled", true)
        set(v) = prefs.edit().putBoolean("agc_enabled", v).apply()

    var agcMaxGain: Int
        get() = prefs.getInt("agc_max_gain", 40) // 0=1x .. 200=200x
        set(v) = prefs.edit().putInt("agc_max_gain", v.coerceIn(0, 200)).apply()
}
