package com.udp2mic.app

import android.app.Activity
import android.Manifest
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
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
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.ContextCompat
import com.udp2mic.app.service.CaptureService
import kotlinx.coroutines.launch

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

    private val micPermLauncher = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        // 麦克风权限处理完后，检查通知权限
        requestNotifIfNeeded()
    }
    private val notifPermLauncher = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        // 权限全部处理完毕
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Prefs.init(applicationContext)
        bindService(Intent(this, CaptureService::class.java), connection, Context.BIND_AUTO_CREATE)

        // 先申请麦克风权限
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
                        OpusSettingsScreen(onBack = { showOpusSettings = false })
                    } else {
                        MainScreen(
                            onStart = { ip, port, testTone -> doStart(ip, port, testTone) },
                            onStop = { doStop() },
                            service = serviceState.value,
                            onOpenSettings = { showOpusSettings = true }
                        )
                    }
                }
            }
        }
    }

    private fun doStart(ip: String, port: Int, testTone: Boolean) {
        Prefs.targetIp = ip; Prefs.targetPort = port; Prefs.testToneMode = testTone
        val sampleRateHz = 48000
        val bitrateKbps = if (Prefs.opusBitrateKbps > 0) Prefs.opusBitrateKbps else 512

        val action = {
            startService(Intent(this, CaptureService::class.java))
            captureService?.startCapture(sampleRateHz, bitrateKbps, ip, port, testTone)
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
    onStart: (String, Int, Boolean) -> Unit,
    onStop: () -> Unit,
    service: CaptureService?,
    onOpenSettings: () -> Unit
) {
    var targetIp by remember { mutableStateOf(Prefs.targetIp) }
    var targetPort by remember { mutableStateOf(Prefs.targetPort.toString()) }
    var isRunning by remember { mutableStateOf(false) }
    var testToneMode by remember { mutableStateOf(Prefs.testToneMode) }
    var errorMsg by remember { mutableStateOf("") }
    var isSearching by remember { mutableStateOf(false) }
    // 运行时状态
    var audioSource by remember { mutableStateOf("") }
    var opusMode by remember { mutableStateOf("") }
    var realtimeKbps by remember { mutableStateOf(0f) }
    var sampleRateHz by remember { mutableStateOf(0) }
    var targetKbps by remember { mutableStateOf(0) }
    var vbrMode by remember { mutableStateOf("") }
    val scope = rememberCoroutineScope()

    LaunchedEffect(service) {
        service?.status?.collect { status ->
            isRunning = status.isRunning; errorMsg = status.errorMsg
            audioSource = status.audioSource
            opusMode = status.opusMode
            realtimeKbps = status.bitrateKbps
            sampleRateHz = status.sampleRateHz
            targetKbps = status.bitrateTargetKbps
            vbrMode = status.vbrMode
        }
    }

    Column(
        modifier = Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(top = 44.dp, start = 20.dp, end = 20.dp, bottom = 16.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(10.dp)
    ) {
        Text("UDP2Mic", fontSize = 22.sp, color = Color(0xFF00E676))

        OutlinedTextField(value = targetIp, onValueChange = { targetIp = it },
            label = { Text("目标 IP") }, singleLine = true,
            modifier = Modifier.fillMaxWidth().onFocusChanged { fs ->
                if (!fs.isFocused) { Prefs.targetIp = targetIp }
            },
            trailingIcon = {
                Box(modifier = Modifier.clickable(enabled = !isSearching) {
                    scope.launch {
                        isSearching = true
                        errorMsg = "正在搜索局域网内的PC端..."
                        val result = DiscoveryManager.discoverServer()
                        if (result != null) {
                            targetIp = result.first
                            targetPort = result.second.toString()
                            Prefs.targetIp = result.first
                            Prefs.targetPort = result.second
                            errorMsg = "已自动连接到 PC: ${result.first}:${result.second}"
                        } else {
                            errorMsg = "未找到接收端，请确保 PC 端已打开且在同一 Wi-Fi 下"
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
            else { val p = targetPort.toIntOrNull() ?: 44044; onStart(targetIp, p, testToneMode) }
        }, modifier = Modifier.fillMaxWidth().height(48.dp),
            colors = ButtonDefaults.buttonColors(containerColor = if (isRunning) Color(0xFFD32F2F) else Color(0xFF00C853))
        ) { Text(if (isRunning) "停止采集" else "开始采集", fontSize = 18.sp) }

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Text("1kHz测试音", fontSize = 14.sp, color = if (testToneMode) Color(0xFFFF9800) else Color(0xFFAAAAAA))
            Switch(checked = testToneMode, onCheckedChange = {
                testToneMode = it; Prefs.testToneMode = it
                if (isRunning) { val p = targetPort.toIntOrNull() ?: 44044; onStart(targetIp, p, it) }
            }, colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFFFF9800)))
        }

        OutlinedButton(onClick = onOpenSettings, modifier = Modifier.fillMaxWidth()) { Text("Opus 编码设置", color = Color(0xFFAAAAAA)) }

        // ── 未运行时显示简要 Opus 配置摘要 ──
        if (!isRunning) {
            val bwNames = listOf("窄带", "中带", "宽带", "超宽带", "全频带")
            val bwIdx = when (Prefs.opusBandwidth) { 1101 -> 0; 1102 -> 1; 1103 -> 2; 1104 -> 3; else -> 4 }
            val sigNames = listOf("语音", "音乐")
            val sigIdx = if (Prefs.opusSignal == 3001) 0 else 1
            Text("复杂度=${Prefs.opusComplexity} ${sigNames[sigIdx]} ${bwNames[bwIdx]} ${if (Prefs.opusVbr != 0) "VBR" else "CBR"}/DTX=${if (Prefs.opusDtx!=0)"开"else"关"} FEC=${if(Prefs.opusFec!=0)"开"else"关"}/丢包=${Prefs.opusPacketLoss}% 码率=${if (Prefs.opusBitrateKbps > 0) "${Prefs.opusBitrateKbps}k" else "自动"}", fontSize = 11.sp, color = Color(0xFF666666))
        }

        // ═══ 运行时状态卡片 ═══
        if (isRunning) {
            Spacer(Modifier.height(4.dp))

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

            // ── Opus 编码状态卡片 ──
            Card(
                modifier = Modifier.fillMaxWidth(),
                colors = CardDefaults.cardColors(containerColor = Color(0xFF1A1A2E)),
                shape = MaterialTheme.shapes.medium
            ) {
                Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    Text("Opus 编码状态", fontSize = 13.sp, color = Color(0xFF888888))
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("模式", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(opusMode.ifEmpty { "—" }, fontSize = 13.sp,
                            color = if (opusMode.contains("语音")) Color(0xFFFF9800) else Color(0xFF00B0FF))
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("采样率", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(if (sampleRateHz > 0) "${sampleRateHz / 1000}kHz" else "—", fontSize = 13.sp, color = Color.White)
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("实时码率", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(if (realtimeKbps > 0f) "%.1fkbps".format(realtimeKbps) else "—", fontSize = 13.sp, color = Color(0xFF00E676))
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("目标码率", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(if (targetKbps > 0) "${targetKbps}kbps" else "自动", fontSize = 13.sp, color = Color.White)
                    }
                    Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                        Text("VBR模式", fontSize = 13.sp, color = Color(0xFFAAAAAA))
                        Text(vbrMode.ifEmpty { "—" }, fontSize = 13.sp, color = Color(0xFFB0B0B0))
                    }
                }
            }
        }

        if (errorMsg.isNotEmpty()) Text(errorMsg, fontSize = 13.sp, color = Color(0xFFFF5252))
    }
}

@Composable
fun OpusSettingsScreen(onBack: () -> Unit) {
    var complexity by remember { mutableStateOf(Prefs.opusComplexity.toFloat()) }
    var signalType by remember { mutableStateOf(Prefs.opusSignal) }
    var bandwidth by remember { mutableStateOf(Prefs.opusBandwidth) }
    var dtx by remember { mutableStateOf(Prefs.opusDtx != 0) }
    var vbr by remember { mutableStateOf(Prefs.opusVbr != 0) }
    var bitrateAuto by remember { mutableStateOf(Prefs.opusBitrateKbps == 0) }
    var manualBitrate by remember { mutableStateOf(if (Prefs.opusBitrateKbps > 0) Prefs.opusBitrateKbps else 128) }
    var fec by remember { mutableStateOf(Prefs.opusFec != 0) }
    var packetLoss by remember { mutableStateOf(Prefs.opusPacketLoss.toFloat()) }
    var vbrConstraint by remember { mutableStateOf(Prefs.opusVbrConstraint != 0) }

    Column(
        modifier = Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(top = 44.dp, start = 20.dp, end = 20.dp, bottom = 16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = onBack) { Text("← 返回", fontSize = 16.sp, color = Color(0xFF00E676)) }
            Spacer(Modifier.width(10.dp))
            Text("Opus 高级编码设置", fontSize = 18.sp, color = Color.White)
        }

        Spacer(Modifier.height(4.dp))
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
        Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
            val bws = listOf(1101 to "窄带 (NB) - 8kHz", 1102 to "中带 (MB) - 12kHz", 1103 to "宽带 (WB) - 16kHz", 1104 to "超宽带 (SWB) - 24kHz", 1105 to "全频带 (FB) - 48kHz")
            bws.forEach { (valOf, label) ->
                Row(verticalAlignment = Alignment.CenterVertically) {
                    RadioButton(selected = bandwidth == valOf, onClick = { bandwidth = valOf; Prefs.opusBandwidth = valOf })
                    Text(label, fontSize = 13.sp, color = Color(0xFFD0D0D0))
                }
            }
        }

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Text("自动分配码率", fontSize = 14.sp, color = Color.White)
            Switch(checked = bitrateAuto, onCheckedChange = {
                bitrateAuto = it; Prefs.opusBitrateKbps = if (it) 0 else manualBitrate
            }, colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFF00E676)))
        }
        if (!bitrateAuto) {
            Text("手动码率: ${manualBitrate}kbps", fontSize = 13.sp, color = Color(0xFFB0B0B0))
            Slider(value = manualBitrate.toFloat(), onValueChange = {
                manualBitrate = it.toInt(); Prefs.opusBitrateKbps = it.toInt()
            }, valueRange = 32f..512f, steps = 14, modifier = Modifier.fillMaxWidth()) // OPUS 协议上限 510kbps；步长 32kbps
        }

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Text("DTX (静音时不传输)", fontSize = 14.sp, color = Color.White)
            Switch(checked = dtx, onCheckedChange = {
                dtx = it
                Prefs.opusDtx = if (it) 1 else 0
                // 【核心联动】如果开启了 DTX，必须强制开启 VBR
                if (it) {
                    vbr = true
                    Prefs.opusVbr = 1
                }
            }, colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFF00E676)))
        }

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Column {
                Text("VBR (动态码率)", fontSize = 14.sp, color = if (dtx) Color(0xFF666666) else Color.White)
                if (dtx) {
                    Text("开启DTX时强制启用VBR", fontSize = 11.sp, color = Color(0xFFFF9800))
                }
            }
            Switch(
                checked = vbr,
                onCheckedChange = {
                    vbr = it
                    Prefs.opusVbr = if (it) 1 else 0
                },
                // 【核心约束】如果 DTX 为 true，禁用 VBR 开关的交互
                enabled = !dtx,
                colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFF00E676))
            )
        }

        // VBR 约束（仅 VBR 开启时显示）
        if (vbr) {
            Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
                Column {
                    Text("VBR 码率约束", fontSize = 14.sp, color = Color.White)
                    Text(if (vbrConstraint) "严格不超过目标码率" else "可临时超过目标码率", fontSize = 11.sp, color = Color(0xFF888888))
                }
                Switch(
                    checked = vbrConstraint,
                    onCheckedChange = {
                        vbrConstraint = it
                        Prefs.opusVbrConstraint = if (it) 1 else 0
                    },
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
                onCheckedChange = {
                    fec = it
                    Prefs.opusFec = if (it) 2 else 0 // FEC=2 允许 CELT + FEC（不强制 SILK）
                },
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
            valueRange = 0f..30f,
            steps = 29,
            modifier = Modifier.fillMaxWidth()
        )
    }
}