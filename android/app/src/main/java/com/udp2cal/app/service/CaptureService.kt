package com.udp2cal.app.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.net.Network
import android.os.Binder
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import com.udp2cal.app.WifiNetworkBinder
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeoutOrNull

/**
 * 采集服务（v3）：Service 外壳。
 *
 * 具体会话编排在 [CaptureLifecycle]，正向/反向/控制分别封装在
 * [ForwardLink] / [ReverseLink] / [ControlLoop]。本类只负责：
 *   * Binder 与前台通知；
 *   * Wi-Fi 网络回调转发；
 *   * 服务停机时的协程 join 与原生句柄归还顺序。
 */
class CaptureService : Service(), WifiNetworkBinder.Listener {
    companion object {
        private const val TAG = SERVICE_TAG
    }

    private val binder = LocalBinder()
    private val serviceScope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    private val statusBus = StatusBus()
    private var notificationJob: Job? = null

    private lateinit var lifecycle: CaptureLifecycle

    val status: StateFlow<CaptureStatus> = statusBus.flow

    inner class LocalBinder : Binder() {
        fun getService(): CaptureService = this@CaptureService
    }

    override fun onBind(intent: Intent?): IBinder = binder

    override fun onCreate() {
        super.onCreate()
        lifecycle = CaptureLifecycle(this, serviceScope, statusBus)
        // 整个服务生命周期监听 Wi-Fi：绑定网络与断网重连都以这里为准
        WifiNetworkBinder.start(applicationContext, this)
    }

    override fun onWifiAvailable(network: Network) = lifecycle.onWifiAvailable(network)

    override fun onWifiLost(network: Network) = lifecycle.onWifiLost(network)

    // startForeground 必须在 startService→onStartCommand 时立即调用（5s 限制）
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent == null) {
            stopSelf()
            return START_NOT_STICKY
        }
        createNotificationChannel()
        try {
            startForeground(1, buildNotification())
        } catch (e: Exception) {
            Log.e(TAG, "startForeground failed", e)
            stopSelf()
            return START_NOT_STICKY
        }
        if (notificationJob?.isActive != true) {
            notificationJob = serviceScope.launch {
                // 通知只展示连接状态/模式/音源。码率与反向统计每秒都在变，
                // 但通知文本不变——按内容去重，避免每次 _status 发射都
                // 走一遍 startForeground（binder 调用 + SystemUI 唤醒）。
                statusBus.flow
                    .map { notificationKey(it) }
                    .distinctUntilChanged()
                    .collect {
                        try {
                            startForeground(1, buildNotification())
                        } catch (_: Exception) {
                        }
                    }
            }
        }
        return START_NOT_STICKY
    }

    fun startCapture(
        sampleRateHz: Int,
        bitrateKbps: Int,
        targetIp: String,
        targetPort: Int,
        testTone: Boolean = false,
    ) = lifecycle.startCapture(sampleRateHz, bitrateKbps, targetIp, targetPort, testTone)

    fun stopCapture() {
        // 先停通知收集器：否则 IO 侧收到 connected=false 后会在
        // CaptureLifecycle 的 stopForeground 之后再次 startForeground，
        // 停止后仍显示"等待连接"的通知。
        notificationJob?.cancel()
        notificationJob = null
        lifecycle.stopCapture()
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel("udp2cal_capture", "采集服务", NotificationManager.IMPORTANCE_LOW)
                    .apply { setShowBadge(false) }
            )
        }
    }

    /** 通知栏实际展示的内容：只有这些字段变化才需要重建前台通知 */
    private data class NotificationKey(
        val connected: Boolean,
        val modeLabel: String,
        val audioLabel: String,
    )

    private fun notificationKey(status: CaptureStatus): NotificationKey = NotificationKey(
        connected = status.connected,
        modeLabel = if (status.opusMode.contains("语音")) "语音" else "音乐",
        audioLabel = when {
            status.audioSource.contains("AEC已启用") -> "NS+AGC+AEC"
            status.audioSource.contains("硬件降噪") -> "NS+AGC"
            else -> "MIC-Direct"
        },
    )

    private fun buildNotification(): Notification {
        val key = notificationKey(statusBus.value)
        val connStr = if (key.connected) "已连接" else "等待连接"
        return NotificationCompat.Builder(this, "udp2cal_capture")
            .setContentTitle("UDP2CAL")
            .setContentText("$connStr · ${key.modeLabel} ${key.audioLabel}")
            .setSmallIcon(android.R.drawable.ic_btn_speak_now)
            .setOngoing(true)
            .build()
    }

    override fun onDestroy() {
        // 在 stopCapture 清空任务槽之前取快照；顺序有意义：
        // 先等 control（NonCancellable 里的 jitter.reset()），再等 capture
        // （它的收尾会关闭反向 socket），reverseRxLoop 阻塞在 sock.receive()
        // 上、协程取消唤不醒它，socket 关闭后才会跟着退出。
        val oldJobs = lifecycle.sessionJobs()
        lifecycle.stopCapture()
        serviceScope.cancel()
        WifiNetworkBinder.stop()
        val allExited = runBlocking {
            withTimeoutOrNull(1000) {
                oldJobs.forEach { it.cancelAndJoin() }
                true
            } ?: false
        }
        if (!allExited) {
            // 仍有协程没退出（如 receive() 卡死）：句柄宁可不归还——泄漏一小块
            // Rust 堆，远好于与还在执行的原生调用竞争。
            Log.w(TAG, "协程未在 1s 内退出，保留原生句柄以避免 use-after-free")
            super.onDestroy()
            return
        }
        // 热循环专用线程此时都已空闲：随服务一起停掉，不留孤儿线程。
        lifecycle.closeThreads()
        // 原生句柄用完之后必须归还：JitterBuffer/DriftController/DeviceRateEstimator
        // 的状态在 Rust 堆上（protocol/jni.rs），Kotlin 只是持有 jlong 句柄。
        lifecycle.closeNative()
        super.onDestroy()
    }
}
