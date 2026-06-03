package com.udp2mic.app

import android.Manifest
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.provider.Settings
import android.net.Uri
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.ContextCompat
import com.udp2mic.app.service.CaptureService

class MainActivity : ComponentActivity() {
    companion object {
        private const val TAG = "MainActivity"
    }

    private var captureService: CaptureService? = null
    private var serviceBound = false
    private var pendingStart: ((Boolean) -> Unit)? = null

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
            captureService = (service as CaptureService.LocalBinder).getService()
            serviceBound = true
            pendingStart?.invoke(false)
            pendingStart = null
        }
        override fun onServiceDisconnected(name: ComponentName?) {
            captureService = null
            serviceBound = false
        }
    }

    private val permLauncher = registerForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) {}

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Prefs.init(this)
        bindService(Intent(this, CaptureService::class.java), connection, Context.BIND_AUTO_CREATE)

        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO)
            != PackageManager.PERMISSION_GRANTED
        ) permLauncher.launch(Manifest.permission.RECORD_AUDIO)

        if (Build.VERSION.SDK_INT >= 33 &&
            ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS)
            != PackageManager.PERMISSION_GRANTED
        ) permLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)

        setContent {
            MaterialTheme(
                colorScheme = darkColorScheme(
                    primary = Color(0xFF00E676),
                    secondary = Color(0xFF00B0FF),
                )
            ) {
                Surface(modifier = Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
                    MainScreen(
                        onStart = { ip, port, testTone, noiseReduction, voiceEnhance ->
                        doStart(ip, port, testTone, noiseReduction, voiceEnhance)
                    },
                        onStop = { doStop() },
                        service = captureService
                    )
                }
            }
        }
    }

    /** 自动检测设备支持的最佳采样率，按优先级降序尝试 */
    private fun detectBestSampleRate(): Int {
        // 仅使用协议支持的采样率: 48k, 24k, 16k, 8k
        // 排除 44.1kHz — 协议 v1 不支持, 且映射到 48k 会导致帧大小不匹配 → 颤音/升调
        val candidates = listOf(48000, 24000, 16000, 8000)
        for (hz in candidates) {
            val bufSize = android.media.AudioRecord.getMinBufferSize(
                hz,
                android.media.AudioFormat.CHANNEL_IN_MONO,
                android.media.AudioFormat.ENCODING_PCM_16BIT
            )
            if (bufSize > 0 && bufSize != android.media.AudioRecord.ERROR_BAD_VALUE) {
                Log.d(TAG, "自动协商: 选择采样率 ${hz}Hz")
                return hz
            }
            Log.d(TAG, "自动协商: 采样率 ${hz}Hz 不支持 (bufSize=$bufSize)")
        }
        return 48000
    }

    /** 根据采样率自动选择建议码率 (kbps)
     *  48kHz→128kbps: Opus CBR 高保真传输 */
    private fun autoBitrate(sampleRateHz: Int): Int = when {
        sampleRateHz >= 48000 -> 128
        sampleRateHz >= 24000 -> 64
        sampleRateHz >= 16000 -> 48
        sampleRateHz >= 12000 -> 32
        else -> 24
    }

    private fun doStart(ip: String, port: Int, testTone: Boolean, noiseReduction: Boolean, voiceEnhance: Boolean) {
        Prefs.targetIp = ip; Prefs.targetPort = port; Prefs.testToneMode = testTone; Prefs.noiseReduction = noiseReduction; Prefs.voiceEnhance = voiceEnhance

        // 自动协商: 检测最佳采样率并计算对应码率
        val sampleRateHz = detectBestSampleRate()
        val bitrateKbps = autoBitrate(sampleRateHz)
        Log.i(TAG, "启动采集: ip=$ip port=$port sr=${sampleRateHz}Hz br=${bitrateKbps}kbps testTone=$testTone ng=$noiseReduction ve=$voiceEnhance")

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            val pm = getSystemService(Context.POWER_SERVICE) as android.os.PowerManager
            if (!pm.isIgnoringBatteryOptimizations(packageName)) {
                try { startActivity(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS).apply {
                    data = Uri.parse("package:$packageName") }) } catch (_: Exception) {}
            }
        }

        val action = { _: Boolean ->
            startService(Intent(this, CaptureService::class.java))
            captureService?.startCapture(sampleRateHz, bitrateKbps, ip, port, testTone, noiseReduction, voiceEnhance)
            Unit
        }
        if (captureService != null) action(false) else pendingStart = action
    }

    private fun doStop() { captureService?.stopCapture() }

    override fun onDestroy() {
        if (serviceBound) unbindService(connection)
        super.onDestroy()
    }
}

