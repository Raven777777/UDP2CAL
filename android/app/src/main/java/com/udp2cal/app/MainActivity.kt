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
import androidx.activity.SystemBarStyle
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
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
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.scale
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import com.udp2cal.app.service.CaptureService

class MainActivity : ComponentActivity() {
    companion object { private const val TAG = "MainActivity" }

    override fun attachBaseContext(newBase: Context) {
        super.attachBaseContext(AppLocale.wrapActivity(newBase))
    }

    private var captureService: CaptureService? = null
    /** bindService() 的返回值：为 true 就必须 unbind，即使 onServiceConnected 还没回调 */
    private var bindRequested = false
    private var pendingStart: (() -> Unit)? = null
    private val serviceState = mutableStateOf<CaptureService?>(null)

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
            captureService = (service as CaptureService.LocalBinder).getService()
            serviceState.value = captureService
            pendingStart?.invoke(); pendingStart = null
        }
        override fun onServiceDisconnected(name: ComponentName?) {
            captureService = null; serviceState.value = null
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

        // M3 界面统一走 edge-to-edge：内容由各页面的 Scaffold 通过 WindowInsets 自行避让，
        // 状态栏/导航栏保持透明，卡片可以自然滑到系统栏下方。
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT)
        )

        Prefs.init(applicationContext)
        // 发现广播/连接前就要能选 Wi-Fi 网络；监听本身由 CaptureService 起停
        WifiNetworkBinder.init(applicationContext)
        refreshBatteryWhitelist()
        bindRequested = bindService(Intent(this, CaptureService::class.java), connection, Context.BIND_AUTO_CREATE)

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
            // edge-to-edge 常开，因此退出息屏时恢复的也是 false（由 Scaffold 再次避让系统栏）。
            DisposableEffect(blackout) {
                val controller = WindowCompat.getInsetsController(window, window.decorView)
                if (blackout) {
                    window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                    controller.systemBarsBehavior =
                        WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
                    controller.hide(WindowInsetsCompat.Type.systemBars())
                } else {
                    window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                    controller.show(WindowInsetsCompat.Type.systemBars())
                }
                onDispose {
                    if (blackout) {
                        window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
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

            Udp2CalTheme {
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
                                val ringColor = MaterialTheme.colorScheme.primary
                                Box(
                                    Modifier.size(150.dp)
                                        .scale(1f + bb * 0.12f)
                                        .alpha(0.06f + bb * 0.10f)
                                        .border(1.dp, ringColor, CircleShape)
                                )
                                Box(
                                    Modifier.size(110.dp)
                                        .scale(1f + bb * 0.07f)
                                        .alpha(0.15f + bb * 0.25f)
                                        .border(2.dp, ringColor, CircleShape)
                                )
                                Box(
                                    Modifier.size(70.dp)
                                        .scale(1f + bb * 0.05f)
                                        .alpha(0.25f + bb * 0.45f)
                                        .border(3.dp, ringColor, CircleShape)
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
        val sampleRateHz = Udp2CalProtocol.OPUS_SAMPLE_RATE
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

    override fun onDestroy() {
        pendingStart = null
        if (bindRequested) {
            bindRequested = false
            runCatching { unbindService(connection) }
        }
        super.onDestroy()
    }
}
