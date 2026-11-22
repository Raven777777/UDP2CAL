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
import com.udp2cal.app.AppLocale
import com.udp2cal.app.R
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
    @Volatile private var notificationJob: Job? = null

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

    /** 高级设置：反向（PC→手机）目标水位，运行时立即生效 */
    fun setReverseWaterTargetMs(ms: Int) = lifecycle.setReverseWaterTargetMs(ms)

    /**
     * 会话在无重启的情况下自然结束（失败/读失败/停采集）。
     *
     * 停止通知收集器，避免 stopForeground 之后的下一次状态发射再次
     * startForeground，留下与任何会话都不对应的残留通知。
     */
    internal fun onSessionEnded() {
        notificationJob?.cancel()
        notificationJob = null
    }

    /** 语言切换后立即重建前台通知（通知文本不随状态变化，需显式刷新）。 */
    fun refreshNotification() {
        if (notificationJob == null) return
        createNotificationChannel()
        try {
            startForeground(1, buildNotification())
        } catch (_: Exception) {
        }
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel(
                    "udp2cal_capture",
                    AppLocale.s(this, R.string.channel_name),
                    NotificationManager.IMPORTANCE_LOW,
                ).apply {
                    description = AppLocale.s(this@CaptureService, R.string.channel_desc)
                    setShowBadge(false)
                }
            )
        }
    }

    /** 通知栏实际展示的内容：只有这些字段变化才需要重建前台通知 */
    private data class NotificationKey(
        val connected: Boolean,
        val mode: CaptureMode,
        val source: AudioSource,
        val aec: AecState,
    )

    private fun notificationKey(status: CaptureStatus): NotificationKey = NotificationKey(
        connected = status.connected,
        mode = status.mode,
        source = status.audioSource,
        aec = status.aec,
    )

    private fun buildNotification(): Notification {
        val key = notificationKey(statusBus.value)
        val connected = AppLocale.s(
            this,
            if (key.connected) R.string.notification_connected else R.string.notification_waiting,
        )
        val mode = AppLocale.s(this, key.mode.labelRes)
        var source = AppLocale.s(this, key.source.sourceRes)
        if (key.aec != AecState.NONE) {
            source += " + " + AppLocale.s(this, key.aec.aecRes)
        }
        val text = AppLocale.s(this, R.string.notification_text, connected, mode, source)
        return NotificationCompat.Builder(this, "udp2cal_capture")
            .setContentTitle(AppLocale.s(this, R.string.app_name))
            .setContentText(text)
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
            }
            // stopCapture 之后仍可能有协程在收尾：采集协程可能在最后一个挂起点
            // 之后才创建反向接收协程，旧快照抓不到它。重取一次快照再 join：
            // 第一次超时的协程也会在这里再得到 1 s。
            val remaining = lifecycle.sessionJobs()
            val joined = withTimeoutOrNull(1000) {
                remaining.forEach { it.cancelAndJoin() }
                true
            } ?: false
            // join 期间收尾协程可能刚把超时的子协程登记进 lingering：再做一次
            // 最终快照确认没有任何仍活跃的协程，才允许 closeNative。
            // 必须用 isCompleted 判断：已取消但仍在执行的协程 isActive == false，
            // 用 none { it.isActive } 会把它们误判为已退出，随后 closeNative
            // 释放原生堆，而它们还可能在 JNI 里访问（use-after-free）。
            val after = lifecycle.sessionJobs()
            joined && after.all { it.isCompleted }
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
