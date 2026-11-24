package com.udp2cal.app

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.udp2cal.app.service.CaptureMode
import com.udp2cal.app.service.CaptureService
import com.udp2cal.app.service.VbrMode
import com.udp2cal.app.service.labelRes
import com.udp2cal.app.service.vbrRes
import kotlin.math.roundToInt

@Composable
fun OpusSettingsScreen(
    onBack: () -> Unit,
    service: CaptureService?,
    onLanguageChanged: (String) -> Unit
) {
    val context = LocalContext.current
    val versionName = remember {
        runCatching {
            context.packageManager.getPackageInfo(context.packageName, 0).versionName
        }.getOrNull().orEmpty().ifEmpty { "?" }
    }
    // ── Opus 编码参数 ──
    var complexity by remember { mutableStateOf(Prefs.opusComplexity.toFloat()) }
    var signalType by remember { mutableStateOf(Prefs.opusSignal) }
    var bandwidth by remember { mutableStateOf(Prefs.opusBandwidth) }
    var dtx by remember { mutableStateOf(Prefs.opusDtx != 0) }
    var vbr by remember { mutableStateOf(Prefs.opusVbr != 0) }
    // 码率固定为具体档位：不再有"自动分配"（它等价于用上限，且会强制开 VBR）
    var bitrate by remember { mutableStateOf(Prefs.opusBitrateKbps) }
    var fec by remember { mutableStateOf(Prefs.opusFec != 0) }
    var packetLoss by remember { mutableStateOf(Prefs.opusPacketLoss.toFloat()) }
    var vbrConstraint by remember { mutableStateOf(Prefs.opusVbrConstraint != 0) }
    var waterTarget by remember { mutableStateOf(Prefs.waterTargetMs) }

    // ── 运行时状态 ──
    var isRunning by remember { mutableStateOf(false) }
    var mode by remember { mutableStateOf(CaptureMode.FULLBAND) }
    var realtimeKbps by remember { mutableStateOf(0f) }
    var sampleRateHz by remember { mutableStateOf(0) }
    var targetKbps by remember { mutableStateOf(0) }
    var vbrMode by remember { mutableStateOf(VbrMode.CBR) }
    var connected by remember { mutableStateOf(false) }
    var opusBandwidth by remember { mutableStateOf(0) }
    var reverseAudio by remember { mutableStateOf(false) }
    var reverseBitrateKbps by remember { mutableStateOf(0f) }
    var reverseJitter by remember { mutableStateOf("") }

    // 1kHz 测试音
    var testToneMode by remember { mutableStateOf(Prefs.testToneMode) }

    LaunchedEffect(service) {
        service?.status?.collect { status ->
            isRunning = status.isRunning
            mode = status.mode
            realtimeKbps = status.bitrateKbps
            sampleRateHz = status.sampleRateHz
            targetKbps = status.bitrateTargetKbps
            vbrMode = status.vbrMode
            connected = status.connected
            opusBandwidth = status.opusBandwidth
            reverseAudio = status.reverseAudio
            reverseBitrateKbps = status.reverseBitrateKbps
            reverseJitter = status.reverseJitter
        }
    }

    // 带宽码（Opus 枚举值 1101..1105）→ 显示文本由 bwDisplayName 统一处理。

    Column(
        modifier = Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(top = 44.dp, start = 20.dp, end = 20.dp, bottom = 24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        // ═══ 顶部导航 ═══
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = onBack) { Text(stringResource(R.string.action_back), fontSize = 16.sp, color = Color(0xFF00E676)) }
            Spacer(Modifier.width(10.dp))
            Text(stringResource(R.string.action_advanced), fontSize = 18.sp, color = Color.White)
        }

        // ═══ 1kHz 测试音开关（移入高级设置） ═══
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E)),
            shape = MaterialTheme.shapes.medium
        ) {
            Row(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp),
                Arrangement.SpaceBetween, Alignment.CenterVertically) {
                Column {
                    Text(stringResource(R.string.test_tone_title), fontSize = 14.sp, color = if (testToneMode) Color(0xFFFF9800) else Color(0xFFAAAAAA))
                    Text(
                        stringResource(if (testToneMode) R.string.test_tone_on else R.string.test_tone_off),
                        fontSize = 11.sp, color = Color(0xFF666666)
                    )
                }
                Switch(checked = testToneMode, onCheckedChange = {
                    testToneMode = it; Prefs.testToneMode = it
                }, colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFFFF9800)))
            }
        }

        // ═══ 运行时状态卡片（仅采集运行时显示） ═══
        if (isRunning) {
            // ── Opus 编码状态卡片（含 P2P 独占连接状态 + 实时带宽） ──
            Card(
                modifier = Modifier.fillMaxWidth(),
                colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E)),
                shape = MaterialTheme.shapes.medium
            ) {
                Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text(stringResource(R.string.opus_status_title), fontSize = 13.sp, color = Color(0xFF888888))
                        Text(
                            stringResource(
                                if (connected) R.string.opus_exclusive else R.string.opus_not_connected
                            ),
                            fontSize = 11.sp,
                            color = if (connected) Color(0xFF00E676) else Color(0xFFFF5252)
                        )
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text(stringResource(R.string.opus_mode), fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(
                            stringResource(mode.labelRes),
                            fontSize = 13.sp,
                            color = if (mode == CaptureMode.VOICE) Color(0xFFFF9800) else Color(0xFF00B0FF)
                        )
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text(stringResource(R.string.opus_bandwidth), fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        // 运行时显示**已生效**值（编码器上报），未运行时显示**已配置**值，
                        // 这样任何时候改滑块都能在这一行看到结果
                        Text(
                            bwDisplayName(if (isRunning) opusBandwidth else bandwidth).ifEmpty { "—" },
                            fontSize = 13.sp, color = Color.White
                        )
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        // 采集/编码输入固定 48 kHz；"带宽"指的是编码后保留的频率范围，
                        // 两者不是一回事，标签写清楚免得看起来自相矛盾
                        Text(stringResource(R.string.opus_input_sample_rate), fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(if (sampleRateHz > 0) "${sampleRateHz / 1000}kHz" else "48kHz", fontSize = 13.sp, color = Color.White)
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text(stringResource(R.string.opus_realtime_bitrate), fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(if (realtimeKbps > 0f) "%.1fkbps".format(realtimeKbps) else "—", fontSize = 13.sp, color = Color(0xFF00E676))
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text(stringResource(R.string.opus_target_bitrate), fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        val shownKbps = if (isRunning && targetKbps > 0) targetKbps else bitrate
                        Text("${shownKbps}kbps", fontSize = 13.sp, color = Color.White)
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text(stringResource(R.string.opus_vbr_mode), fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        // 与 EncoderCfg.vbrMode() 同义：DTX 会强制 VBR
                        val shownVbr = when {
                            !(vbr || dtx) -> VbrMode.CBR
                            vbrConstraint -> VbrMode.VBR_CONSTRAINED
                            else -> VbrMode.VBR
                        }
                        Text(stringResource((if (isRunning) vbrMode else shownVbr).vbrRes), fontSize = 13.sp, color = Color(0xFFB0B0B0))
                    }
                }
            }

            // ── 反向串流状态卡片（仅反向活跃时显示，零值项不显示） ──
            if (reverseAudio) {
                Card(
                    modifier = Modifier.fillMaxWidth(),
                    colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E)),
                    shape = MaterialTheme.shapes.medium
                ) {
                    Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                            Text(stringResource(R.string.reverse_title), fontSize = 13.sp, color = Color(0xFF888888))
                            Text(stringResource(R.string.reverse_active), fontSize = 11.sp, color = Color(0xFF00E676))
                        }
                        if (reverseBitrateKbps > 0f) {
                            Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                                Text(stringResource(R.string.reverse_bitrate), fontSize = 13.sp, color = Color(0xFFAAAAAA))
                                Text("%.1fkbps".format(reverseBitrateKbps), fontSize = 13.sp, color = Color(0xFF00E676))
                            }
                        }
                        if (reverseJitter.isNotEmpty()) {
                            Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                                Text(stringResource(R.string.reverse_jitter), fontSize = 13.sp, color = Color(0xFFAAAAAA))
                                Text(reverseJitter, fontSize = 12.sp, color = Color(0xFFB0B0B0))
                            }
                        }
                        if (reverseAudio) {
                            Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                                Text(stringResource(R.string.reverse_bandwidth), fontSize = 13.sp, color = Color(0xFFAAAAAA))
                                Text(stringResource(R.string.reverse_fb_stereo), fontSize = 13.sp, color = Color.White)
                            }
                        }
                    }
                }
            }

            Spacer(Modifier.height(4.dp))
            HorizontalDivider(color = Color(0xFF333333))
            Spacer(Modifier.height(4.dp))
        }

        // ═══ Opus 编码参数设置 ═══
        Text(stringResource(R.string.settings_encoding), fontSize = 15.sp, color = Color(0xFF00E676))

        Text(stringResource(R.string.settings_complexity, complexity.toInt()), fontSize = 14.sp, color = Color.White)
        Slider(value = complexity, onValueChange = { complexity = it; Prefs.opusComplexity = it.toInt() }, valueRange = 1f..10f, steps = 8, modifier = Modifier.fillMaxWidth())

        Text(stringResource(R.string.settings_mic_source), fontSize = 14.sp, color = Color.White)
        StepSlider(
            value = signalType,
            values = MIC_SIGNAL_CODES,
            tickLabels = MIC_SIGNAL_LABEL_RES.map { stringResource(it) },
            onValueChange = { v ->
                if (v != signalType) {
                    signalType = v
                    Prefs.opusSignal = v
                }
            },
            hint = stringResource(R.string.settings_mic_source_hint),
            fallbackIndex = 1,
        )

        Text(stringResource(R.string.settings_bandwidth), fontSize = 14.sp, color = Color.White)
        // 与码率滑块保持完全一致的版式：标题 → 当前值 → 滑块 → 刻度 → 说明
        StepSlider(
            value = bandwidth,
            values = BW_CODES,
            tickLabels = BW_SHORT_RES.map { stringResource(it) },
            onValueChange = { v ->
                if (v != bandwidth) {
                    bandwidth = v
                    Prefs.opusBandwidth = v
                }
            },
            hint = stringResource(R.string.settings_bandwidth_hint),
        )

        // 码率：五档滑块。协议上限 512 kbps（10 ms 帧下 640 字节/帧，远低于 1275）
        Text(stringResource(R.string.settings_bitrate), fontSize = 14.sp, color = Color.White)
        run {
            val steps = Udp2CalProtocol.BITRATE_STEPS_KBPS
            val idx = steps.indexOf(bitrate).takeIf { it >= 0 } ?: steps.lastIndex
            StepSlider(
                value = bitrate,
                values = steps,
                tickLabels = steps.map { it.toString() },
                onValueChange = { v ->
                    if (v != bitrate) {
                        bitrate = v
                        Prefs.opusBitrateKbps = v
                    }
                },
                hint = stringResource(
                    R.string.settings_bitrate_hint,
                    steps[idx],
                    steps[idx] * 1000 / 8 * 10 / 1000,
                ),
            )
        }

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Text(stringResource(R.string.settings_dtx), fontSize = 14.sp, color = Color.White)
            Switch(checked = dtx, onCheckedChange = {
                dtx = it
                Prefs.opusDtx = if (it) 1 else 0
                if (it) { vbr = true; Prefs.opusVbr = 1 }
            }, colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFF00E676)))
        }

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Column {
                Text(stringResource(R.string.settings_vbr), fontSize = 14.sp,
                    color = if (dtx) Color(0xFF666666) else Color.White)
                if (dtx) {
                    Text(stringResource(R.string.settings_vbr_forced), fontSize = 11.sp, color = Color(0xFFFF9800))
                }
            }
            Switch(
                checked = vbr || dtx,
                onCheckedChange = { vbr = it; Prefs.opusVbr = if (it) 1 else 0 },
                enabled = !dtx,
                colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFF00E676))
            )
        }

        if (vbr || dtx) {
            Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
                Column {
                    Text(stringResource(R.string.settings_vbr_constraint), fontSize = 14.sp, color = Color.White)
                    Text(
                        stringResource(
                            if (vbrConstraint) R.string.settings_vbr_strict else R.string.settings_vbr_loose
                        ),
                        fontSize = 11.sp, color = Color(0xFF888888)
                    )
                }
                Switch(
                    checked = vbrConstraint,
                    onCheckedChange = { vbrConstraint = it; Prefs.opusVbrConstraint = if (it) 1 else 0 },
                    colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFFFF9800))
                )
            }
        }

        Spacer(Modifier.height(8.dp))
        Text(stringResource(R.string.settings_fec_section), fontSize = 15.sp, color = Color(0xFF00E676))

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Column {
                Text(stringResource(R.string.settings_fec), fontSize = 14.sp, color = Color.White)
                Text(stringResource(R.string.settings_fec_hint), fontSize = 11.sp, color = Color(0xFF888888))
            }
            Switch(
                checked = fec,
                onCheckedChange = { fec = it; Prefs.opusFec = if (it) 2 else 0 },
                colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFF00B0FF))
            )
        }

        Text(stringResource(R.string.settings_packet_loss, packetLoss.toInt()), fontSize = 14.sp, color = Color.White)
        Text(
            stringResource(
                when {
                    packetLoss == 0f -> R.string.settings_loss_none
                    packetLoss <= 5f -> R.string.settings_loss_low
                    packetLoss <= 15f -> R.string.settings_loss_medium
                    else -> R.string.settings_loss_high
                }
            ),
            fontSize = 11.sp, color = Color(0xFF888888)
        )
        Slider(
            value = packetLoss,
            onValueChange = { packetLoss = it; Prefs.opusPacketLoss = it.toInt() },
            valueRange = 0f..30f, steps = 29, modifier = Modifier.fillMaxWidth()
        )

        Text(stringResource(R.string.settings_water_target, waterTarget), fontSize = 14.sp, color = Color.White)
        run {
            val lastIdx = WATER_TARGET_STEPS_MS.lastIndex
            val idx = WATER_TARGET_STEPS_MS.indexOf(waterTarget).takeIf { it >= 0 }
                ?: WATER_TARGET_STEPS_MS.indexOf(Udp2CalProtocol.WATER_TARGET_MS).coerceAtLeast(0)
            Slider(
                value = idx.toFloat(),
                onValueChange = { v ->
                    val i = v.roundToInt().coerceIn(0, lastIdx)
                    val ms = WATER_TARGET_STEPS_MS[i]
                    if (ms != waterTarget) {
                        waterTarget = ms
                        Prefs.waterTargetMs = ms
                        service?.setReverseWaterTargetMs(ms)
                    }
                },
                valueRange = 0f..lastIdx.toFloat(),
                steps = WATER_TARGET_STEPS_MS.size - 2,
                modifier = Modifier.fillMaxWidth()
            )
            Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                WATER_TARGET_STEPS_MS.forEach { ms ->
                    Text(
                        "$ms",
                        fontSize = 10.sp,
                        color = if (ms == waterTarget) Color(0xFF00E676) else Color(0xFF888888)
                    )
                }
            }
            Text(
                stringResource(R.string.settings_water_target_hint),
                fontSize = 11.sp, color = Color(0xFF888888)
            )
        }

        // ═══ 语言 ═══
        Spacer(Modifier.height(12.dp))
        HorizontalDivider(color = Color(0xFF333333))
        Spacer(Modifier.height(12.dp))

        Text(stringResource(R.string.language_title), fontSize = 15.sp, color = Color(0xFF00E676))
        val languageOptions = remember {
            listOf(AppLocale.SYSTEM to context.getString(R.string.language_system)) +
                AppLocale.available(context)
        }
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E)),
            shape = MaterialTheme.shapes.medium
        ) {
            Column(Modifier.padding(vertical = 4.dp)) {
                languageOptions.forEach { (tag, name) ->
                    Row(
                        Modifier.fillMaxWidth()
                            .clickable { onLanguageChanged(tag) }
                            .padding(horizontal = 12.dp, vertical = 8.dp),
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        RadioButton(
                            selected = Prefs.language == tag,
                            onClick = { onLanguageChanged(tag) },
                            colors = RadioButtonDefaults.colors(selectedColor = Color(0xFF00E676))
                        )
                        Spacer(Modifier.width(8.dp))
                        Text(name, fontSize = 14.sp, color = Color.White)
                    }
                }
            }
        }

        // ═══ 关于页面 ═══
        Spacer(Modifier.height(12.dp))
        HorizontalDivider(color = Color(0xFF333333))
        Spacer(Modifier.height(12.dp))

        Text(stringResource(R.string.about_title), fontSize = 15.sp, color = Color(0xFF00E676))

        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E)),
            shape = MaterialTheme.shapes.medium
        ) {
            Column(
                Modifier.padding(16.dp).fillMaxWidth(),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(6.dp)
            ) {
                Text("UDP2CAL", fontSize = 22.sp, color = Color(0xFF00E676))
                Text(stringResource(R.string.about_version, versionName), fontSize = 13.sp, color = Color(0xFF888888))
                Spacer(Modifier.height(4.dp))
                Text(stringResource(R.string.about_author), fontSize = 14.sp, color = Color(0xFFCCCCCC))
                Text(stringResource(R.string.about_license), fontSize = 12.sp, color = Color(0xFF666666))
                Spacer(Modifier.height(8.dp))
                Text(stringResource(R.string.about_tagline), fontSize = 14.sp, color = Color(0xFF00B0FF))
                Text(stringResource(R.string.about_desc), fontSize = 12.sp, color = Color(0xFF888888))
                Text(stringResource(R.string.about_features), fontSize = 12.sp, color = Color(0xFF888888))
                Spacer(Modifier.height(8.dp))
                Text("https://github.com/Raven777777/UDP2CAL", fontSize = 12.sp, color = Color(0xFF00E676), textAlign = TextAlign.Center)
            }
        }
    }
}

/**
 * 离散档位滑块：档位值数组 + 刻度文案 + 说明文案。
 * 滑块的刻度索引按 [value] 在 [values] 中的位置计算，找不到时回退到最后一档。
 */
@Composable
private fun StepSlider(
    value: Int,
    values: IntArray,
    tickLabels: List<String>,
    onValueChange: (Int) -> Unit,
    hint: String,
    fallbackIndex: Int = values.lastIndex,
) {
    val lastIdx = values.lastIndex
    val idx = values.indexOf(value).takeIf { it >= 0 } ?: fallbackIndex.coerceIn(0, lastIdx)
    Slider(
        value = idx.toFloat(),
        onValueChange = { v ->
            val i = v.roundToInt().coerceIn(0, lastIdx)
            onValueChange(values[i])
        },
        valueRange = 0f..lastIdx.toFloat(),
        steps = values.size - 2,
        modifier = Modifier.fillMaxWidth()
    )
    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
        tickLabels.forEachIndexed { i, label ->
            Text(
                label,
                fontSize = 10.sp,
                color = if (i == idx) Color(0xFF00E676) else Color(0xFF888888)
            )
        }
    }
    Text(hint, fontSize = 11.sp, color = Color(0xFF888888))
}
