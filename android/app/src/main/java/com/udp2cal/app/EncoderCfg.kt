package com.udp2cal.app

/**
 * 编码器参数快照：正向 Opus 参数的**唯一推导点**。
 *
 * 本地编码器（[com.udp2cal.app.native.OpusEncoder]）与发给 Windows 的 CONNECT
 * 负载都从这里取数，不再各自推导「DTX 强制 VBR、VBR 关闭则约束清零」。
 *
 * 旧实现用 `coerceIn(0, 1)` 接受越界值；现在统一为「非 1 即关」（例如
 * `opusDtx = 2` 视为关闭）。合法 0/1 配置下两者完全等价，脏配置下更保守。
 */
internal data class EncoderCfg(
    val bitrateKbps: Int,
    val complexity: Int,
    val signal: Int,
    val bandwidth: Int,
    val dtx: Int,
    val vbr: Int,
    val vbrConstraint: Int,
    val fec: Int,
    val packetLoss: Int,
) {
    companion object {
        fun fromPrefs(): EncoderCfg {
            val dtx = if (Prefs.opusDtx == 1) 1 else 0
            val vbr = if (dtx == 1 || Prefs.opusVbr == 1) 1 else 0
            return EncoderCfg(
                bitrateKbps = Udp2CalProtocol.clampBitrateKbps(Prefs.opusBitrateKbps),
                complexity = Prefs.opusComplexity,
                signal = Prefs.opusSignal,
                bandwidth = Prefs.opusBandwidth,
                dtx = dtx,
                vbr = vbr,
                vbrConstraint = if (vbr == 1 && Prefs.opusVbrConstraint == 1) 1 else 0,
                fec = Prefs.opusFec,
                packetLoss = Prefs.opusPacketLoss,
            )
        }
    }
}
