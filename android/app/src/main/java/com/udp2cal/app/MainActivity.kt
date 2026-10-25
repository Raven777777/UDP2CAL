package com.udp2cal.app

import android.app.Activity
import android.Manifest
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.clickable
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.ContextCompat
import com.udp2cal.app.service.CaptureService
import kotlinx.coroutines.launch
import kotlin.math.roundToInt

class MainActivity : ComponentActivity() {
    companion object { private const val TAG = "MainActivity" }

    private var captureService: CaptureService? = null
    private var serviceBound = false
    private var pendingStart: (() -> Unit)? = null
    private val serviceState = mutableStateOf<CaptureService?>(null)

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
            captureService = (service as CaptureService.LocalBinder).getService()
            serviceState.value = captureService
            serviceBound = true
            pendingStart?.invoke(); pendingStart = null
        }
        override fun onServiceDisconnected(name: ComponentName?) {
            captureService = null; serviceState.value = null; serviceBound = false
        }
    }

    /** 麦克风权限被拒（Compose 侧据此弹窗） */
    private val permissionDenied = mutableStateOf(false)

    private val micPermLauncher = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        // 拒绝后必须给出明确指引：否则用户点"开始采集"只会看到
        // "网络连接失败 / 麦克风初始化失败"，完全看不出根因是权限
        permissionDenied.value = !granted
        requestNotifIfNeeded()
    }
    private val notifPermLauncher = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
    }

    private fun openAppSettings() {
        try {
            startActivity(
                Intent(
                    Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                    Uri.fromParts("package", packageName, null)
                )
            )
        } catch (_: Exception) {
        }
    }

    override fun onResume() {
        super.onResume()
        // 用户从系统设置返回后重新判定
        if (permissionDenied.value &&
            ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED
        ) {
            permissionDenied.value = false
            requestNotifIfNeeded()
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        Prefs.init(applicationContext)
        bindService(Intent(this, CaptureService::class.java), connection, Context.BIND_AUTO_CREATE)

        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            micPermLauncher.launch(Manifest.permission.RECORD_AUDIO)
        } else {
            requestNotifIfNeeded()
        }

        setContent {
            val activity = LocalContext.current as Activity
            var showOpusSettings by remember { mutableStateOf(false) }

            BackHandler(enabled = true) {
                if (showOpusSettings) showOpusSettings = false else activity.moveTaskToBack(false)
            }

            MaterialTheme(colorScheme = darkColorScheme(primary = Color(0xFF00E676), secondary = Color(0xFF00B0FF))) {
                Surface(modifier = Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
                    if (showOpusSettings) {
                        OpusSettingsScreen(
                            onBack = { showOpusSettings = false },
                            service = serviceState.value
                        )
                    } else {
                        MainScreen(
                            onStart = { ip, port -> doStart(ip, port) },
                            onStop = { doStop() },
                            service = serviceState.value,
                            onOpenSettings = { showOpusSettings = true }
                        )
                    }
                }

                // 麦克风权限被拒：必须明确告知，否则用户只会看到"初始化失败"
                if (permissionDenied.value) {
                    AlertDialog(
                        onDismissRequest = { permissionDenied.value = false },
                        title = { Text("需要麦克风权限") },
                        text = {
                            Text(
                                "UDP2CAL 需要录音权限才能采集手机音频。" +
                                    "请在系统设置中允许「麦克风」后返回。"
                            )
                        },
                        confirmButton = {
                            TextButton(onClick = {
                                permissionDenied.value = false
                                openAppSettings()
                            }) { Text("去设置") }
                        },
                        dismissButton = {
                            TextButton(onClick = { permissionDenied.value = false }) { Text("稍后") }
                        }
                    )
                }
            }
        }
    }

    private fun doStart(ip: String, port: Int) {
        Prefs.targetIp = ip; Prefs.targetPort = port
        val sampleRateHz = 48000
        // 码率已是具体档位（Prefs 读出即吸附），不存在自动档
        val bitrateKbps = Prefs.opusBitrateKbps

        val action = {
            startService(Intent(this, CaptureService::class.java))
            captureService?.startCapture(sampleRateHz, bitrateKbps, ip, port, Prefs.testToneMode)
            Unit
        }
        if (captureService != null) action() else pendingStart = action
    }

    private fun doStop() {pendingStart = null; captureService?.stopCapture(); stopService(Intent(this, CaptureService::class.java))}

    private fun requestNotifIfNeeded() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED)
            notifPermLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
    }

    override fun onDestroy() {pendingStart = null; if (serviceBound) unbindService(connection) ; super.onDestroy() }
}

