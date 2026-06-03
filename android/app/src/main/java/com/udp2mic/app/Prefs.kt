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
        get() = prefs.getInt("target_port", 8899)
        set(v) = prefs.edit().putInt("target_port", v).apply()

    var testToneMode: Boolean
        get() = prefs.getBoolean("test_tone", false)
        set(v) = prefs.edit().putBoolean("test_tone", v).apply()

    var noiseReduction: Boolean
        get() = prefs.getBoolean("noise_reduction", false)
        set(v) = prefs.edit().putBoolean("noise_reduction", v).apply()

    var voiceEnhance: Boolean
        get() = prefs.getBoolean("voice_enhance", false)
        set(v) = prefs.edit().putBoolean("voice_enhance", v).apply()
}