@Composable
fun MainScreen(
    onStart: (String, Int, Boolean, Boolean, Boolean) -> Unit,
    onStop: () -> Unit,
    service: CaptureService?
) {
    var targetIp by remember { mutableStateOf(Prefs.targetIp) }
    var targetPort by remember { mutableStateOf(Prefs.targetPort.toString()) }
    var isRunning by remember { mutableStateOf(false) }
    var testToneMode by remember { mutableStateOf(Prefs.testToneMode) }
    var noiseReduction by remember { mutableStateOf(Prefs.noiseReduction) }
    var voiceEnhance by remember { mutableStateOf(Prefs.voiceEnhance) }

    var negSr by remember { mutableStateOf("") }
    var errorMsg by remember { mutableStateOf("") }
    var agcGainDb by remember { mutableStateOf(0f) }
    var nsActive by remember { mutableStateOf(false) }
    var veActive by remember { mutableStateOf(false) }

    // 监听服务状态
    LaunchedEffect(service) {
        service?.status?.collect { status ->
            isRunning = status.isRunning
            errorMsg = status.errorMsg
            negSr = if (status.sampleRateHz > 0) "${status.sampleRateHz}Hz / ${status.bitrateTargetKbps}kbps" else ""
            agcGainDb = status.agcGainDb
            nsActive = status.nsActive
            veActive = status.veActive
        }
    }

    // 顶部内边距避免被摄像头遮挡
    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(top = 44.dp, start = 20.dp, end = 20.dp, bottom = 16.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(10.dp)
    ) {
        Spacer(Modifier.height(0.dp))

        Text("UDP2Mic", fontSize = 22.sp, color = Color(0xFF00E676))

        OutlinedTextField(
            value = targetIp,
            onValueChange = { targetIp = it },
            label = { Text("目标 IP") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth()
        )

        OutlinedTextField(
            value = targetPort,
            onValueChange = { targetPort = it },
            label = { Text("端口") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth()
        )

        Spacer(Modifier.height(4.dp))

        Button(
            onClick = {
                if (isRunning) { isRunning = false; onStop() }
                else { isRunning = true; val port = targetPort.toIntOrNull() ?: 8899; onStart(targetIp, port, testToneMode, noiseReduction, voiceEnhance) }
            },
            modifier = Modifier.fillMaxWidth().height(48.dp),
            colors = ButtonDefaults.buttonColors(
                containerColor = if (isRunning) Color(0xFFD32F2F) else Color(0xFF00C853)
            )
        ) {
            Text(if (isRunning) "停止采集" else "开始采集", fontSize = 18.sp)
        }

        // 测试音模式开关
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.SpaceBetween
        ) {
            Text("1kHz测试音", fontSize = 14.sp, color = if (testToneMode) Color(0xFFFF9800) else Color(0xFFAAAAAA))
            Switch(
                checked = testToneMode,
                onCheckedChange = { testToneMode = it; Prefs.testToneMode = it; if (it) noiseReduction = false },
                colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFFFF9800))
            )
        }

        // 噪声门开关
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.SpaceBetween
        ) {
            Text("噪声门", fontSize = 14.sp, color = if (noiseReduction) Color(0xFF00B0FF) else Color(0xFFAAAAAA))
            Switch(
                checked = noiseReduction,
                onCheckedChange = { noiseReduction = it; Prefs.noiseReduction = it; if (it) testToneMode = false },
                colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFF00B0FF))
            )
        }

        // 人声降噪开关 (Wiener降噪, 去除背景杂音突出人声)
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.SpaceBetween
        ) {
            Text("人声降噪", fontSize = 14.sp, color = if (voiceEnhance) Color(0xFF7C4DFF) else Color(0xFFAAAAAA))
            Switch(
                checked = voiceEnhance,
                onCheckedChange = { voiceEnhance = it; Prefs.voiceEnhance = it; if (it) testToneMode = false },
                colors = SwitchDefaults.colors(checkedTrackColor = Color(0xFF7C4DFF))
            )
        }

        // 状态信息
        if (isRunning) {
            if (negSr.isNotEmpty()) {
                Text("协商: $negSr", fontSize = 13.sp, color = Color(0xFF00B0FF))
            }
            if (agcGainDb > 0f) {
                val nsLabel = if (nsActive) "NS✓" else ""
                val veLabel = if (veActive) "VE✓" else ""
                Text("AGC: +${"%.1f".format(agcGainDb)}dB $nsLabel $veLabel", fontSize = 12.sp, color = Color(0xFFAAAAAA))
            }
        }

        // 错误信息
        if (errorMsg.isNotEmpty()) {
            Text(errorMsg, fontSize = 13.sp, color = Color(0xFFFF5252))
        }
    }
}
