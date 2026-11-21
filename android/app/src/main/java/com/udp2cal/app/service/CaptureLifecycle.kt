package com.udp2cal.app.service

import android.app.Service
import android.content.Context
import android.net.Network
import android.net.wifi.WifiManager
import android.os.PowerManager
import android.util.Log
import androidx.annotation.StringRes
import com.udp2cal.app.DiscoveryManager
import com.udp2cal.app.Prefs
import com.udp2cal.app.R
import com.udp2cal.app.Udp2CalProtocol
import com.udp2cal.app.UdpSender
import com.udp2cal.app.WifiNetworkBinder
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.ExecutorCoroutineDispatcher
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.asCoroutineDispatcher
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.util.concurrent.Executors
import kotlin.coroutines.cancellation.CancellationException

/**
 * 采集会话生命周期：安装/拆除链路、Wi-Fi 断连重连、重启排队。
 *
 * 这里只保留**跨链路共享**的状态（任务槽、UdpSender、重启参数、Wi-Fi 等待标记）；
 * 正向与反向各自的运行时状态分别封装在 [ForwardLink] / [ReverseLink] 中。
 *
 * 线程/协程模型（规范第七、八章）：
 * ```
 * 采集协程（AudioRecord 阻塞读 480 采样 → Opus 10 ms → RTP → 主 socket）
 * 控制协程（每秒 CONNECT，200 ms 轮询 ACK 新鲜度）
 * 反向接收协程（reverse socket 阻塞 receive → 校验 → 抖动缓冲）
 * 反向播放协程（AudioTrack 播放头拉取 → 解码/PLC → 阻塞写）
 * ```
 * 音频路径上**没有**应用层 PCM 队列、没有 `soTimeout` 驱动、没有 PLC 定时器。
 */
