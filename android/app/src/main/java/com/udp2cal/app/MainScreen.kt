package com.udp2cal.app

import android.content.Intent
import android.net.Uri
import android.provider.Settings
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.scale
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.udp2cal.app.service.CaptureService
import kotlinx.coroutines.launch

/**
 * 主页（Material 3）。
 *
 * 版式遵循 M3 的"分区 + 卡片"结构：TopAppBar 承载应用标识与高级设置入口，
 * 内容区自上而下为 连接状态 → 目标设备 → 采集参数 → 省电与辅助 → 提示条。
 *
 * 麦克风音源与预期延迟两项**常用**参数已从高级设置移到此处（原始写 Prefs 的行为不变），
 * 采集链路重建 / 反向水位下发仍由既有业务逻辑响应，这里只做 UI 读写。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MainScreen(
    onStart: (String, Int) -> Unit,
    onStop: () -> Unit,
    onBlackout: () -> Unit,
    service: CaptureService?,
    onOpenSettings: () -> Unit,
    batteryWhitelisted: Boolean
) {
    var targetIp by remember { mutableStateOf(Prefs.targetIp) }
    var targetPort by remember { mutableStateOf(Prefs.targetPort.toString()) }
    var isRunning by remember { mutableStateOf(false) }
    var connected by remember { mutableStateOf(false) }
    var message by remember { mutableStateOf("") }
    var messageIsError by remember { mutableStateOf(false) }
    var isSearching by remember { mutableStateOf(false) }
    var showManualConfig by remember { mutableStateOf(false) }
    // 设备发现列表
    var showDeviceDialog by remember { mutableStateOf(false) }
    var discoveredDevices by remember { mutableStateOf<List<DiscoveryManager.DiscoverResult>>(emptyList()) }
    val scope = rememberCoroutineScope()
    val ctx = LocalContext.current

    // ── 采集参数（自高级设置上移） ──
    var micSource by remember { mutableStateOf(Prefs.opusSignal) }
    var waterTargetMs by remember { mutableStateOf(Prefs.waterTargetMs) }

    LaunchedEffect(service) {
        service?.status?.collect { status ->
            isRunning = status.isRunning
            connected = status.connected
            if (status.errorRes != 0) {
                message = ctx.getString(status.errorRes, *status.errorArgs.toTypedArray())
                messageIsError = true
            } else if (messageIsError) {
                message = ""
                messageIsError = false
            }
        }
    }

    val discover: () -> Unit = {
        scope.launch {
            isSearching = true
            message = ctx.getString(R.string.discover_searching)
            messageIsError = false
            val devices = DiscoveryManager.discoverServers()
            if (devices.isEmpty()) {
                message = ctx.getString(R.string.discover_not_found)
                messageIsError = true
            } else {
                discoveredDevices = devices
                showDeviceDialog = true
                message = ctx.resources.getQuantityString(
                    R.plurals.discover_found, devices.size, devices.size
                )
            }
            isSearching = false
        }
        Unit
    }

    val statusColor = when {
        connected && isRunning -> MaterialTheme.colorScheme.primary
        isRunning -> MaterialTheme.colorScheme.secondary
        else -> MaterialTheme.colorScheme.onSurfaceVariant
    }
    val statusText = when {
        connected && isRunning -> stringResource(R.string.status_connected)
        isRunning -> stringResource(R.string.status_connecting)
        else -> stringResource(R.string.status_disconnected)
    }
    val shownPort = targetPort.ifBlank { Udp2CalProtocol.DEFAULT_LISTEN_PORT.toString() }
    val targetLabel = if (targetIp.isBlank()) stringResource(R.string.target_none) else "$targetIp:$shownPort"

    Scaffold(
        modifier = Modifier.fillMaxSize(),
        containerColor = MaterialTheme.colorScheme.background,
        topBar = {
            TopAppBar(
                title = {
                    Text(
                        text = stringResource(R.string.app_name),
                        style = MaterialTheme.typography.titleLarge,
                        fontWeight = FontWeight.SemiBold,
                        letterSpacing = 1.sp
                    )
                },
                actions = {
                    TextButton(onClick = onOpenSettings) {
                        Text(stringResource(R.string.action_advanced))
                    }
                },
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.background,
                    titleContentColor = MaterialTheme.colorScheme.primary,
                    actionIconContentColor = MaterialTheme.colorScheme.onSurfaceVariant
                )
            )
        }
    ) { innerPadding ->
        LazyColumn(
            modifier = Modifier.fillMaxSize().padding(innerPadding),
            contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 32.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp)
        ) {
            // ── 连接状态 + 主操作 ──
            item {
                ConnectionCard(
                    statusText = statusText,
                    statusColor = statusColor,
                    isRunning = isRunning,
                    targetLabel = targetLabel,
                    onToggle = {
                        if (isRunning) {
                            onStop()
                        } else {
                            val p = targetPort.toIntOrNull() ?: Udp2CalProtocol.DEFAULT_LISTEN_PORT
                            onStart(targetIp, p)
                        }
                    }
                )
            }

            // ── 目标设备 ──
            item {
                SectionTitle(stringResource(R.string.target_title))
            }
            item {
                SettingsCard {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        Column(Modifier.weight(1f)) {
                            Text(
                                text = stringResource(R.string.target_title),
                                style = MaterialTheme.typography.labelMedium,
                                color = MaterialTheme.colorScheme.onSurfaceVariant
                            )
                            Text(
                                text = targetLabel,
                                style = MaterialTheme.typography.titleMedium,
                                color = if (targetIp.isBlank()) MaterialTheme.colorScheme.onSurfaceVariant
                                else MaterialTheme.colorScheme.onSurface
                            )
                        }
                        TextButton(
                            onClick = { showManualConfig = !showManualConfig },
                            enabled = !isRunning
                        ) {
                            Text(
                                stringResource(
                                    if (showManualConfig) R.string.action_collapse
                                    else R.string.action_manual_input
                                )
                            )
                        }
                    }

                    FilledTonalButton(
                        onClick = discover,
                        enabled = !isSearching && !isRunning,
                        modifier = Modifier.fillMaxWidth().height(48.dp)
                    ) {
                        Icon(
                            imageVector = AutorenewIcon,
                            contentDescription = null,
                            modifier = Modifier.size(18.dp)
                        )
                        Spacer(Modifier.width(8.dp))
                        Text(
                            stringResource(
                                if (isSearching) R.string.action_searching else R.string.action_auto_discover
                            )
                        )
                    }

                    AnimatedVisibility(showManualConfig) {
                        Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                            OutlinedTextField(
                                value = targetIp,
                                onValueChange = {
                                    if (it != targetIp) Prefs.targetDeviceId = ""
                                    targetIp = it
                                },
                                label = { Text(stringResource(R.string.target_ip)) },
                                singleLine = true,
                                enabled = !isRunning,
                                modifier = Modifier.fillMaxWidth().onFocusChanged { fs ->
                                    if (!fs.isFocused) { Prefs.targetIp = targetIp }
                                }
                            )
                            OutlinedTextField(
                                value = targetPort,
                                onValueChange = {
                                    if (it != targetPort) Prefs.targetDeviceId = ""
                                    targetPort = it
                                },
                                label = { Text(stringResource(R.string.target_port)) },
                                singleLine = true,
                                enabled = !isRunning,
                                modifier = Modifier.fillMaxWidth().onFocusChanged { fs ->
                                    if (!fs.isFocused) {
                                        Prefs.targetPort = targetPort.toIntOrNull()
                                            ?: Udp2CalProtocol.DEFAULT_LISTEN_PORT
                                    }
                                }
                            )
                        }
                    }
                }
            }

            // ── 采集设置（麦克风音源 + 预期延迟，自高级设置上移） ──
            item {
                SectionTitle(stringResource(R.string.home_section_capture))
            }
            item {
                SettingsCard(spacing = 20.dp) {
                    StepSlider(
                        label = stringResource(R.string.settings_mic_source),
                        valueText = stringResource(MIC_SIGNAL_LABEL_RES[
                            MIC_SIGNAL_CODES.indexOf(micSource).takeIf { it >= 0 } ?: 1
                        ]),
                        value = micSource,
                        values = MIC_SIGNAL_CODES,
                        tickLabels = MIC_SIGNAL_LABEL_RES.map { stringResource(it) },
                        onValueChange = { v ->
                            if (v != micSource) {
                                micSource = v
                                Prefs.opusSignal = v
                            }
                        },
                        supportingText = stringResource(R.string.settings_mic_source_hint),
                        fallbackIndex = 1
                    )

                    SettingDivider()

                    StepSlider(
                        label = stringResource(R.string.home_latency_title),
                        valueText = "$waterTargetMs ms",
                        value = waterTargetMs,
                        values = Udp2CalProtocol.WATER_TARGET_STEPS_MS,
                        tickLabels = Udp2CalProtocol.WATER_TARGET_STEPS_MS.map { it.toString() },
                        onValueChange = { ms ->
                            if (ms != waterTargetMs) {
                                waterTargetMs = ms
                                Prefs.waterTargetMs = ms
                                service?.setReverseWaterTargetMs(ms)
                            }
                        },
                        supportingText = stringResource(R.string.settings_water_target_hint),
                        fallbackIndex = Udp2CalProtocol.WATER_TARGET_STEPS_MS
                            .indexOf(Udp2CalProtocol.WATER_TARGET_MS).coerceAtLeast(0)
                    )
                }
            }

            // ── 省电：息屏保持采集 ──
            item {
                FilledTonalButton(
                    onClick = onBlackout,
                    modifier = Modifier.fillMaxWidth().height(52.dp),
                    shape = MaterialTheme.shapes.medium
                ) {
                    Text(stringResource(R.string.action_blackout))
                }
            }

            // ── 息屏断流引导：仅当未加入电池优化白名单时显示 ──
            // 每秒一次的 CONNECT 心跳就是系统认账的保活，白名单足够，无需加密心跳。
            if (!batteryWhitelisted) {
                item {
                    Card(
                        modifier = Modifier.fillMaxWidth(),
                        shape = MaterialTheme.shapes.large,
                        colors = CardDefaults.cardColors(
                            containerColor = MaterialTheme.colorScheme.surfaceContainerHigh
                        )
                    ) {
                        Row(
                            modifier = Modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp, top = 14.dp, bottom = 14.dp),
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                                Text(
                                    text = stringResource(R.string.battery_title),
                                    style = MaterialTheme.typography.titleSmall,
                                    color = MaterialTheme.colorScheme.tertiary
                                )
                                Text(
                                    text = stringResource(R.string.battery_desc),
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant
                                )
                            }
                            TextButton(
                                onClick = {
                                    // 优先直跳"请求忽略电池优化"确认页；个别 ROM 拦截该意图时
                                    // 退回电池优化设置列表页
                                    try {
                                        ctx.startActivity(
                                            Intent(
                                                Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                                                Uri.parse("package:" + ctx.packageName)
                                            )
                                        )
                                    } catch (_: Exception) {
                                        try {
                                            ctx.startActivity(
                                                Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)
                                            )
                                        } catch (_: Exception) {
                                        }
                                    }
                                }
                            ) { Text(stringResource(R.string.action_settings)) }
                        }
                    }
                }
            }

            // ── 状态 / 错误提示条 ──
            if (message.isNotEmpty()) {
                item {
                    Surface(
                        modifier = Modifier.fillMaxWidth(),
                        shape = MaterialTheme.shapes.medium,
                        color = if (messageIsError) MaterialTheme.colorScheme.errorContainer
                        else MaterialTheme.colorScheme.secondaryContainer
                    ) {
                        Text(
                            text = message,
                            style = MaterialTheme.typography.bodyMedium,
                            color = if (messageIsError) MaterialTheme.colorScheme.onErrorContainer
                            else MaterialTheme.colorScheme.onSecondaryContainer,
                            modifier = Modifier.padding(horizontal = 16.dp, vertical = 12.dp)
                        )
                    }
                }
            }
        }
    }

    // ── 设备选择弹窗 ──
    if (showDeviceDialog && discoveredDevices.isNotEmpty()) {
        AlertDialog(
            onDismissRequest = { showDeviceDialog = false },
            title = { Text(stringResource(R.string.device_title)) },
            text = {
                LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    items(discoveredDevices) { device ->
                        Row(
                            modifier = Modifier.fillMaxWidth()
                                .clip(MaterialTheme.shapes.medium)
                                .background(MaterialTheme.colorScheme.surfaceContainerHigh)
                                .clickable {
                                    targetIp = device.ip
                                    targetPort = device.port.toString()
                                    Prefs.targetIp = device.ip
                                    Prefs.targetPort = device.port
                                    Prefs.targetDeviceId = Udp2CalProtocol.deviceIdToString(device.deviceId)
                                    message = ctx.getString(
                                        R.string.device_selected,
                                        device.ip,
                                        device.port,
                                        device.deviceName,
                                    )
                                    messageIsError = false
                                    showDeviceDialog = false
                                    showManualConfig = false
                                }
                                .padding(horizontal = 16.dp, vertical = 14.dp),
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                                Text(
                                    text = device.deviceName,
                                    style = MaterialTheme.typography.titleSmall,
                                    color = MaterialTheme.colorScheme.primary
                                )
                                Text(
                                    text = "${device.ip}:${device.port}",
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant
                                )
                            }
                            Text(
                                text = stringResource(R.string.action_select),
                                style = MaterialTheme.typography.labelLarge,
                                color = MaterialTheme.colorScheme.primary
                            )
                        }
                    }
                }
            },
            confirmButton = {
                TextButton(onClick = { showDeviceDialog = false }) {
                    Text(stringResource(R.string.action_cancel))
                }
            }
        )
    }
}

/**
 * 主页顶部的主操作卡片：状态胶囊 + 呼吸光环大按钮 + 目标地址。
 * 呼吸环是"正在采集"的持续可见信号；运行态整卡转为 error 色系以区分停止动作。
 */
