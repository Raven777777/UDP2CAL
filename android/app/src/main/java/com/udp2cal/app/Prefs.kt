package com.udp2cal.app

import android.content.Context
import android.content.SharedPreferences

object Prefs {
    private const val NAME = "udp2cal_prefs"

    internal fun sanitizeOpusComplexity(value: Int): Int = value.coerceIn(1, 10)

    /** 码率默认档位（自动档已移除，新装与旧版"自动"都落到这里） */
    const val DEFAULT_BITRATE_KBPS = 256

    /** 主线程 init、工作线程读取；volatile 保证安全发布 */
    @Volatile private lateinit var prefs: SharedPreferences

    fun init(ctx: Context) {
        prefs = ctx.applicationContext.getSharedPreferences(NAME, Context.MODE_PRIVATE)
    }

    var targetIp: String
        get() = prefs.getString("target_ip", "192.168.1.100") ?: "192.168.1.100"
        set(v) = prefs.edit().putString("target_ip", v).apply()

    var targetPort: Int
        get() = prefs.getInt("target_port", 44044)
        set(v) = prefs.edit().putInt("target_port", v).apply()

    /** Last discovered Windows identity; blank for manually configured targets. */
    var targetDeviceId: String
        get() = prefs.getString("target_device_id", "") ?: ""
        set(v) = prefs.edit().putString("target_device_id", v).apply()

    var testToneMode: Boolean
        get() = prefs.getBoolean("test_tone", false)
        set(v) = prefs.edit().putBoolean("test_tone", v).apply()

    // ── Opus 编码器设置 ──
    var opusComplexity: Int
        get() = sanitizeOpusComplexity(
            prefs.getInt("opus_complexity", 10) // 默认最高复杂度，极致音质
        )
        set(v) = prefs.edit()
            .putInt("opus_complexity", sanitizeOpusComplexity(v))
            .apply()

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
        // 只有具体档位（64/128/256/384/512），自动档已移除。
        // 读出时吸附：历史配置里的 768/960/1000 归一到 512，
        // 旧的"自动(0)"归一到默认 256 —— 否则五档滑块会停在非法位置。
        get() = Udp2CalProtocol.snapBitrateKbps(prefs.getInt("opus_bitrate_kbps", DEFAULT_BITRATE_KBPS))
            .takeIf { it > 0 } ?: DEFAULT_BITRATE_KBPS
        set(v) = prefs.edit().putInt("opus_bitrate_kbps", v.coerceIn(0, Udp2CalProtocol.BITRATE_MAX_KBPS)).apply()

    var opusFec: Int
        // 默认开（2 = 与 UI 勾选状态一致）：10 ms 帧的带内 FEC 只是把上一帧
        // 的 LBRR 冗余放进当前包，不增加包数/延迟，却能靠单个后续包重建一帧。
        get() = prefs.getInt("opus_fec", 2)
        set(v) = prefs.edit().putInt("opus_fec", v.coerceIn(0, 2)).apply()

    var opusPacketLoss: Int
        // 配合 FEC 开启：告诉编码器预期丢包率，它才会持续生成 LBRR 冗余；
        // 为 0 时 libopus 认为无需冗余，等于把 FEC 白开。
        get() = prefs.getInt("opus_packet_loss", 10)
        set(v) = prefs.edit().putInt("opus_packet_loss", v.coerceIn(0, 100)).apply()

    var opusVbrConstraint: Int
        get() = prefs.getInt("opus_vbr_constraint", 0) // 0=无约束, 1=约束（不超码率）
        set(v) = prefs.edit().putInt("opus_vbr_constraint", v.coerceIn(0, 1)).apply()

    // ═══ P2P 独占通信配置 ═══
    var deviceId: String
        get() = prefs.getString("device_id", "") ?: ""
        set(v) = prefs.edit().putString("device_id", v).apply()
}