internal class CaptureLifecycle(
    private val service: CaptureService,
    private val scope: CoroutineScope,
    private val status: StatusBus,
) : ForwardLink.Host {
    companion object {
        private const val TAG = SERVICE_TAG
    }

    private data class CaptureParams(
        val sampleRateHz: Int,
        val bitrateKbps: Int,
        val targetIp: String,
        val targetPort: Int,
        val testTone: Boolean,
    )

    private val lifecycleLock = Any()
    @Volatile private var captureJob: Job? = null
    @Volatile private var controlJob: Job? = null
    @Volatile private var activeForward: ForwardLink? = null
    @Volatile private var udpSender: UdpSender? = null

    private var pendingRestart: CaptureParams? = null
    private var pendingRestartIsExplicit = false

    /** 当前会话的采集参数（重启时复用；码率以 Prefs 为准，见 requestLinkRestart） */
    @Volatile private var activeParams: CaptureParams? = null

    /** 主线程置位、IO 协程读取，必须 volatile */
    @Volatile private var hasNewCapture = false

    /**
     * Wi-Fi 丢失后等待恢复：置位期间不重建链路（当前无网络可绑），
     * 等 NetworkCallback 报来新的 Wi-Fi 再走 restart 通道重建 socket。
     */
    @Volatile private var wifiReconnectPending = false

    /** 已因参数变化投递重启，避免 100 ms 检查重复触发 */
    @Volatile private var restartPending = false

    /** Wi-Fi 低延迟锁：采集期间阻止网卡进入省电聚合 */
    private var wifiLock: WifiManager.WifiLock? = null

    // ── 音频热循环专用线程 ──
    // 协程跑在 Dispatchers.IO 上时，delay/yield 之后可能被换到另一个池线程：
    // setThreadPriority 设在“当时的线程”上不稳定，还会把 URGENT_AUDIO 留给
    // 后续复用该线程的任务。三个单线程执行器让热循环从首次 dispatch 到退出
    // 都钉在同一线程，优先级在线程体里设置一次即全程有效。
    // 执行器随服务创建，跨重启复用；停机时统一 close。
    private val micDispatcher = singleThreadDispatcher("udp2cal-mic")
    private val reverseRxDispatcher = singleThreadDispatcher("udp2cal-rx")
    private val reversePlayDispatcher = singleThreadDispatcher("udp2cal-play")

    private val reverse = ReverseLink(
        context = service.applicationContext,
        status = status,
        scope = scope,
        rxDispatcher = reverseRxDispatcher,
        playDispatcher = reversePlayDispatcher,
    )

    // ══════════════════════════════════════════════════════════
    // 会话启停
    // ══════════════════════════════════════════════════════════

    fun startCapture(
        sampleRateHz: Int,
        bitrateKbps: Int,
        targetIp: String,
        targetPort: Int,
        testTone: Boolean = false,
    ) {
        synchronized(lifecycleLock) {
            val params = CaptureParams(sampleRateHz, bitrateKbps, targetIp, targetPort, testTone)
            val old = captureJob
            if (old != null) {
                // 旧任务可能仍在拆除对象（已 cancel 但 finally 没跑完）：先登记
                // 重启，由它自己的 finally 在收尾**完全结束**后再安装新链路。
                // 若在旧 finally 之前直接 doStartCapture，旧任务会关掉新对象。
                pendingRestart = params
                pendingRestartIsExplicit = true
                restartPending = true
                old.cancel()
                return
            }
            pendingRestart = null
            pendingRestartIsExplicit = false
            doStartCapture(params)
        }
    }

    private fun doStartCapture(params: CaptureParams) {
        val powerManager = service.getSystemService(Context.POWER_SERVICE) as PowerManager
        val wakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "UDP2CAL:Capture")
        try {
            wakeLock.acquire()
        } catch (_: Exception) {
        }

        if (wifiLock == null) {
            try {
                val wm = service.applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
                wifiLock = wm?.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "UDP2CAL:Wifi")
                    ?.apply {
                        setReferenceCounted(false)
                        acquire()
                        Log.i(TAG, "WifiLock 已持有 (FULL_LOW_LATENCY)")
                    }
            } catch (e: Exception) {
                Log.w(TAG, "WifiLock 获取失败（继续运行）", e)
            }
        }

        hasNewCapture = true
        val deviceId = DiscoveryManager.getOrCreateDeviceId()
        activeParams = params
        restartPending = false
        wifiReconnectPending = false

        val forward = ForwardLink(
            context = service.applicationContext,
            status = status,
            dispatcher = micDispatcher,
            host = this,
        )
        activeForward = forward

        // ATOMIC：launch 后立刻 stopCapture 也不会在“未启动”状态被取消，
        // 否则协程体（含 finally 拆除）永远不跑，captureJob 会永久占位。
        captureJob = scope.launch(start = CoroutineStart.ATOMIC) {
            status.update {
                it.copy(isRunning = true, errorRes = 0, errorArgs = emptyList(), fatal = false)
            }
            try {
                // 被 stop 取消时尽早失败，finally 仍会执行完整拆除
                ensureActive()
                // ── 网络层 ──
                val sender = UdpSender(params.targetIp, params.targetPort)
                if (!sender.connect()) {
                    // 连接失败时 socket 可能已创建：必须显式关闭，否则句柄泄漏
                    sender.close()
                    fail(R.string.error_network_connect)
                    return@launch
                }
                udpSender = sender

                // ── 正向编码器（固定 48 kHz / 10 ms / 单声道）──
                val cfg = forward.prepare(params.testTone) ?: return@launch
                // 音源/路由/AEC 都用 prepare() 的同一快照，避免连接期间切
                // 音乐↔语音时 AudioRecord 与 voiceMode 来自两个不同时刻。
                val voiceMode = cfg.voiceMode()

                // ── 反向链路：解码器 + 接收协程（PC→Phone，立体声）──
                reverse.startSession(sender)
                status.update {
                    it.copy(
                        isRunning = true,
                        sampleRateHz = Udp2CalProtocol.OPUS_SAMPLE_RATE,
                        bitrateTargetKbps = cfg.bitrateKbps,
                        vbrMode = cfg.vbrMode(),
                        // 状态卡片的「带宽」一行读的就是这个字段；以前从没赋过值，
                        // 于是永远渲染成 "—"，看起来像"改了带宽没反应"
                        opusBandwidth = cfg.bandwidth,
                        mode = if (voiceMode) CaptureMode.VOICE else CaptureMode.FULLBAND,
                        connected = false,
                        deviceId = Udp2CalProtocol.deviceIdToString(deviceId),
                    )
                }

                // ── 控制协程 ──
                controlJob = launch {
                    ControlLoop(
                        sender = sender,
                        status = status,
                        reverse = reverse,
                        isWifiAwaitingReconnect = { wifiReconnectPending },
                    ).run()
                }

                // 采集热循环钉到专用线程（见 micDispatcher）
                forward.run(sender, cfg, params.testTone)
            } catch (_: CancellationException) {
            } catch (e: Exception) {
                // 内部异常文本不直接当 UI 文案：日志保留细节，界面显示可翻译的通用错误
                Log.e(TAG, "采集异常", e)
                status.update {
                    it.copy(
                        isRunning = false,
                        errorRes = R.string.error_unknown,
                        errorArgs = emptyList(),
                    )
                }
                hasNewCapture = false
            } finally {
                // 控制协程可能在 NonCancellable 里调用 jitter.reset()；必须等它
                // 真正退出再清引用，否则 onDestroy 拿不到句柄可 join，原生堆已
                // 释放而它还在调用（use-after-free）。
                val ctrl = controlJob
                controlJob = null
                withContext(NonCancellable) { ctrl?.cancelAndJoin() }
                reverse.stopAndJoin()
                forward.teardown()
                try { udpSender?.close() } catch (_: Exception) {}
                udpSender = null
                reverse.teardownSession()
                try { if (wakeLock.isHeld) wakeLock.release() } catch (_: Exception) {}

                // 硬件初始化硬故障时不得无限重启。
                val fatal = status.value.fatal
                if (fatal) Log.e(TAG, "硬件初始化硬故障，禁止自动重试")

                // 对象已全部拆除，此时才清空任务槽并安装下一条链路；两者与
                // startCapture 共用 lifecycleLock，避免旧 finally 覆盖新任务。
                synchronized(lifecycleLock) {
                    captureJob = null
                    activeForward = null
                    if (fatal && !pendingRestartIsExplicit) {
                        // 硬件硬故障不自动重试，但不能丢弃用户刚明确请求的新会话。
                        pendingRestart = null
                        restartPending = false
                    }
                    val restart = pendingRestart
                    pendingRestart = null
                    pendingRestartIsExplicit = false
                    if (restart != null) {
                        // 重启路径继续持有 Wi-Fi 锁，下一条链路直接复用。
                        doStartCapture(restart)
                    } else if (!hasNewCapture) {
                        // 会话真正结束（失败/读失败/停采集）：归还 Wi-Fi 低延迟锁。
                        // 失败自终止时 stopCapture 不会运行，而绑定中的 Service 也不会
                        // onDestroy，只能在这里释放，否则锁会一直留在网卡上耗电。
                        try { wifiLock?.release() } catch (_: Exception) {}
                        wifiLock = null
                        status.update { it.copy(isRunning = false) }
                        try { service.stopForeground(Service.STOP_FOREGROUND_REMOVE) } catch (_: Exception) {}
                        service.stopSelf()
                    }
                }
            }
        }
    }

    fun stopCapture() {
        val forwardToStop: ForwardLink?
        synchronized(lifecycleLock) {
            pendingRestart = null
            pendingRestartIsExplicit = false
            activeParams = null
            restartPending = false
            hasNewCapture = false
            wifiReconnectPending = false
            try { wifiLock?.release() } catch (_: Exception) {}
            wifiLock = null
            // 不在这里把 captureJob 置空：旧任务的 finally 拆除完对象后才清空。
            // 提前置空会让下一次 startCapture 与旧收尾并行，旧 finally 会关掉
            // 新任务的 audioRecord/encoder/udpSender/audioPlayer。
            captureJob?.cancel()
            controlJob?.cancel()
            // 保留 controlJob 引用到它自己的协程结束：onDestroy 需要 cancelAndJoin
            // 它，才能确认没有线程还停在 jitter.reset() 的原生调用里。
            // 只停本次要拆的实例；若随后 startCapture 拉起新链路，不能误停它
            forwardToStop = activeForward
        }
        // 阻塞在 AudioRecord.read 里的采集线程只有 stop() 能唤醒；否则
        // finally 的清理要等到下一次读返回，麦克风会一直开着。
        forwardToStop?.stopRecord()
        reverse.stop()
        status.update { it.copy(connected = false, reverseAudio = false) }
    }

    /** onDestroy 用：在 stopCapture 清空任务槽之前取一份待 join 的协程快照 */
    fun sessionJobs(): List<Job> = buildList {
        controlJob?.let(::add)
        captureJob?.let(::add)
        addAll(reverse.jobs())
    }

    /** 服务停机：热循环专用线程都已空闲后才可关闭，不留孤儿线程 */
    fun closeThreads() {
        runCatching { micDispatcher.close() }
        runCatching { reverseRxDispatcher.close() }
        runCatching { reversePlayDispatcher.close() }
    }

    /** 服务停机：归还反向链路的原生句柄（见 ReverseLink.closeNative） */
    fun closeNative() = reverse.closeNative()

    /** 高级设置：反向（PC→手机）目标水位，运行时立即生效 */
    fun setReverseWaterTargetMs(ms: Int) = reverse.setWaterTarget(ms)

    // ══════════════════════════════════════════════════════════
    // Wi-Fi 网络回调（由 CaptureService 转发）
    // ══════════════════════════════════════════════════════════

    /**
     * Wi-Fi 恢复：只有"断网等待中"才重建。回调线程不是主线程，
     * restart 自带生命周期锁，可直接调用。
     */
    fun onWifiAvailable(network: Network) {
        if (!wifiReconnectPending) return
        wifiReconnectPending = false
        Log.i(TAG, "Wi-Fi 已恢复（$network），重建采集链路")
        requestLinkRestart()
    }

    /**
     * 绑定的 Wi-Fi 丢失：立即关闭 socket，既不空转发送，也杜绝等待期间
     * 未显式绑定的包被系统默认路由（蜂窝）接走；等 onAvailable 再重建。
     */
    fun onWifiLost(network: Network) {
        if (!hasNewCapture) return
        val sender = udpSender
        // 已建会话时只关心它实际绑定的网络；sender 为 null（connect 进行中）
        // 时无法比对，按断网处理，避免错过重连时机。
        if (sender != null && sender.boundNetwork != network) return
        // onAvailable 可能先于 onLost 到达（快速重连/网络换代）：若已经有
        // 新 Wi-Fi 在线就无需等待，直接重建。先置位再拆 socket，避免控制环
        // 在这段窗口里用通用“已断开”覆盖 Wi-Fi 专属提示。
        val hasWifiNow = WifiNetworkBinder.currentNetwork() != null
        wifiReconnectPending = !hasWifiNow
        Log.w(TAG, if (hasWifiNow) "Wi-Fi 网络已切换，重建采集链路" else "Wi-Fi 网络丢失，关闭 UDP 会话并等待恢复")
        try { sender?.close() } catch (_: Exception) {}
        status.update {
            it.copy(
                connected = false,
                reverseAudio = false,
                errorRes = if (hasWifiNow) R.string.status_network_switched else R.string.status_wifi_lost,
                errorArgs = emptyList(),
            )
        }
        if (hasWifiNow) requestLinkRestart()
    }

    // ══════════════════════════════════════════════════════════
    // ForwardLink.Host
    // ══════════════════════════════════════════════════════════

    override fun fail(@StringRes id: Int, vararg args: Any) {
        status.update {
            it.copy(
                isRunning = false,
                errorRes = id,
                errorArgs = args.toList(),
            )
        }
        hasNewCapture = false
        service.stopSelf()
    }

    override fun onCaptureStalled() {
        status.update {
            it.copy(errorRes = R.string.error_mic_read, errorArgs = emptyList())
        }
        hasNewCapture = false
    }

    /**
     * 因参数变化重建采集链路；返回 false 表示已有重启在途（调用方无需重复记日志）。
     *
     * 复用 `startCapture` 已有的 restart 通道：置 `pendingRestart` 后取消当前任务，
     * `doStartCapture` 的 finally 会以**同一套参数**重新起一条采集链。
     * 码率取当前 Prefs（用户可能在触发重建的同一时刻也改了码率）。
     */
    override fun requestLinkRestart(): Boolean = synchronized(lifecycleLock) {
        if (restartPending) return@synchronized false
        restartPending = true
        // 与 stopCapture 串行，防止 stop 清空后又被旧采集线程排入重启。
        if (pendingRestart == null && hasNewCapture) {
            val p = activeParams
            if (p != null) {
                pendingRestart = p.copy(
                    bitrateKbps = Prefs.opusBitrateKbps,
                    testTone = Prefs.testToneMode,
                )
                pendingRestartIsExplicit = false
                captureJob?.cancel()
            }
        }
        true
    }

    override fun ensurePlayer(voiceMode: Boolean, sessionId: Int) {
        reverse.ensurePlayer(voiceMode, sessionId)
    }
}

/** 单线程执行器 → 协程调度器（热循环专用） */
private fun singleThreadDispatcher(name: String): ExecutorCoroutineDispatcher =
    Executors.newSingleThreadExecutor { r -> Thread(r, name) }.asCoroutineDispatcher()