@Composable
private fun ConnectionCard(
    statusText: String,
    statusColor: Color,
    isRunning: Boolean,
    targetLabel: String,
    onToggle: () -> Unit
) {
    val breathTransition = rememberInfiniteTransition(label = "heroBreath")
    val breath by breathTransition.animateFloat(
        initialValue = 0f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(
            tween(if (isRunning) 1700 else 2400, easing = FastOutSlowInEasing),
            RepeatMode.Reverse
        ),
        label = "heroBreathValue"
    )
    val ringColor by animateColorAsState(
        targetValue = if (isRunning) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.primary,
        animationSpec = tween(220),
        label = "heroRingColor"
    )
    // 连接中（已启动、尚未握手）时状态点做缓慢闪烁，已连接则常亮
    val dotAlpha = if (statusText == stringResource(R.string.status_connecting)) {
        1f - breath * 0.6f
    } else {
        1f
    }

    Card(
        modifier = Modifier.fillMaxWidth(),
        shape = MaterialTheme.shapes.extraLarge,
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.surfaceContainerHigh
        )
    ) {
        Column(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 24.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(20.dp)
        ) {
            Surface(shape = CircleShape, color = statusColor.copy(alpha = 0.14f)) {
                Row(
                    modifier = Modifier.padding(horizontal = 14.dp, vertical = 7.dp),
                    verticalAlignment = Alignment.CenterVertically
                ) {
                    Box(
                        Modifier.size(8.dp)
                            .alpha(dotAlpha)
                            .background(statusColor, CircleShape)
                    )
                    Spacer(Modifier.width(8.dp))
                    Text(
                        text = statusText,
                        style = MaterialTheme.typography.labelLarge,
                        color = statusColor
                    )
                }
            }

            Box(
                modifier = Modifier.height(152.dp).fillMaxWidth(),
                contentAlignment = Alignment.Center
            ) {
                // 呼吸光环（非交互，仅状态提示）
                Box(
                    Modifier.size(132.dp)
                        .scale(1f + breath * 0.10f)
                        .alpha(0.10f + breath * 0.22f)
                        .border(2.dp, ringColor, CircleShape)
                )
                Button(
                    onClick = onToggle,
                    modifier = Modifier.size(112.dp).scale(1f + breath * 0.02f),
                    shape = CircleShape,
                    contentPadding = PaddingValues(0.dp),
                    colors = ButtonDefaults.buttonColors(
                        containerColor = if (isRunning) MaterialTheme.colorScheme.error
                        else MaterialTheme.colorScheme.primary,
                        contentColor = if (isRunning) MaterialTheme.colorScheme.onError
                        else MaterialTheme.colorScheme.onPrimary
                    )
                ) {
                    Text(
                        text = stringResource(if (isRunning) R.string.action_stop else R.string.action_start),
                        style = MaterialTheme.typography.titleMedium,
                        fontWeight = FontWeight.SemiBold,
                        textAlign = TextAlign.Center
                    )
                }
            }

            Text(
                text = targetLabel,
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant
            )
        }
    }
}