@Composable
fun MainScreen(
    onStart: (String, Int) -> Unit,
    onStop: () -> Unit,
    service: CaptureService?,
    onOpenSettings: () -> Unit
) {
    var targetIp by remember { mutableStateOf(Prefs.targetIp) }
    var targetPort by remember { mutableStateOf(Prefs.targetPort.toString()) }
    var isRunning by remember { mutableStateOf(false) }
    var errorMsg by remember { mutableStateOf("") }
    var isSearching by remember { mutableStateOf(false) }
    // 设备发现列表
    var showDeviceDialog by remember { mutableStateOf(false) }
    var discoveredDevices by remember { mutableStateOf<List<DiscoveryManager.DiscoverResult>>(emptyList()) }
    val scope = rememberCoroutineScope()

    LaunchedEffect(service) {
        service?.status?.collect { status ->
            isRunning = status.isRunning; errorMsg = status.errorMsg
        }
    }

    Column(
        modifier = Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(top = 44.dp, start = 20.dp, end = 20.dp, bottom = 16.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(10.dp)
    ) {
        Text("UDP2CAL", fontSize = 22.sp, color = Color(0xFF00E676))

        OutlinedTextField(value = targetIp, onValueChange = { targetIp = it },
            label = { Text("目标 IP") }, singleLine = true,
            modifier = Modifier.fillMaxWidth().onFocusChanged { fs ->
                if (!fs.isFocused) { Prefs.targetIp = targetIp }
            },
            trailingIcon = {
                Box(modifier = Modifier.clickable(enabled = !isSearching && !isRunning) {
                    scope.launch {
                        isSearching = true
                        errorMsg = "正在搜索局域网内的PC端..."
                        val devices = DiscoveryManager.discoverServers()
                        if (devices.isEmpty()) {
                            errorMsg = "未找到接收端，请确保 PC 端已打开且在同一 Wi-Fi 下"
                        } else {
                            discoveredDevices = devices
                            showDeviceDialog = true
                            errorMsg = "找到 ${devices.size} 个设备，请选择"
                        }
                        isSearching = false
                    }
                }) {
                    Icon(
                        imageVector = AutorenewIcon,
                        contentDescription = "自动搜索",
                        tint = if (isSearching) Color(0xFF888888) else Color(0xFF00B0FF),
                        modifier = Modifier.size(24.dp)
                    )
                }
            })

        OutlinedTextField(value = targetPort, onValueChange = { targetPort = it },
            label = { Text("端口") }, singleLine = true,
            modifier = Modifier.fillMaxWidth().onFocusChanged { fs ->
                if (!fs.isFocused) { Prefs.targetPort = targetPort.toIntOrNull() ?: 44044 }
            })

        Spacer(Modifier.height(4.dp))

        Button(onClick = {
            if (isRunning) { onStop() }
            else { val p = targetPort.toIntOrNull() ?: 44044; onStart(targetIp, p) }
        }, modifier = Modifier.fillMaxWidth().height(48.dp),
            colors = ButtonDefaults.buttonColors(containerColor = if (isRunning) Color(0xFFD32F2F) else Color(0xFF00C853))
        ) { Text(if (isRunning) "停止采集" else "开始采集", fontSize = 18.sp) }

        OutlinedButton(onClick = onOpenSettings, modifier = Modifier.fillMaxWidth()) { Text("高级设置", color = Color(0xFFAAAAAA)) }

        if (errorMsg.isNotEmpty()) {
            Spacer(Modifier.height(4.dp))
            Text(errorMsg, fontSize = 13.sp, color = Color(0xFFFF5252))
        }
    }

    // ── 设备选择弹窗 ──
    if (showDeviceDialog && discoveredDevices.isNotEmpty()) {
        AlertDialog(
            onDismissRequest = { showDeviceDialog = false },
            title = { Text("选择接收端", color = Color.White) },
            text = {
                LazyColumn {
                    items(discoveredDevices) { device ->
                        Card(
                            modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp).clickable {
                                targetIp = device.ip
                                targetPort = device.port.toString()
                                Prefs.targetIp = device.ip
                                Prefs.targetPort = device.port
                                errorMsg = "已选择: ${device.ip}:${device.port} (${device.deviceName})"
                                showDeviceDialog = false
                            },
                            colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E))
                        ) {
                            Row(Modifier.padding(12.dp), verticalAlignment = Alignment.CenterVertically) {
                                Column(Modifier.weight(1f)) {
                                    Text(device.deviceName, fontSize = 15.sp, color = Color(0xFF00E676))
                                    Text("${device.ip}:${device.port}", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                                }
                                Text("选择 →", fontSize = 12.sp, color = Color(0xFF00B0FF))
                            }
                        }
                    }
                }
            },
            confirmButton = {
                TextButton(onClick = { showDeviceDialog = false }) {
                    Text("取消", color = Color(0xFF888888))
                }
            },
            containerColor = Color(0xFF0D0D1A)
        )
    }
}

