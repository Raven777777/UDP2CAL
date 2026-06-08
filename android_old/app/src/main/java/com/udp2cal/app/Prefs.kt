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
        get() = prefs.getString("target_ip", "") ?: ""
        set(v) = prefs.edit().putString("target_ip", v).apply()

    var targetPort: Int
        get() = prefs.getInt("target_port", 44044)
        set(v) = prefs.edit().putInt("target_port", v).apply()

    var deviceId: String
        get() = prefs.getString("device_id", "") ?: ""
        set(v) = prefs.edit().putString("device_id", v).apply()

    var connectedDeviceId: String
        get() = prefs.getString("connected_device_id", "") ?: ""
        set(v) = prefs.edit().putString("connected_device_id", v).apply()
}
