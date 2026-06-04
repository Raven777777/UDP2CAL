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
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
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

class MainActivity : ComponentActivity() {
    companion object { private const val TAG = "MainActivity" }

    private var captureService: CaptureService? = null
    private var serviceBound = false
    private var pendingStart: ((Boolean) -> Unit)? = null
    private val serviceState = mutableStateOf<CaptureService?>(null)

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
            captureService = (service as CaptureService.LocalBinder).getService()
            serviceState.value = captureService
            serviceBound = true
            pendingStart?.invoke(false); pendingStart = null
        }
        override fun onServiceDisconnected(name: ComponentName?) {
            captureService = null; serviceState.value = null; serviceBound = false
        }
    }

    private val permLauncher = registerForActivityResult(ActivityResultContracts.RequestPermission()) {}

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Prefs.init(this)
        bindService(Intent(this, CaptureService::class.java), connection, Context.BIND_AUTO_CREATE)

        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) 
            permLauncher.launch(Manifest.permission.RECORD_AUDIO)

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
                            onStart = { ip, port, testTone, noiseGate -> doStart(ip, port, testTone, noiseGate) },
                            onStop = { doStop() },
                            service = serviceState.value,
                            onOpenSettings = { showOpusSettings = true }
                        )
                    }
                }
            }
        }
    }

    private fun autoBitrate(sampleRateHz: Int): Int = when {
        sampleRateHz >= 48000 -> 128; sampleRateHz >= 24000 -> 64; else -> 24
    }

    private fun doStart(ip: String, port: Int, testTone: Boolean, noiseGate: Boolean) {
        Prefs.targetIp = ip; Prefs.targetPort = port; Prefs.testToneMode = testTone; Prefs.noiseGate = noiseGate
        val sampleRateHz = 48000
        val bitrateKbps = if (Prefs.opusBitrateKbps > 0) Prefs.opusBitrateKbps else autoBitrate(sampleRateHz)

        val action = { _: Boolean ->
            startService(Intent(this, CaptureService::class.java))
            captureService?.startCapture(sampleRateHz, bitrateKbps, ip, port, testTone, noiseGate)
            Unit
        }
        if (captureService != null) action(false) else pendingStart = action
    }

    private fun doStop() { captureService?.stopCapture() }
    override fun onDestroy() { if (serviceBound) unbindService(connection) ; super.onDestroy() }
}

