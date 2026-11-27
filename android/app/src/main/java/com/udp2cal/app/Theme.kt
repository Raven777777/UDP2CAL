package com.udp2cal.app

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Shapes
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * Material Design 3 主题。
 *
 * 色板以品牌绿（原 #00E676）为种子生成 M3 的**角色色**：主色取 T80（深色背景上的
 * 可读浅绿），容器色取 T30/T90 配对。这样界面里不再出现"某个控件用某个固定 HEX"
 * 的情况——所有颜色都从 colorScheme 语义角色取，亮/暗与可选的高对比变体都一致。
 *
 * 应用本身是常暗工具类界面（息屏省电、录音场景），因此固定使用 darkColorScheme，
 * 不跟随系统亮色，避免录制时白底刺眼。
 */
private val Udp2CalDarkColors = darkColorScheme(
    primary = Color(0xFF6EE7A8),             // T80 品牌绿：深色面上的可读主色
    onPrimary = Color(0xFF00391F),
    primaryContainer = Color(0xFF005232),    // T30
    onPrimaryContainer = Color(0xFF9DFFC6),  // T90

    secondary = Color(0xFF8CD6F8),           // T80 原 #00B0FF 的蓝
    onSecondary = Color(0xFF003549),
    secondaryContainer = Color(0xFF004D68),
    onSecondaryContainer = Color(0xFFC7E8FF),

    tertiary = Color(0xFFF6BD6A),            // 提示/警告（原 #FF9800）
    onTertiary = Color(0xFF412D00),
    tertiaryContainer = Color(0xFF5E4200),
    onTertiaryContainer = Color(0xFFFFDEA8),

    error = Color(0xFFFFB4AB),
    onError = Color(0xFF690005),
    errorContainer = Color(0xFF93000A),
    onErrorContainer = Color(0xFFFFDAD6),

    background = Color(0xFF0F1512),
    onBackground = Color(0xFFDFE4DE),

    surface = Color(0xFF0F1512),
    onSurface = Color(0xFFDFE4DE),
    surfaceVariant = Color(0xFF3F4944),
    onSurfaceVariant = Color(0xFFBEC9C2),
    surfaceTint = Color(0xFF6EE7A8),

    inverseSurface = Color(0xFFDFE4DE),
    inverseOnSurface = Color(0xFF2C322E),
    inversePrimary = Color(0xFF006C46),

    outline = Color(0xFF89938C),
    outlineVariant = Color(0xFF3F4944),
    scrim = Color(0xFF000000),

    // 升降层（M3 用 surface container 层级代替阴影表达高度）
    surfaceBright = Color(0xFF353B37),
    surfaceDim = Color(0xFF0F1512),
    surfaceContainerLowest = Color(0xFF0A0F0D),
    surfaceContainerLow = Color(0xFF171D1A),
    surfaceContainer = Color(0xFF1B211E),
    surfaceContainerHigh = Color(0xFF262C28),
    surfaceContainerHighest = Color(0xFF303633)
)

/** M3 标准形状标度（4/8/12/16/28dp） */
private val Udp2CalShapes = Shapes(
    extraSmall = androidx.compose.foundation.shape.RoundedCornerShape(4.dp),
    small = androidx.compose.foundation.shape.RoundedCornerShape(8.dp),
    medium = androidx.compose.foundation.shape.RoundedCornerShape(12.dp),
    large = androidx.compose.foundation.shape.RoundedCornerShape(16.dp),
    extraLarge = androidx.compose.foundation.shape.RoundedCornerShape(28.dp)
)

/**
 * 只微调标题字重：M3 默认的 Roboto 字标度在中文下略轻，标题统一提到 Medium/SemiBold
 * 更接近 Android 原生观感；其余字号/行高严格沿用 M3 标度。
 */
private val Udp2CalTypography = Typography().let { base ->
    base.copy(
        headlineSmall = base.headlineSmall.copy(fontWeight = FontWeight.SemiBold),
        titleLarge = base.titleLarge.copy(fontWeight = FontWeight.SemiBold, letterSpacing = 0.5.sp),
        titleMedium = base.titleMedium.mergeWeight(FontWeight.Medium),
        titleSmall = base.titleSmall.mergeWeight(FontWeight.Medium),
        labelLarge = base.labelLarge.copy(fontWeight = FontWeight.Medium)
    )
}

private fun TextStyle.mergeWeight(weight: FontWeight): TextStyle = copy(fontWeight = weight)

@Composable
fun Udp2CalTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = Udp2CalDarkColors,
        typography = Udp2CalTypography,
        shapes = Udp2CalShapes,
        content = content
    )
}
