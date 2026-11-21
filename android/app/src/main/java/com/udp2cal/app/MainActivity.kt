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
import android.os.PowerManager
import android.provider.Settings
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
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
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.clickable
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.scale
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.ContextCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import com.udp2cal.app.service.CaptureMode
import com.udp2cal.app.service.CaptureService
import com.udp2cal.app.service.VbrMode
import com.udp2cal.app.service.labelRes
import com.udp2cal.app.service.vbrRes
import kotlinx.coroutines.launch
import kotlin.math.roundToInt

class MainActivity : ComponentActivity() {
    companion object { private const val TAG = "MainActivity" }

    override fun attachBaseContext(newBase: Context) {
        super.attachBaseContext(AppLocale.wrapActivity(newBase))
    }

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

    /** 是否已加入电池优化白名单（息屏不断流；Compose 侧据此显示引导按钮） */
    private val batteryWhitelisted = mutableStateOf(true)

    private fun refreshBatteryWhitelist() {
        batteryWhitelisted.value = try {
            (getSystemService(Context.POWER_SERVICE) as PowerManager)
                .isIgnoringBatteryOptimizations(packageName)
        } catch (_: Exception) {
            true
        }
    }

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
        // 用户从系统设置返回后重新判定（电池优化白名单 + 麦克风权限）
        refreshBatteryWhitelist()
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
        // 发现广播/连接前就要能选 Wi-Fi 网络；监听本身由 CaptureService 起停
        WifiNetworkBinder.init(applicationContext)
        refreshBatteryWhitelist()
        bindService(Intent(this, CaptureService::class.java), connection, Context.BIND_AUTO_CREATE)

        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            micPermLauncher.launch(Manifest.permission.RECORD_AUDIO)
        } else {
            requestNotifIfNeeded()
        }

        setContent {
            val activity = LocalContext.current as Activity
            var showOpusSettings by rememberSaveable { mutableStateOf(false) }
            var blackout by remember { mutableStateOf(false) }
            val window = activity.window

            // 息屏省电：纯黑全屏 + 隐藏系统栏，保持屏幕常亮；触屏/返回退出。
            DisposableEffect(blackout) {
                val controller = WindowCompat.getInsetsController(window, window.decorView)
                if (blackout) {
                    window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                    WindowCompat.setDecorFitsSystemWindows(window, false)
                    controller.systemBarsBehavior =
                        WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
                    controller.hide(WindowInsetsCompat.Type.systemBars())
                } else {
                    window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                    WindowCompat.setDecorFitsSystemWindows(window, true)
                    controller.show(WindowInsetsCompat.Type.systemBars())
                }
                onDispose {
                    if (blackout) {
                        window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                        WindowCompat.setDecorFitsSystemWindows(window, true)
                        controller.show(WindowInsetsCompat.Type.systemBars())
                    }
                }
            }

            LifecycleEventEffect(Lifecycle.Event.ON_RESUME) {
                if (blackout) {
                    WindowCompat.getInsetsController(window, window.decorView)
                        .hide(WindowInsetsCompat.Type.systemBars())
                }
            }

            BackHandler(enabled = true) {
                if (blackout) blackout = false
                else if (showOpusSettings) showOpusSettings = false
                else activity.moveTaskToBack(false)
            }

            MaterialTheme(colorScheme = darkColorScheme(primary = Color(0xFF00E676), secondary = Color(0xFF00B0FF))) {
                Surface(modifier = Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
                    Box(Modifier.fillMaxSize()) {
                        if (showOpusSettings) {
                            OpusSettingsScreen(
                                onBack = { showOpusSettings = false },
                                service = serviceState.value,
                                onLanguageChanged = { tag ->
                                    Prefs.language = tag
                                    captureService?.refreshNotification()
                                    recreate()
                                }
                            )
                        } else {
                            MainScreen(
                                onStart = { ip, port -> doStart(ip, port) },
                                onStop = { doStop() },
                                onBlackout = { blackout = true },
                                service = serviceState.value,
                                onOpenSettings = { showOpusSettings = true },
                                batteryWhitelisted = batteryWhitelisted.value
                            )
                        }

                        if (blackout) {
                            // 息屏画面：中央呼吸圆环提示采集仍在进行，触屏任意处退出
                            val blackoutBreath = rememberInfiniteTransition(label = "blackoutBreath")
                            val bb by blackoutBreath.animateFloat(
                                initialValue = 0f,
                                targetValue = 1f,
                                animationSpec = infiniteRepeatable(
                                    tween(2000, easing = FastOutSlowInEasing),
                                    RepeatMode.Reverse
                                ),
                                label = "blackoutBreath"
                            )
                            Box(
                                Modifier.fillMaxSize().background(Color.Black)
                                    .pointerInput(Unit) { detectTapGestures { blackout = false } },
                                contentAlignment = Alignment.Center
                            ) {
                                Box(
                                    Modifier.size(150.dp)
                                        .scale(1f + bb * 0.12f)
                                        .alpha(0.06f + bb * 0.10f)
                                        .border(1.dp, Color(0xFF00E676), CircleShape)
                                )
                                Box(
                                    Modifier.size(110.dp)
                                        .scale(1f + bb * 0.07f)
                                        .alpha(0.15f + bb * 0.25f)
                                        .border(2.dp, Color(0xFF00E676), CircleShape)
                                )
                                Box(
                                    Modifier.size(70.dp)
                                        .scale(1f + bb * 0.05f)
                                        .alpha(0.25f + bb * 0.45f)
                                        .border(3.dp, Color(0xFF00E676), CircleShape)
                                )
                            }
                        }
                    }
                }

                // 麦克风权限被拒：必须明确告知，否则用户只会看到"初始化失败"
                if (permissionDenied.value) {
                    AlertDialog(
                        onDismissRequest = { permissionDenied.value = false },
                        title = { Text(stringResource(R.string.permission_title)) },
                        text = { Text(stringResource(R.string.permission_message)) },
                        confirmButton = {
                            TextButton(onClick = {
                                permissionDenied.value = false
                                openAppSettings()
                            }) { Text(stringResource(R.string.action_settings)) }
                        },
                        dismissButton = {
                            TextButton(onClick = { permissionDenied.value = false }) {
                                Text(stringResource(R.string.action_later))
                            }
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
    val shownPort = targetPort.ifBlank { "44044" }
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
                    else { val p = targetPort.toIntOrNull() ?: 44044; onStart(targetIp, p) }
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
                                if (!fs.isFocused) { Prefs.targetPort = targetPort.toIntOrNull() ?: 44044 }
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

/** 带宽码（Opus 枚举值）→ 滑块与状态卡片共用的文案资源 */
private val BW_CODES = intArrayOf(1101, 1102, 1103, 1104, 1105)
private val BW_LABEL_RES = intArrayOf(
    R.string.bw_nb,
    R.string.bw_mb,
    R.string.bw_wb,
    R.string.bw_swb,
    R.string.bw_fb,
)
private val BW_SHORT_RES = intArrayOf(
    R.string.bw_short_nb,
    R.string.bw_short_mb,
    R.string.bw_short_wb,
    R.string.bw_short_swb,
    R.string.bw_short_fb,
)

/** 麦克风音源档位 → 短标签资源 */
private val MIC_SIGNAL_CODES = intArrayOf(3001, 3002, 3003, 3004)
private val MIC_SIGNAL_LABEL_RES = intArrayOf(
    R.string.mic_voice,
    R.string.mic_direct,
    R.string.mic_raw,
    R.string.mic_recognition,
)

@Composable
private fun bwDisplayName(bw: Int): String {
    val index = BW_CODES.indexOf(bw)
    return if (index >= 0) stringResource(BW_LABEL_RES[index]) else ""
}

/** 反向接收目标水位五档（ms） */
private val WATER_TARGET_STEPS_MS = intArrayOf(80, 120, 160, 200, 300)

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
        run {
            val values = MIC_SIGNAL_CODES
            val lastIdx = values.lastIndex
            val idx = values.indexOf(signalType).takeIf { it >= 0 } ?: 1
            Slider(
                value = idx.toFloat(),
                onValueChange = { v ->
                    val i = v.roundToInt().coerceIn(0, lastIdx)
                    if (values[i] != signalType) {
                        signalType = values[i]
                        Prefs.opusSignal = values[i]
                    }
                },
                valueRange = 0f..lastIdx.toFloat(),
                steps = values.size - 2,
                modifier = Modifier.fillMaxWidth()
            )
            Row(Modifier.fillMaxWidth(), Arrangement.SpaceBetween) {
                MIC_SIGNAL_LABEL_RES.forEachIndexed { i, res ->
                    Text(
                        stringResource(res),
                        fontSize = 10.sp,
                        color = if (i == idx) Color(0xFF00E676) else Color(0xFF888888)
                    )
                }
            }
            Text(
                stringResource(R.string.settings_mic_source_hint),
                fontSize = 11.sp, color = Color(0xFF888888)
            )
        }

        Text(stringResource(R.string.settings_bandwidth), fontSize = 14.sp, color = Color.White)
        run {
            // 与码率滑块保持完全一致的版式：标题 → 当前值 → 滑块 → 刻度 → 说明
            val lastIdx = BW_CODES.lastIndex
            val idx = BW_CODES.indexOf(bandwidth).takeIf { it >= 0 } ?: lastIdx
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
                BW_SHORT_RES.forEachIndexed { i, res ->
                    Text(
                        stringResource(res),
                        fontSize = 10.sp,
                        color = if (i == idx) Color(0xFF00E676) else Color(0xFF888888)
                    )
                }
            }
            Text(
                stringResource(R.string.settings_bandwidth_hint),
                fontSize = 11.sp, color = Color(0xFF888888)
            )
        }

        // 码率：五档滑块。协议上限 512 kbps（10 ms 帧下 640 字节/帧，远低于 1275）
        Text(stringResource(R.string.settings_bitrate), fontSize = 14.sp, color = Color.White)
        run {
            val steps = Udp2CalProtocol.BITRATE_STEPS_KBPS
            val lastIdx = steps.lastIndex
            val idx = steps.indexOf(bitrate).takeIf { it >= 0 } ?: lastIdx
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
                stringResource(
                    R.string.settings_bitrate_hint,
                    steps[idx],
                    steps[idx] * 1000 / 8 * 10 / 1000,
                ),
                fontSize = 11.sp, color = Color(0xFF888888)
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