@Composable
fun MainScreen(
    onStart: (String, Int, Boolean, Boolean) -> Unit,
    onStop: () -> Unit,
    service: CaptureService?,
    onOpenSettings: () -> Unit
) {
    var targetIp by remember { mutableStateOf(Prefs.targetIp) }
    var targetPort by remember { mutableStateOf(Prefs.targetPort.toString()) }
    var isRunning by remember { mutableStateOf(false) }
    var testToneMode by remember { mutableStateOf(Prefs.testToneMode) }
    var noiseGate by remember { mutableStateOf(Prefs.noiseGate) }
    var agcEnabled by remember { mutableStateOf(Prefs.agcEnabled) }
    var agcMaxGain by remember { mutableStateOf(Prefs.agcMaxGain) }
    var negSr by remember { mutableStateOf("") }
    var errorMsg by remember { mutableStateOf("") }
    var agcGainDb by remember { mutableStateOf(0f) }
    var ngActive by remember { mutableStateOf(false) }
    var agcGainX by remember { mutableStateOf(0f) }

    LaunchedEffect(service) {
        service?.status?.collect { status ->
            isRunning = status.isRunning; errorMsg = status.errorMsg
            negSr = if (status.sampleRateHz > 0) "${status.sampleRateHz}Hz / ${status.bitrateTargetKbps}kbps" else ""
            agcGainDb = status.agcGainDb; ngActive = status.ngActive; agcGainX = status.agcGainX
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
            })

        OutlinedTextField(value = targetPort, onValueChange = { targetPort = it },
            label = { Text("端口") }, singleLine = true,
            modifier = Modifier.fillMaxWidth().onFocusChanged { fs ->
                if (!fs.isFocused) { Prefs.targetPort = targetPort.toIntOrNull() ?: 8899 }
            })

        Spacer(Modifier.height(4.dp))

        Button(onClick = {
            if (isRunning) { onStop() }
            else { val p = targetPort.toIntOrNull() ?: 8899; onStart(targetIp, p, testToneMode, noiseGate) }
        }, modifier = Modifier.fillMaxWidth().height(48.dp),
            colors = ButtonDefaults.buttonColors(containerColor = if (isRunning) Color(0xFFD32F2F) else Color(0xFF00C853))
        ) { Text(if (isRunning) "停止采集" else "开始采集", fontSize = 18.sp) }

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Text("1kHz测试音", fontSize = 14.sp, color = if (testToneMode) Color(0xFFFF9800) else Color(0xFFAAAAAA))
            Switch(checked = testToneMode, onCheckedChange = {
                testToneMode = it; Prefs.testToneMode = it
                if (it) { noiseGate = false; Prefs.noiseGate = false }
                if (isRunning) { onStop(); val p = targetPort.toIntOrNull() ?: 8899; onStart(targetIp, p, it, noiseGate) }
            }, colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFFFF9800)))
        }

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Text("噪声门", fontSize = 14.sp, color = if (noiseGate) Color(0xFF00B0FF) else Color(0xFFAAAAAA))
            Switch(checked = noiseGate, onCheckedChange = { noiseGate = it; Prefs.noiseGate = it }, colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFF00B0FF)))
        }

        Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) {
            Text("AGC 自动增益", fontSize = 14.sp, color = if (agcEnabled) Color(0xFF00E676) else Color(0xFFAAAAAA))
            Switch(checked = agcEnabled, onCheckedChange = { agcEnabled = it; Prefs.agcEnabled = it }, colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFF00E676)))
        }
        if (!agcEnabled) {
            Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween, Alignment.CenterVertically) { Text("固定增益: ${agcMaxGain}x", fontSize = 13.sp, color = Color(0xFFB0B0B0)) }
            Slider(value = (agcMaxGain / 10).toFloat(), onValueChange = {
                agcMaxGain = (it * 10).toInt().coerceIn(0, 200); Prefs.agcMaxGain = agcMaxGain
            }, valueRange = 0f..20f, steps = 19, modifier = Modifier.fillMaxWidth())
        }

        OutlinedButton(onClick = onOpenSettings, modifier = Modifier.fillMaxWidth()) { Text("Opus 编码设置", color = Color(0xFFAAAAAA)) }

        val bwNames = listOf("窄带", "中带", "宽带", "超宽带", "全频带")
        val bwIdx = when (Prefs.opusBandwidth) { 1101 -> 0; 1102 -> 1; 1103 -> 2; 1104 -> 3; else -> 4 }
        val sigNames = listOf("语音", "音乐", "自动")
        val sigIdx = when (Prefs.opusSignal) { 3001 -> 0; 3002 -> 1; else -> 2 }
        Text("复杂度=${Prefs.opusComplexity} ${sigNames[sigIdx]} ${bwNames[bwIdx]} ${if (Prefs.opusVbr != 0) "VBR" else "CBR"}/DTX=${if (Prefs.opusDtx!=0)"开"else"关"} 码率=${if (Prefs.opusBitrateKbps > 0) "${Prefs.opusBitrateKbps}k" else "自动"}", fontSize = 11.sp, color = Color(0xFF666666))

        if (isRunning) {
            if (negSr.isNotEmpty()) Text(negSr, fontSize = 13.sp, color = Color(0xFF00B0FF))
            if (agcGainDb > 0f) {
                val ng = if (ngActive) "NG✓" else ""
                val displayGainX = if (agcEnabled) agcGainX else agcMaxGain.toFloat()
                val displayGainDb = if (agcEnabled) agcGainDb else (20 * kotlin.math.log10(displayGainX.toDouble().coerceAtLeast(1e-6))).toFloat()
                Text(text = "${if (agcEnabled) "AGC 自动" else "固定增益"}: +${"%.1f".format(displayGainDb)}dB (${displayGainX.toInt()}x) $ng", fontSize = 12.sp, color = if (agcEnabled) Color(0xFF00E676) else Color(0xFFAAAAAA))
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
    var manualBitrate by remember { mutableStateOf(if (Prefs.opusBitrateKbps > 0) Prefs.opusBitrateKbps else 64) }

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
            val options = listOf(3001 to "语音", 3002 to "音乐", 3005 to "自动")
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
            }, valueRange = 8f..256f, steps = 31, modifier = Modifier.fillMaxWidth())
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
    }
}