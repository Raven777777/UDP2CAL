package com.udp2cal.app

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import kotlin.math.roundToInt

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

// ═══════════════════════════════════════════════════════════════════════════
// 以下为 Material 3 界面的复用构件：主页与高级设置页共用，避免两页样式漂移。
// ═══════════════════════════════════════════════════════════════════════════

/** 分区标题：M3 titleSmall + primary，用于把若干卡片编成一组。 */
@Composable
internal fun SectionTitle(text: String, modifier: Modifier = Modifier) {
    Text(
        text = text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = modifier.padding(start = 4.dp, top = 8.dp, bottom = 2.dp)
    )
}

/**
 * 统一的分组卡片：surfaceContainer + large 圆角。
 * M3 用"容器层级"而不是阴影表达高度，因此这里不用 elevation。
 */
@Composable
internal fun SettingsCard(
    modifier: Modifier = Modifier,
    spacing: Dp = 16.dp,
    containerColor: Color = MaterialTheme.colorScheme.surfaceContainer,
    content: @Composable ColumnScope.() -> Unit
) {
    Card(
        modifier = modifier.fillMaxWidth(),
        shape = MaterialTheme.shapes.large,
        colors = CardDefaults.cardColors(containerColor = containerColor)
    ) {
        Column(
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 16.dp),
            verticalArrangement = Arrangement.spacedBy(spacing),
            content = content
        )
    }
}

/** 卡片内的细分隔线（M3 divider：outlineVariant）。 */
@Composable
internal fun SettingDivider(modifier: Modifier = Modifier) {
    HorizontalDivider(modifier = modifier, color = MaterialTheme.colorScheme.outlineVariant)
}

/**
 * 开关行：标题 + 可选说明 + 尾部 Switch。
 * M3 规范里列表型设置项左文右开关，点击热区覆盖整行。
 */
@Composable
internal fun SettingSwitchRow(
    title: String,
    checked: Boolean,
    onCheckedChange: (Boolean) -> Unit,
    modifier: Modifier = Modifier,
    subtitle: String? = null,
    enabled: Boolean = true,
    titleColor: Color = MaterialTheme.colorScheme.onSurface,
    subtitleColor: Color = MaterialTheme.colorScheme.onSurfaceVariant
) {
    Row(
        modifier = modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically
    ) {
        Column(
            modifier = Modifier.weight(1f).padding(end = 12.dp),
            verticalArrangement = Arrangement.spacedBy(2.dp)
        ) {
            Text(
                text = title,
                style = MaterialTheme.typography.bodyLarge,
                color = if (enabled) titleColor else MaterialTheme.colorScheme.onSurface.copy(alpha = 0.38f)
            )
            subtitle?.let {
                Text(
                    text = it,
                    style = MaterialTheme.typography.bodySmall,
                    color = subtitleColor
                )
            }
        }
        Switch(checked = checked, onCheckedChange = onCheckedChange, enabled = enabled)
    }
}

/**
 * 连续档位滑块：标题行（左侧名称、右侧当前值）+ M3 Slider + 可选说明。
 * 所有取值都由调用方写回 Prefs，这里只负责交互与排版。
 */
@Composable
internal fun ValueSlider(
    label: String,
    value: Float,
    onValueChange: (Float) -> Unit,
    valueRange: ClosedFloatingPointRange<Float>,
    modifier: Modifier = Modifier,
    valueText: String? = null,
    steps: Int = 0,
    supportingText: String? = null
) {
    Column(modifier = modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(2.dp)) {
        SliderLabelRow(label, valueText)
        Slider(
            value = value,
            onValueChange = onValueChange,
            valueRange = valueRange,
            steps = steps,
            modifier = Modifier.fillMaxWidth()
        )
        supportingText?.let { SupportingText(it) }
    }
}

/**
 * 离散档位滑块：档位值数组 + 刻度文案 + 说明文案。
 * 滑块位置按 [value] 在 [values] 中的下标计算，找不到时回退到 [fallbackIndex]。
 */
@Composable
internal fun StepSlider(
    label: String,
    value: Int,
    values: IntArray,
    tickLabels: List<String>,
    onValueChange: (Int) -> Unit,
    modifier: Modifier = Modifier,
    valueText: String? = null,
    supportingText: String? = null,
    fallbackIndex: Int = values.lastIndex
) {
    val lastIdx = values.lastIndex
    val idx = values.indexOf(value).takeIf { it >= 0 } ?: fallbackIndex.coerceIn(0, lastIdx)

    Column(modifier = modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(2.dp)) {
        SliderLabelRow(label, valueText)
        Slider(
            value = idx.toFloat(),
            onValueChange = { v ->
                val i = v.roundToInt().coerceIn(0, lastIdx)
                if (i != idx) onValueChange(values[i])
            },
            valueRange = 0f..lastIdx.toFloat(),
            steps = (values.size - 2).coerceAtLeast(0),
            modifier = Modifier.fillMaxWidth()
        )
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween
        ) {
            tickLabels.forEachIndexed { i, tick ->
                Text(
                    text = tick,
                    style = MaterialTheme.typography.labelSmall,
                    color = if (i == idx) MaterialTheme.colorScheme.primary
                    else MaterialTheme.colorScheme.onSurfaceVariant
                )
            }
        }
        supportingText?.let { SupportingText(it) }
    }
}

@Composable
private fun SliderLabelRow(label: String, valueText: String?) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically
    ) {
        Text(text = label, style = MaterialTheme.typography.bodyLarge)
        valueText?.let {
            Text(
                text = it,
                style = MaterialTheme.typography.labelLarge,
                color = MaterialTheme.colorScheme.primary
            )
        }
    }
}

@Composable
private fun SupportingText(text: String) {
    Text(
        text = text,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant
    )
}
