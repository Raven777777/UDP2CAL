package com.udp2cal.app

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.udp2cal.app.service.CaptureMode
import com.udp2cal.app.service.CaptureService
import com.udp2cal.app.service.VbrMode
import com.udp2cal.app.service.labelRes
import com.udp2cal.app.service.vbrModeOf
import com.udp2cal.app.service.vbrRes

/**
 * 高级设置（Material 3）。
 *
 * 只保留"配置一次就不再动"的编码器参数、语言与关于信息。
 * 麦克风音源与预期延迟属于频繁调整项，已移到主页（见 MainScreen 的采集设置卡片）；
 * 两处写回的 Prefs 键完全一致，因此业务侧无需任何改动。
 */
@OptIn(ExperimentalMaterial3Api::class)
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
    // 开关初值表示**归一化后的用户意图**（非 1 即关，脏值如 2 视为关）。
    // 生效值由 `vbr || dtx`（DTX 强制 VBR）与 EncoderCfg 统一推导，因此 VBR /
    // VBR-约束必须读各自的 Prefs 意图，不能直接取 EncoderCfg 的推导值：否则
    // dtx=1、vbr=0 时开关会显示"VBR 已开"，用户关掉 DTX 后界面仍停在 VBR 开，
    // 而编码器实际按 CBR 运行。
    val initialCfg = remember { EncoderCfg.fromPrefs() }
    var complexity by remember { mutableStateOf(Prefs.opusComplexity.toFloat()) }
    var bandwidth by remember { mutableStateOf(Prefs.opusBandwidth) }
    // dtx 归一化与 EncoderCfg 一致（等价于 Prefs.opusDtx == 1）
    var dtx by remember { mutableStateOf(initialCfg.dtx == 1) }
    var vbr by remember { mutableStateOf(Prefs.opusVbr == 1) }
    // 码率固定为具体档位：不再有"自动分配"（它等价于用上限，且会强制开 VBR）
    var bitrate by remember { mutableStateOf(Prefs.opusBitrateKbps) }
    var fec by remember { mutableStateOf(Prefs.opusFec != 0) }
    var packetLoss by remember { mutableStateOf(Prefs.opusPacketLoss.toFloat()) }
    var vbrConstraint by remember { mutableStateOf(Prefs.opusVbrConstraint == 1) }

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

    Scaffold(
        modifier = Modifier.fillMaxSize(),
        containerColor = MaterialTheme.colorScheme.background,
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.action_advanced)) },
                navigationIcon = {
                    TextButton(onClick = onBack) {
                        Text(stringResource(R.string.action_back))
                    }
                },
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.background,
                    titleContentColor = MaterialTheme.colorScheme.onSurface,
                    navigationIconContentColor = MaterialTheme.colorScheme.primary
                )
            )
        }
    ) { innerPadding ->
        LazyColumn(
            modifier = Modifier.fillMaxSize().padding(innerPadding),
            contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 32.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp)
        ) {
            // ═══ 1kHz 测试音 ═══
            item {
                SettingsCard {
                    SettingSwitchRow(
                        title = stringResource(R.string.test_tone_title),
                        subtitle = stringResource(
                            if (testToneMode) R.string.test_tone_on else R.string.test_tone_off
                        ),
                        checked = testToneMode,
                        onCheckedChange = {
                            testToneMode = it
                            Prefs.testToneMode = it
                        },
                        titleColor = if (testToneMode) MaterialTheme.colorScheme.tertiary
                        else MaterialTheme.colorScheme.onSurface
                    )
                }
            }

            // ═══ 运行时状态（仅采集运行时显示） ═══
            if (isRunning) {
                item { SectionTitle(stringResource(R.string.opus_status_title)) }
                item {
                    SettingsCard(spacing = 10.dp) {
                        StatusRow(
                            label = stringResource(R.string.opus_status_title),
                            value = stringResource(
                                if (connected) R.string.opus_exclusive else R.string.opus_not_connected
                            ),
                            valueColor = if (connected) MaterialTheme.colorScheme.primary
                            else MaterialTheme.colorScheme.error
                        )
                        StatusRow(
                            label = stringResource(R.string.opus_mode),
                            value = stringResource(mode.labelRes),
                            valueColor = if (mode == CaptureMode.VOICE) MaterialTheme.colorScheme.tertiary
                            else MaterialTheme.colorScheme.secondary
                        )
                        // 运行时显示**已生效**值（编码器上报），未运行时显示**已配置**值
                        StatusRow(
                            label = stringResource(R.string.opus_bandwidth),
                            value = bwDisplayName(if (isRunning) opusBandwidth else bandwidth).ifEmpty { "—" }
                        )
                        // 采集/编码输入固定 48 kHz；"带宽"指的是编码后保留的频率范围，
                        // 两者不是一回事，标签写清楚免得看起来自相矛盾
                        StatusRow(
                            label = stringResource(R.string.opus_input_sample_rate),
                            value = if (sampleRateHz > 0) "${sampleRateHz / 1000}kHz" else "48kHz"
                        )
                        StatusRow(
                            label = stringResource(R.string.opus_realtime_bitrate),
                            value = if (realtimeKbps > 0f) "%.1fkbps".format(realtimeKbps) else "—",
                            valueColor = MaterialTheme.colorScheme.primary
                        )
                        val shownKbps = if (isRunning && targetKbps > 0) targetKbps else bitrate
                        StatusRow(
                            label = stringResource(R.string.opus_target_bitrate),
                            value = "${shownKbps}kbps"
                        )
                        // 与 EncoderCfg.vbrMode() 共用同一推导：DTX 会强制 VBR
                        val shownVbr = vbrModeOf(if (vbr || dtx) 1 else 0, if (vbrConstraint) 1 else 0)
                        StatusRow(
                            label = stringResource(R.string.opus_vbr_mode),
                            value = stringResource((if (isRunning) vbrMode else shownVbr).vbrRes)
                        )
                    }
                }

                // ── 反向串流（仅反向活跃时显示，零值项不显示） ──
                if (reverseAudio) {
                    item { SectionTitle(stringResource(R.string.reverse_title)) }
                    item {
                        SettingsCard(spacing = 10.dp) {
                            StatusRow(
                                label = stringResource(R.string.reverse_title),
                                value = stringResource(R.string.reverse_active),
                                valueColor = MaterialTheme.colorScheme.primary
                            )
                            if (reverseBitrateKbps > 0f) {
                                StatusRow(
                                    label = stringResource(R.string.reverse_bitrate),
                                    value = "%.1fkbps".format(reverseBitrateKbps),
                                    valueColor = MaterialTheme.colorScheme.primary
                                )
                            }
                            if (reverseJitter.isNotEmpty()) {
                                StatusRow(
                                    label = stringResource(R.string.reverse_jitter),
                                    value = reverseJitter
                                )
                            }
                            StatusRow(
                                label = stringResource(R.string.reverse_bandwidth),
                                value = stringResource(R.string.reverse_fb_stereo)
                            )
                        }
                    }
                }
            }

            // ═══ 编码参数 ═══
            item { SectionTitle(stringResource(R.string.settings_encoding)) }
            item {
                SettingsCard(spacing = 18.dp) {
                    ValueSlider(
                        label = stringResource(R.string.settings_complexity, complexity.toInt()),
                        value = complexity,
                        onValueChange = {
                            complexity = it
                            Prefs.opusComplexity = it.toInt()
                        },
                        valueRange = 1f..10f,
                        steps = 8
                    )

                    SettingDivider()

                    StepSlider(
                        label = stringResource(R.string.settings_bandwidth),
                        value = bandwidth,
                        values = BW_CODES,
                        tickLabels = BW_SHORT_RES.map { stringResource(it) },
                        onValueChange = { v ->
                            if (v != bandwidth) {
                                bandwidth = v
                                Prefs.opusBandwidth = v
                            }
                        },
                        supportingText = stringResource(R.string.settings_bandwidth_hint)
                    )

                    SettingDivider()

                    // 码率：五档滑块。协议上限 512 kbps（10 ms 帧下 640 字节/帧，远低于 1275）
                    run {
                        val steps = Udp2CalProtocol.BITRATE_STEPS_KBPS
                        val idx = steps.indexOf(bitrate).takeIf { it >= 0 } ?: steps.lastIndex
                        StepSlider(
                            label = stringResource(R.string.settings_bitrate),
                            value = bitrate,
                            values = steps,
                            tickLabels = steps.map { it.toString() },
                            onValueChange = { v ->
                                if (v != bitrate) {
                                    bitrate = v
                                    Prefs.opusBitrateKbps = v
                                }
                            },
                            supportingText = stringResource(
                                R.string.settings_bitrate_hint,
                                steps[idx],
                                steps[idx] * 1000 / 8 * 10 / 1000,
                            )
                        )
                    }

                    SettingDivider()

                    SettingSwitchRow(
                        title = stringResource(R.string.settings_dtx),
                        checked = dtx,
                        onCheckedChange = {
                            dtx = it
                            Prefs.opusDtx = if (it) 1 else 0
                            if (it) {
                                vbr = true
                                Prefs.opusVbr = 1
                            }
                        }
                    )

                    SettingSwitchRow(
                        title = stringResource(R.string.settings_vbr),
                        subtitle = if (dtx) stringResource(R.string.settings_vbr_forced) else null,
                        subtitleColor = MaterialTheme.colorScheme.tertiary,
                        checked = vbr || dtx,
                        onCheckedChange = { vbr = it; Prefs.opusVbr = if (it) 1 else 0 },
                        enabled = !dtx
                    )

                    if (vbr || dtx) {
                        SettingSwitchRow(
                            title = stringResource(R.string.settings_vbr_constraint),
                            subtitle = stringResource(
                                if (vbrConstraint) R.string.settings_vbr_strict
                                else R.string.settings_vbr_loose
                            ),
                            checked = vbrConstraint,
                            onCheckedChange = {
                                vbrConstraint = it
                                Prefs.opusVbrConstraint = if (it) 1 else 0
                            }
                        )
                    }
                }
            }

            // ═══ 前向纠错与丢包容错 ═══
            item { SectionTitle(stringResource(R.string.settings_fec_section)) }
            item {
                SettingsCard(spacing = 18.dp) {
                    SettingSwitchRow(
                        title = stringResource(R.string.settings_fec),
                        subtitle = stringResource(R.string.settings_fec_hint),
                        checked = fec,
                        onCheckedChange = {
                            fec = it
                            Prefs.opusFec = if (it) 2 else 0
                        }
                    )

                    SettingDivider()

                    ValueSlider(
                        label = stringResource(R.string.settings_packet_loss, packetLoss.toInt()),
                        value = packetLoss,
                        onValueChange = {
                            packetLoss = it
                            Prefs.opusPacketLoss = it.toInt()
                        },
                        valueRange = 0f..30f,
                        steps = 29,
                        supportingText = stringResource(
                            when {
                                packetLoss == 0f -> R.string.settings_loss_none
                                packetLoss <= 5f -> R.string.settings_loss_low
                                packetLoss <= 15f -> R.string.settings_loss_medium
                                else -> R.string.settings_loss_high
                            }
                        )
                    )
                }
            }

            // ═══ 语言 ═══
            item {
                val languageOptions = remember {
                    listOf(AppLocale.SYSTEM to context.getString(R.string.language_system)) +
                        AppLocale.available(context)
                }
                // 语言 tag 不是数值，滑块按选项下标定位，选中后再映射回 tag。
                val tags = remember(languageOptions) { languageOptions.map { it.first } }
                val current = tags.indexOf(Prefs.language).takeIf { it >= 0 } ?: 0
                SettingsCard {
                    StepSlider(
                        label = stringResource(R.string.language_title),
                        valueText = languageOptions[current].second,
                        value = current,
                        values = IntArray(tags.size) { it },
                        tickLabels = languageOptions.map { it.second },
                        onValueChange = { onLanguageChanged(tags[it]) },
                        fallbackIndex = 0
                    )
                }
            }

            // ═══ 关于 ═══
            item { SectionTitle(stringResource(R.string.about_title)) }
            item {
                Card(
                    modifier = Modifier.fillMaxWidth(),
                    shape = MaterialTheme.shapes.large,
                    colors = CardDefaults.cardColors(
                        containerColor = MaterialTheme.colorScheme.surfaceContainer
                    )
                ) {
                    Column(
                        modifier = Modifier.fillMaxWidth().padding(20.dp),
                        horizontalAlignment = Alignment.CenterHorizontally,
                        verticalArrangement = Arrangement.spacedBy(6.dp)
                    ) {
                        Text(
                            text = "UDP2CAL",
                            style = MaterialTheme.typography.headlineSmall,
                            color = MaterialTheme.colorScheme.primary
                        )
                        Text(
                            text = stringResource(R.string.about_version, versionName),
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                        Spacer(Modifier.height(6.dp))
                        Text(
                            text = stringResource(R.string.about_author),
                            style = MaterialTheme.typography.bodyMedium
                        )
                        Text(
                            text = stringResource(R.string.about_license),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                        Spacer(Modifier.height(10.dp))
                        Text(
                            text = stringResource(R.string.about_tagline),
                            style = MaterialTheme.typography.titleSmall,
                            color = MaterialTheme.colorScheme.secondary
                        )
                        Text(
                            text = stringResource(R.string.about_desc),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                        Text(
                            text = stringResource(R.string.about_features),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                        Spacer(Modifier.height(10.dp))
                        Text(
                            text = "https://github.com/Raven777777/UDP2CAL",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.primary,
                            textAlign = TextAlign.Center
                        )
                    }
                }
            }
        }
    }
}

/** 运行时状态的一行"名称 → 值"，值贴右对齐。 */
@Composable
private fun StatusRow(
    label: String,
    value: String,
    valueColor: Color = MaterialTheme.colorScheme.onSurface
) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically
    ) {
        Text(
            text = label,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant
        )
        Text(
            text = value,
            style = MaterialTheme.typography.bodyMedium,
            color = valueColor
        )
    }
}
