package com.udp2mic.app

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.PathFillType
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.path
import androidx.compose.ui.unit.dp

@Suppress("CheckReturnValue")
val AutorenewIcon: ImageVector
    get() {
        if (_autorenew != null) {
            return _autorenew!!
        }
        _autorenew = ImageVector.Builder(
            name = "autorenew",
            defaultWidth = 40.dp,
            defaultHeight = 40.dp,
            viewportWidth = 40f,
            viewportHeight = 40f,
        )
            .apply {
                path(
                    fill = SolidColor(Color.Black),
                    fillAlpha = 1f,
                    stroke = null,
                    strokeAlpha = 1f,
                    strokeLineWidth = 1f,
                    strokeLineCap = StrokeCap.Butt,
                    strokeLineJoin = StrokeJoin.Bevel,
                    strokeLineMiter = 1f,
                    pathFillType = PathFillType.Companion.NonZero,
                ) {
                    moveTo(8.28f, 26.39f)
                    quadTo(7.42f, 24.86f, 7.04f, 23.29f)
                    reflectiveQuadTo(6.67f, 20.08f)
                    quadToRelative(0f, -5.5f, 3.92f, -9.43f)
                    reflectiveQuadTo(20f, 6.72f)
                    horizontalLineToRelative(1.29f)
                    lineTo(18.18f, 3.61f)
                    lineTo(20.04f, 1.75f)
                    lineTo(26.4f, 8.11f)
                    lineToRelative(-6.36f, 6.36f)
                    lineTo(18.15f, 12.58f)
                    lineTo(21.24f, 9.5f)
                    horizontalLineTo(20f)
                    quadToRelative(-4.36f, 0f, -7.46f, 3.1f)
                    reflectiveQuadToRelative(-3.1f, 7.48f)
                    quadToRelative(0f, 1.17f, 0.24f, 2.24f)
                    reflectiveQuadToRelative(0.62f, 2.04f)
                    lineTo(8.28f, 26.39f)
                    close()
                    moveTo(19.9f, 38.33f)
                    lineTo(13.54f, 31.97f)
                    lineTo(19.9f, 25.61f)
                    lineToRelative(1.86f, 1.86f)
                    lineToRelative(-3.11f, 3.11f)
                    horizontalLineTo(20f)
                    quadToRelative(4.36f, 0f, 7.46f, -3.1f)
                    reflectiveQuadTo(30.56f, 20f)
                    quadToRelative(0f, -1.17f, -0.22f, -2.24f)
                    reflectiveQuadTo(29.67f, 15.72f)
                    lineToRelative(2.03f, -2.03f)
                    quadToRelative(0.86f, 1.53f, 1.25f, 3.1f)
                    reflectiveQuadTo(33.33f, 20f)
                    quadToRelative(0f, 5.5f, -3.92f, 9.43f)
                    reflectiveQuadTo(20f, 33.36f)
                    horizontalLineTo(18.65f)
                    lineToRelative(3.11f, 3.11f)
                    lineTo(19.9f, 38.33f)
                    close()
                }
            }
            .build()
        return _autorenew!!
    }

private var _autorenew: ImageVector? = null
