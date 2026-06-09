package com.udp2cal.app

import android.Manifest
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.graphics.Color
import android.media.AudioManager
import android.os.Bundle
import android.os.IBinder
import android.view.KeyEvent
import android.view.MotionEvent
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat
import androidx.lifecycle.lifecycleScope
import com.udp2cal.app.service.CaptureService
import kotlinx.coroutines.*

class MainActivity : ComponentActivity() {
    companion object { private const val TAG = "MainActivity" }

    private var captureService: CaptureService? = null
    private var serviceBound = false
    private var pendingStart: (() -> Unit)? = null

    private lateinit var connectButton: Button
    private lateinit var statusText: TextView
    private lateinit var mainPanel: LinearLayout
    private lateinit var aboutPanel: LinearLayout

    private var isRunning = false
    private var scanScope: CoroutineScope? = null
    /** 当前音源，用于音量键路由 */
    private var currentAudioSource: String = ""
    private var showAbout = false
    /** 手势滑动起点 */
    private var touchStartX = 0f

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
            captureService = (service as CaptureService.LocalBinder).getService()
            serviceBound = true
            pendingStart?.invoke(); pendingStart = null
            // 开始收集服务状态
            collectStatus()
        }
        override fun onServiceDisconnected(name: ComponentName?) {
            captureService = null; serviceBound = false
        }
    }

    private val micPermLauncher = registerForActivityResult(ActivityResultContracts.RequestPermission()) { _ -> }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        Prefs.init(applicationContext)
        bindService(Intent(this, CaptureService::class.java), connection, Context.BIND_AUTO_CREATE)

        connectButton = findViewById(R.id.connectButton)
        statusText = findViewById(R.id.statusText)
        mainPanel = findViewById(R.id.mainPanel)
        aboutPanel = findViewById(R.id.aboutPanel)

        connectButton.setOnClickListener { onConnectClick() }

        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            micPermLauncher.launch(Manifest.permission.RECORD_AUDIO)
        }
    }

    private fun onConnectClick() {
        if (isRunning) {
            // 断开连接
            doStop()
        } else {
            // 自动发现并连接
            doAutoConnect()
        }
    }

    private fun doAutoConnect() {
        setStatus("正在搜索局域网内的PC端...", Color.parseColor("#FF9800"))
        connectButton.isEnabled = false

        scanScope = CoroutineScope(Dispatchers.Main + SupervisorJob())
        scanScope?.launch {
            val devices = withContext(Dispatchers.IO) {
                DiscoveryManager.discoverServers()
            }
            if (devices.isEmpty()) {
                setStatus("未找到接收端，请确保 PC 端已打开且在同一 Wi-Fi 下", Color.parseColor("#FF5252"))
                connectButton.isEnabled = true
            } else {
                val device = devices.first()
                Prefs.targetIp = device.ip
                Prefs.targetPort = device.port
                setStatus("已找到 ${device.deviceName}，正在连接...", Color.parseColor("#00E676"))
                doStart(device.ip, device.port)
            }
        }
    }

    private fun doStart(ip: String, port: Int) {
        Prefs.targetIp = ip; Prefs.targetPort = port
        val sampleRateHz = 48000  // 测试：48kHz 验证编码性能
        val bitrateKbps = 256  // 均衡：256kbps，CPU 20%以内

        val action = {
            startService(Intent(this, CaptureService::class.java))
            captureService?.startCapture(sampleRateHz, bitrateKbps, ip, port)
            Unit
        }
        if (captureService != null) action() else pendingStart = action
    }

    private fun doStop() {
        pendingStart = null
        captureService?.stopCapture()
        stopService(Intent(this, CaptureService::class.java))
        scanScope?.cancel()
        scanScope = null
    }

    private fun setStatus(msg: String, color: Int = Color.parseColor("#AAAAAA")) {
        runOnUiThread {
            statusText.text = msg
            statusText.setTextColor(color)
            statusText.visibility = TextView.VISIBLE
        }
    }

    private fun toggleAbout() {
        showAbout = !showAbout
        mainPanel.visibility = if (showAbout) android.view.View.GONE else android.view.View.VISIBLE
        aboutPanel.visibility = if (showAbout) android.view.View.VISIBLE else android.view.View.GONE
    }

    override fun onTouchEvent(event: MotionEvent?): Boolean {
        event ?: return super.onTouchEvent(event)
        when (event.action) {
            MotionEvent.ACTION_DOWN -> touchStartX = event.x
            MotionEvent.ACTION_UP -> {
                val dx = event.x - touchStartX
                if (kotlin.math.abs(dx) > 80) { toggleAbout(); return true }
            }
        }
        return super.onTouchEvent(event)
    }

    /** 十字键上下：调音量；左右：切换关于页 */
    override fun onKeyDown(keyCode: Int, event: KeyEvent?): Boolean {
        when (keyCode) {
            KeyEvent.KEYCODE_DPAD_LEFT, KeyEvent.KEYCODE_DPAD_RIGHT -> {
                toggleAbout(); return true
            }
            KeyEvent.KEYCODE_DPAD_UP, KeyEvent.KEYCODE_DPAD_DOWN -> {
                if (showAbout) { toggleAbout(); return true }
                if (isRunning) {
                    val audioManager = getSystemService(Context.AUDIO_SERVICE) as? AudioManager ?: return super.onKeyDown(keyCode, event)
                    val isVoice = currentAudioSource.contains("系统硬件降噪") || currentAudioSource.contains("AEC")
                    val streamType = if (isVoice) AudioManager.STREAM_VOICE_CALL else AudioManager.STREAM_MUSIC
                    val direction = if (keyCode == KeyEvent.KEYCODE_DPAD_UP) AudioManager.ADJUST_RAISE else AudioManager.ADJUST_LOWER
                    audioManager.adjustStreamVolume(streamType, direction, AudioManager.FLAG_SHOW_UI)
                    return true
                }
            }
        }
        return super.onKeyDown(keyCode, event)
    }

    private fun buildStatusMsg(line1: String, src: String): String {
        val simple = when {
            src.contains("AEC已启用") -> "NS + AGC + AEC"
            src.contains("系统硬件降噪") -> "NS + AGC"
            src.contains("裸麦克风") -> "MIC"
            else -> src
        }
        return if (simple.isNotEmpty()) "$line1\n$simple" else line1
    }

    private fun collectStatus() {
        lifecycleScope.launch {
            captureService?.status?.collect { status ->
                isRunning = status.isRunning
                currentAudioSource = status.audioSource
                runOnUiThread {
                    when {
                        status.isRunning && status.connected -> {
                            connectButton.text = "断开连接"
                            connectButton.setBackgroundColor(Color.parseColor("#D32F2F"))
                            connectButton.isEnabled = true
                            setStatus(buildStatusMsg("已连接", status.audioSource), Color.parseColor("#00E676"))
                        }
                        status.isRunning && !status.connected -> {
                            connectButton.text = "断开连接"
                            connectButton.setBackgroundColor(Color.parseColor("#D32F2F"))
                            connectButton.isEnabled = true
                            setStatus(buildStatusMsg("等待连接确认...", status.audioSource), Color.parseColor("#FF9800"))
                        }
                        status.errorMsg.isNotEmpty() -> {
                            connectButton.text = "自动连接"
                            connectButton.setBackgroundColor(Color.parseColor("#00C853"))
                            connectButton.isEnabled = true
                            setStatus(status.errorMsg, Color.parseColor("#FF5252"))
                        }
                        else -> {
                            connectButton.text = "自动连接"
                            connectButton.setBackgroundColor(Color.parseColor("#00C853"))
                            connectButton.isEnabled = true
                            statusText.visibility = TextView.GONE
                        }
                    }
                }
            }
        }
    }

    override fun onDestroy() {
        pendingStart = null
        scanScope?.cancel()
        if (serviceBound) unbindService(connection)
        super.onDestroy()
    }
}
