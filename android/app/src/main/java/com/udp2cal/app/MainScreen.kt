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
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.scale
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.udp2cal.app.service.CaptureService
import kotlinx.coroutines.launch

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
    var errorMsg by remember { mutableStateOf("") }
    var isSearching by remember { mutableStateOf(false) }
    var showManualConfig by remember { mutableStateOf(false) }
    // 设备发现列表
    var showDeviceDialog by remember { mutableStateOf(false) }
    var discoveredDevices by remember { mutableStateOf<List<DiscoveryManager.DiscoverResult>>(emptyList()) }
    val scope = rememberCoroutineScope()
    val ctx = LocalContext.current

    LaunchedEffect(service) {
        service?.status?.collect { status ->
            isRunning = status.isRunning
            errorMsg = if (status.errorRes != 0) {
                ctx.getString(status.errorRes, *status.errorArgs.toTypedArray())
            } else {
                ""
            }
            connected = status.connected
        }
    }

    val discover: () -> Unit = {
        scope.launch {
            isSearching = true
            errorMsg = ctx.getString(R.string.discover_searching)
            val devices = DiscoveryManager.discoverServers()
            if (devices.isEmpty()) {
                errorMsg = ctx.getString(R.string.discover_not_found)
            } else {
                discoveredDevices = devices
                showDeviceDialog = true
                errorMsg = ctx.resources.getQuantityString(
                    R.plurals.discover_found, devices.size, devices.size
                )
            }
            isSearching = false
        }
        Unit
    }

    val statusColor = when {
        connected && isRunning -> Color(0xFF00E676)
        isRunning -> Color(0xFF00B0FF)
        else -> Color(0xFF888888)
    }
    val statusText = when {
        connected && isRunning -> stringResource(R.string.status_connected)
        isRunning -> stringResource(R.string.status_connecting)
        else -> stringResource(R.string.status_disconnected)
    }
    val statusPulse = rememberInfiniteTransition(label = "statusPulse")
    val statusDotAlpha by statusPulse.animateFloat(
        initialValue = 1f,
        targetValue = 0.35f,
        animationSpec = infiniteRepeatable(tween(700), RepeatMode.Reverse),
        label = "statusDotAlpha"
    )
    val shownPort = targetPort.ifBlank { Udp2CalProtocol.DEFAULT_LISTEN_PORT.toString() }
    val ringTransition = rememberInfiniteTransition(label = "startRing")
    val breath by ringTransition.animateFloat(
        initialValue = 0f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(
            tween(if (isRunning) 1800 else 2400, easing = FastOutSlowInEasing),
            RepeatMode.Reverse
        ),
        label = "startRingBreath"
    )
    val ringColor by animateColorAsState(
        targetValue = if (isRunning) Color(0xFFFF5252) else Color(0xFF00E676),
        animationSpec = tween(220),
        label = "startRingColor"
    )

    Column(
        modifier = Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(top = 44.dp, start = 20.dp, end = 20.dp, bottom = 16.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(10.dp)
    ) {
        Text("UDP2CAL", fontSize = 24.sp, color = Color(0xFF00E676))
        Text(stringResource(R.string.app_subtitle), fontSize = 11.sp, color = Color(0xFF777777))

        Surface(
            color = statusColor.copy(alpha = 0.12f),
            shape = RoundedCornerShape(50)
        ) {
            Row(
                modifier = Modifier.padding(horizontal = 12.dp, vertical = 6.dp),
                verticalAlignment = Alignment.CenterVertically
            ) {
                Box(
                    Modifier.size(7.dp)
                        .alpha(if (isRunning && !connected) statusDotAlpha else 1f)
                        .background(statusColor, CircleShape)
                )
                Spacer(Modifier.width(6.dp))
                Text(statusText, fontSize = 12.sp, color = statusColor)
            }
        }

        // ── 开始/停止按钮（顶部，双层呼吸光晕；运行时呼吸更快、变红） ──
        Box(Modifier.fillMaxWidth().height(80.dp), contentAlignment = Alignment.Center) {
            Box(
                Modifier.fillMaxWidth(0.90f).height(60.dp)
                    .scale(1f + breath * 0.07f)
                    .alpha(0.05f + breath * 0.08f)
                    .border(1.dp, ringColor, RoundedCornerShape(50))
            )
            Box(
                Modifier.fillMaxWidth(0.96f).height(66.dp)
                    .scale(1f + breath * 0.045f)
                    .alpha(0.12f + breath * 0.15f)
                    .border(2.dp, ringColor, RoundedCornerShape(50))
            )
            Button(
                onClick = {
                    if (isRunning) { onStop() }
                    else { val p = targetPort.toIntOrNull() ?: Udp2CalProtocol.DEFAULT_LISTEN_PORT; onStart(targetIp, p) }
                },
                modifier = Modifier.fillMaxWidth().height(58.dp)
                    .scale(1f + breath * 0.02f),
                shape = RoundedCornerShape(29.dp),
                colors = ButtonDefaults.buttonColors(
                    containerColor = if (isRunning) Color(0xFFD32F2F) else Color(0xFF00C853)
                )
            ) {
                Text(
                    stringResource(if (isRunning) R.string.action_stop else R.string.action_start),
                    fontSize = 19.sp,
                    fontWeight = FontWeight.Medium,
                    letterSpacing = 2.sp
                )
            }
        }

        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E)),
            shape = MaterialTheme.shapes.medium
        ) {
            Column(Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
                    Column(Modifier.weight(1f)) {
                        Text(stringResource(R.string.target_title), fontSize = 13.sp, color = Color(0xFF888888))
                        Text(
                            if (targetIp.isBlank()) stringResource(R.string.target_none)
                            else "$targetIp:$shownPort",
                            fontSize = 16.sp,
                            color = if (targetIp.isBlank()) Color(0xFFAAAAAA) else Color.White
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
                            ),
                            color = Color(0xFF00B0FF)
                        )
                    }
                }
                OutlinedButton(
                    onClick = { discover() },
                    enabled = !isSearching && !isRunning,
                    modifier = Modifier.fillMaxWidth()
                ) {
                    Icon(AutorenewIcon, contentDescription = null, modifier = Modifier.size(18.dp))
                    Spacer(Modifier.width(6.dp))
                    Text(
                        stringResource(
                            if (isSearching) R.string.action_searching else R.string.action_auto_discover
                        )
                    )
                }
                AnimatedVisibility(showManualConfig) {
                    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
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
                                if (!fs.isFocused) { Prefs.targetPort = targetPort.toIntOrNull() ?: Udp2CalProtocol.DEFAULT_LISTEN_PORT }
                            }
                        )
                    }
                }
            }
        }

        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(10.dp)
        ) {
            OutlinedButton(
                onClick = onBlackout,
                modifier = Modifier.weight(1f).height(44.dp),
                shape = RoundedCornerShape(22.dp),
                contentPadding = PaddingValues(horizontal = 8.dp)
            ) { Text(stringResource(R.string.action_blackout), color = Color(0xFF00B0FF), fontSize = 13.sp) }
            OutlinedButton(
                onClick = onOpenSettings,
                modifier = Modifier.weight(1f).height(44.dp),
                shape = RoundedCornerShape(22.dp),
                contentPadding = PaddingValues(horizontal = 8.dp)
            ) { Text(stringResource(R.string.action_advanced), color = Color(0xFFAAAAAA), fontSize = 13.sp) }
        }

        // 息屏断流引导：仅当未加入电池优化白名单时显示。
        // 每秒一次的 CONNECT 心跳就是系统认账的保活，白名单足够，无需加密心跳。
        if (!batteryWhitelisted) {
            Card(
                modifier = Modifier.fillMaxWidth(),
                colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E)),
                shape = MaterialTheme.shapes.medium
            ) {
                Row(
                    modifier = Modifier.fillMaxWidth().padding(12.dp),
                    verticalAlignment = Alignment.CenterVertically
                ) {
                    Column(Modifier.weight(1f)) {
                        Text(stringResource(R.string.battery_title), fontSize = 13.sp, color = Color(0xFFB0B0B0))
                        Text(
                            stringResource(R.string.battery_desc),
                            fontSize = 11.sp, color = Color(0xFF777777)
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
                                    ctx.startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
                                } catch (_: Exception) {
                                }
                            }
                        }
                    ) { Text(stringResource(R.string.action_settings), color = Color(0xFF00B0FF)) }
                }
            }
        }

        if (errorMsg.isNotEmpty()) {
            Spacer(Modifier.height(4.dp))
            Text(errorMsg, fontSize = 13.sp, color = Color(0xFFFF5252))
        }
    }

    // ── 设备选择弹窗 ──
    if (showDeviceDialog && discoveredDevices.isNotEmpty()) {
        AlertDialog(
            onDismissRequest = { showDeviceDialog = false },
            title = { Text(stringResource(R.string.device_title), color = Color.White) },
            text = {
                LazyColumn {
                    items(discoveredDevices) { device ->
                        Card(
                            modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp).clickable {
                                targetIp = device.ip
                                targetPort = device.port.toString()
                                Prefs.targetIp = device.ip
                                Prefs.targetPort = device.port
                                Prefs.targetDeviceId = Udp2CalProtocol.deviceIdToString(device.deviceId)
                                errorMsg = ctx.getString(
                                    R.string.device_selected,
                                    device.ip,
                                    device.port,
                                    device.deviceName,
                                )
                                showDeviceDialog = false
                                showManualConfig = false
                            },
                            colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E))
                        ) {
                            Row(Modifier.padding(12.dp), verticalAlignment = Alignment.CenterVertically) {
                                Column(Modifier.weight(1f)) {
                                    Text(device.deviceName, fontSize = 15.sp, color = Color(0xFF00E676))
                                    Text("${device.ip}:${device.port}", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                                }
                                Text(stringResource(R.string.action_select), fontSize = 12.sp, color = Color(0xFF00B0FF))
                            }
                        }
                    }
                }
            },
            confirmButton = {
                TextButton(onClick = { showDeviceDialog = false }) {
                    Text(stringResource(R.string.action_cancel), color = Color(0xFF888888))
                }
            },
            containerColor = Color(0xFF0D0D1A)
        )
    }
}
