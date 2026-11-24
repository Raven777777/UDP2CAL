package com.udp2cal.app

import androidx.compose.runtime.Composable
import androidx.compose.ui.res.stringResource

/** 带宽码（Opus 枚举值）→ 滑块与状态卡片共用的文案资源 */
internal val BW_CODES = intArrayOf(1101, 1102, 1103, 1104, 1105)
private val BW_LABEL_RES = intArrayOf(
    R.string.bw_nb,
    R.string.bw_mb,
    R.string.bw_wb,
    R.string.bw_swb,
    R.string.bw_fb,
)
internal val BW_SHORT_RES = intArrayOf(
    R.string.bw_short_nb,
    R.string.bw_short_mb,
    R.string.bw_short_wb,
    R.string.bw_short_swb,
    R.string.bw_short_fb,
)

/** 麦克风音源档位 → 短标签资源 */
internal val MIC_SIGNAL_CODES = intArrayOf(3001, 3002, 3003, 3004)
internal val MIC_SIGNAL_LABEL_RES = intArrayOf(
    R.string.mic_voice,
    R.string.mic_direct,
    R.string.mic_raw,
    R.string.mic_recognition,
)

@Composable
internal fun bwDisplayName(bw: Int): String {
    val index = BW_CODES.indexOf(bw)
    return if (index >= 0) stringResource(BW_LABEL_RES[index]) else ""
}

/** 反向接收目标水位五档（ms） */
internal val WATER_TARGET_STEPS_MS = intArrayOf(80, 120, 160, 200, 300)