/** 带宽码（Opus 枚举值）→ 滑块与状态卡片共用的文案表 */
private val BW_CODES = intArrayOf(1101, 1102, 1103, 1104, 1105)
private val BW_LABELS = arrayOf(
    "窄带 NB · 8 kHz",
    "中带 MB · 12 kHz",
    "宽带 WB · 16 kHz",
    "超宽带 SWB · 24 kHz",
    "全频带 FB · 48 kHz",
)
private val BW_SHORT = arrayOf("NB", "MB", "WB", "SWB", "FB")

@Composable
fun OpusSettingsScreen(
    onBack: () -> Unit,
    service: CaptureService?
) {
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

    // ── 运行时状态 ──
    var isRunning by remember { mutableStateOf(false) }
    var audioSource by remember { mutableStateOf("") }
    var opusMode by remember { mutableStateOf("") }
    var realtimeKbps by remember { mutableStateOf(0f) }
    var sampleRateHz by remember { mutableStateOf(0) }
    var targetKbps by remember { mutableStateOf(0) }
    var vbrMode by remember { mutableStateOf("") }
    var connected by remember { mutableStateOf(false) }
    var opusBandwidth by remember { mutableStateOf(0) }
    var reverseAudio by remember { mutableStateOf(false) }
    var reverseBitrateKbps by remember { mutableStateOf(0f) }
    var reverseBw by remember { mutableStateOf("") }

    // 1kHz 测试音
    var testToneMode by remember { mutableStateOf(Prefs.testToneMode) }

    LaunchedEffect(service) {
        service?.status?.collect { status ->
            isRunning = status.isRunning
            audioSource = status.audioSource
            opusMode = status.opusMode
            realtimeKbps = status.bitrateKbps
            sampleRateHz = status.sampleRateHz
            targetKbps = status.bitrateTargetKbps
            vbrMode = status.vbrMode
            connected = status.connected
            opusBandwidth = status.opusBandwidth
            reverseAudio = status.reverseAudio
            reverseBitrateKbps = status.reverseBitrateKbps
            reverseBw = status.reverseBw
        }
    }

    // 带宽码（Opus 枚举值 1101..1105）→ 显示文本。
    // 滑块与状态卡片共用同一张表，避免两处文案各自漂移。
    fun bwDisplayName(bw: Int): String {
        val i = BW_CODES.indexOf(bw)
        return if (i >= 0) BW_LABELS[i] else ""
    }

    Column(
        modifier = Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(top = 44.dp, start = 20.dp, end = 20.dp, bottom = 24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        // ═══ 顶部导航 ═══
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = onBack) { Text("← 返回", fontSize = 16.sp, color = Color(0xFF00E676)) }
            Spacer(Modifier.width(10.dp))
            Text("高级设置", fontSize = 18.sp, color = Color.White)
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
                    Text("1kHz 测试音", fontSize = 14.sp, color = if (testToneMode) Color(0xFFFF9800) else Color(0xFFAAAAAA))
                    Text(if (testToneMode) "无需麦克风即可测试音频链路" else "关闭时使用麦克风采集",
                        fontSize = 11.sp, color = Color(0xFF666666))
                }
                Switch(checked = testToneMode, onCheckedChange = {
                    testToneMode = it; Prefs.testToneMode = it
                }, colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFFFF9800)))
            }
        }

        // ═══ 运行时状态卡片（仅采集运行时显示） ═══
        if (isRunning) {
            // ── 音源状态卡片 ──
            Card(
                modifier = Modifier.fillMaxWidth(),
                colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E)),
                shape = MaterialTheme.shapes.medium
            ) {
                Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    Text("音源状态", fontSize = 13.sp, color = Color(0xFF888888))
                    val (srcColor, srcIcon) = when {
                        audioSource.contains("回退") -> Color(0xFFFF9800) to "⚠"
                        audioSource.contains("MIC直出") -> Color(0xFF00B0FF) to "🎤"
                        audioSource.contains("硬件降噪") -> Color(0xFF00E676) to "🔇"
                        else -> Color(0xFFAAAAAA) to "●"
                    }
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text("$srcIcon ", fontSize = 16.sp)
                        Text(audioSource.ifEmpty { "等待启动..." }, fontSize = 15.sp, color = srcColor)
                    }
                    Text(
                        when {
                            audioSource.contains("MIC直出") -> "裸麦克风采集，系统硬件降噪已关闭"
                            audioSource.contains("回退") -> "VOICE_COMMUNICATION不可用，已自动回退MIC裸采集"
                            audioSource.contains("硬件降噪") -> "安卓系统原生硬件降噪(NS+AGC)已启用"
                            else -> ""
                        },
                        fontSize = 11.sp, color = Color(0xFF666666)
                    )
                }
            }

            // ── Opus 编码状态卡片（含 P2P 独占连接状态 + 实时带宽） ──
            Card(
                modifier = Modifier.fillMaxWidth(),
                colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E)),
                shape = MaterialTheme.shapes.medium
            ) {
                Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("Opus 编码状态", fontSize = 13.sp, color = Color(0xFF888888))
                        Text(if (connected) "● 独占连接" else "● 未连接",
                            fontSize = 11.sp,
                            color = if (connected) Color(0xFF00E676) else Color(0xFFFF5252))
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("模式", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(opusMode.ifEmpty { "—" }, fontSize = 13.sp,
                            color = if (opusMode.contains("语音")) Color(0xFFFF9800) else Color(0xFF00B0FF))
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("带宽", fontSize = 13.sp, color = Color(0xFFAAAAAA))
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
                        Text("输入采样率", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(if (sampleRateHz > 0) "${sampleRateHz / 1000}kHz" else "48kHz", fontSize = 13.sp, color = Color.White)
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("实时码率", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(if (realtimeKbps > 0f) "%.1fkbps".format(realtimeKbps) else "—", fontSize = 13.sp, color = Color(0xFF00E676))
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("目标码率", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        val shownKbps = if (isRunning && targetKbps > 0) targetKbps else bitrate
                        Text("${shownKbps}kbps", fontSize = 13.sp, color = Color.White)
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("VBR模式", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        // 与 EncoderCfg.vbrLabel() 同义：DTX 会强制 VBR
                        val shownVbr = when {
                            !(vbr || dtx) -> "CBR"
                            vbrConstraint -> "VBR·约束"
                            else -> "VBR"
                        }
                        Text(if (isRunning && vbrMode.isNotEmpty()) vbrMode else shownVbr, fontSize = 13.sp, color = Color(0xFFB0B0B0))
                    }
                }
            }

            // ── 反向串流状态卡片 ──
            Card(
                modifier = Modifier.fillMaxWidth(),
                colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E)),
                shape = MaterialTheme.shapes.medium
            ) {
                Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("反向串流 (PC→Phone)", fontSize = 13.sp, color = Color(0xFF888888))
                        Text(if (reverseAudio) "● 活跃" else "● 等待中",
                            fontSize = 11.sp,
                            color = if (reverseAudio) Color(0xFF00E676) else Color(0xFF666666))
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("码率", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(
                            if (reverseAudio && reverseBitrateKbps > 0f) "%.1fkbps".format(reverseBitrateKbps)
                            else if (reverseAudio) "解码中..."
                            else "—",
                            fontSize = 13.sp,
                            color = if (reverseBitrateKbps > 0f) Color(0xFF00E676) else Color(0xFF888888)
                        )
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("音频带宽", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(
                            when {
                                !reverseAudio -> "—"
                                reverseBw.isNotEmpty() -> reverseBw
                                else -> "等待数据..."
                            },
                            fontSize = 13.sp,
                            color = Color.White
                        )
                    }
                }
            }

            Spacer(Modifier.height(4.dp))
            HorizontalDivider(color = Color(0xFF333333))
            Spacer(Modifier.height(4.dp))
        }

        // ═══ Opus 编码参数设置 ═══
        Text("编码参数", fontSize = 15.sp, color = Color(0xFF00E676))

        Text("编码复杂度 (Complexity): ${complexity.toInt()}", fontSize = 14.sp, color = Color.White)
        Slider(value = complexity, onValueChange = { complexity = it; Prefs.opusComplexity = it.toInt() }, valueRange = 1f..10f, steps = 8, modifier = Modifier.fillMaxWidth())

        Text("信号类型 (Signal Type)", fontSize = 14.sp, color = Color.White)
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            val options = listOf(3001 to "语音", 3002 to "音乐")
            options.forEach { (valOf, label) ->
                FilterChip(selected = signalType == valOf, onClick = { signalType = valOf; Prefs.opusSignal = valOf }, label = { Text(label) })
            }
        }

        Text("音频带宽 (Bandwidth)", fontSize = 14.sp, color = Color.White)
        run {
            // 与码率滑块保持完全一致的版式：标题 → 当前值 → 滑块 → 刻度 → 说明
            val lastIdx = BW_CODES.lastIndex
            val idx = BW_CODES.indexOf(bandwidth).takeIf { it >= 0 } ?: lastIdx
            Text(
                "当前: ${BW_LABELS[idx]}",
                fontSize = 13.sp, color = Color(0xFFB0B0B0)
            )
            Slider(
                value = idx.toFloat(),
                onValueChange = { v ->
                    val i = v.roundToInt().coerceIn(0, lastIdx)
                    if (BW_CODES[i] != bandwidth) {
                        bandwidth = BW_CODES[i]
                        Prefs.opusBandwidth = BW_CODES[i]
                    }
                },
                valueRange = 0f..lastIdx.toFloat(),
                steps = BW_CODES.size - 2,
                modifier = Modifier.fillMaxWidth()
            )
            Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                BW_SHORT.forEachIndexed { i, code ->
                    Text(
                        code,
                        fontSize = 10.sp,
                        color = if (i == idx) Color(0xFF00E676) else Color(0xFF888888)
                    )
                }
            }
            Text(
                "限制编码频率范围，越低越省码率；语音推荐 WB/SWB，音乐用 FB",
                fontSize = 11.sp, color = Color(0xFF888888)
            )
        }

        // 码率：五档滑块。协议上限 512 kbps（10 ms 帧下 640 字节/帧，远低于 1275）
        Text("码率 (Bitrate)", fontSize = 14.sp, color = Color.White)
        run {
            val steps = Udp2CalProtocol.BITRATE_STEPS_KBPS
            val lastIdx = steps.lastIndex
            val idx = steps.indexOf(bitrate).takeIf { it >= 0 } ?: lastIdx
            Text("当前: ${steps[idx]} kbps", fontSize = 13.sp, color = Color(0xFFB0B0B0))
            Slider(
                value = idx.toFloat(),
                onValueChange = { v ->
                    val i = v.roundToInt().coerceIn(0, lastIdx)
                    if (steps[i] != bitrate) {
                        bitrate = steps[i]
                        Prefs.opusBitrateKbps = steps[i]
                    }
                },
                valueRange = 0f..lastIdx.toFloat(),
                steps = steps.size - 2,
                modifier = Modifier.fillMaxWidth()
            )
            Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                steps.forEach { kbps ->
                    Text(
                        "$kbps",
                        fontSize = 10.sp,
                        color = if (kbps == bitrate) Color(0xFF00E676) else Color(0xFF888888)
                    )
                }
            }
            Text(
                "${steps[idx]} kbps ≈ ${steps[idx] * 1000 / 8 * 10 / 1000} 字节/帧（10 ms），Opus 上限 1275 字节",
                fontSize = 11.sp, color = Color(0xFF888888)
            )
        }

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Text("DTX (静音时不传输)", fontSize = 14.sp, color = Color.White)
            Switch(checked = dtx, onCheckedChange = {
                dtx = it
                Prefs.opusDtx = if (it) 1 else 0
                if (it) { vbr = true; Prefs.opusVbr = 1 }
            }, colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFF00E676)))
        }

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Column {
                Text("VBR (动态码率)", fontSize = 14.sp,
                    color = if (dtx) Color(0xFF666666) else Color.White)
                if (dtx) {
                    Text("开启DTX时强制启用VBR", fontSize = 11.sp, color = Color(0xFFFF9800))
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
                    Text("VBR 码率约束", fontSize = 14.sp, color = Color.White)
                    Text(if (vbrConstraint) "严格不超过目标码率" else "可临时超过目标码率", fontSize = 11.sp, color = Color(0xFF888888))
                }
                Switch(
                    checked = vbrConstraint,
                    onCheckedChange = { vbrConstraint = it; Prefs.opusVbrConstraint = if (it) 1 else 0 },
                    colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFFFF9800))
                )
            }
        }

        Spacer(Modifier.height(8.dp))
        Text("前向纠错与丢包容错", fontSize = 15.sp, color = Color(0xFF00E676))

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Column {
                Text("FEC (前向纠错)", fontSize = 14.sp, color = Color.White)
                Text("包丢失时可用冗余数据恢复", fontSize = 11.sp, color = Color(0xFF888888))
            }
            Switch(
                checked = fec,
                onCheckedChange = { fec = it; Prefs.opusFec = if (it) 2 else 0 },
                colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFF00B0FF))
            )
        }

        Text("预期丢包率: ${packetLoss.toInt()}%", fontSize = 14.sp, color = Color.White)
        Text(
            if (packetLoss == 0f) "无丢包预期，关闭抗丢包优化"
            else if (packetLoss <= 5f) "低丢包：轻量冗余保护"
            else if (packetLoss <= 15f) "中丢包：增加冗余与鲁棒性"
            else "高丢包：最大冗余保护，码率效率降低",
            fontSize = 11.sp, color = Color(0xFF888888)
        )
        Slider(
            value = packetLoss,
            onValueChange = { packetLoss = it; Prefs.opusPacketLoss = it.toInt() },
            valueRange = 0f..30f, steps = 29, modifier = Modifier.fillMaxWidth()
        )

        // ═══ 关于页面 ═══
        Spacer(Modifier.height(12.dp))
        HorizontalDivider(color = Color(0xFF333333))
        Spacer(Modifier.height(12.dp))

        Text("关于", fontSize = 15.sp, color = Color(0xFF00E676))

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
                Text("v1.2.0", fontSize = 13.sp, color = Color(0xFF888888))
                Spacer(Modifier.height(4.dp))
                Text("作者：四折光曲 & 井水玉藻", fontSize = 14.sp, color = Color(0xFFCCCCCC))
                Text("本程序为自由软件，以 GPL v3 许可证发布", fontSize = 12.sp, color = Color(0xFF666666))
                Spacer(Modifier.height(8.dp))
                Text("双向局域网音频串流", fontSize = 14.sp, color = Color(0xFF00B0FF))
                Text("手机麦克风→PC + PC扬声器→手机", fontSize = 12.sp, color = Color(0xFF888888))
                Text("Opus编码 · 声学回声消除 · 低延迟", fontSize = 12.sp, color = Color(0xFF888888))
                Spacer(Modifier.height(8.dp))
                Text("https://github.com/Raven777777/UDP2CAL", fontSize = 12.sp, color = Color(0xFF00E676), textAlign = TextAlign.Center)
            }
        }
    }
}
