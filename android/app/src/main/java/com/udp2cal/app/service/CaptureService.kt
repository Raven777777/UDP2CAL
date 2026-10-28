package com.udp2cal.app.service

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.media.audiofx.AcousticEchoCanceler
import android.net.wifi.WifiManager
import android.os.Binder
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.os.SystemClock
import android.util.Log
import androidx.core.app.NotificationCompat
import com.udp2cal.app.AudioPlayer
import com.udp2cal.app.DiscoveryManager
import com.udp2cal.app.Prefs
import com.udp2cal.app.StreamResampler
import com.udp2cal.app.Udp2CalProtocol
import com.udp2cal.app.UdpSender
import com.udp2cal.app.native.OpusDecoder as ReverseOpusDecoder
import com.udp2cal.app.native.OpusEncoder
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.yield
import androidx.core.content.ContextCompat
import java.security.SecureRandom
import kotlin.coroutines.cancellation.CancellationException
import kotlin.coroutines.coroutineContext

/**
 * 采集服务（v3）
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
class CaptureService : Service() {
    companion object {
        private const val TAG = "CaptureService"

        /** 正向 Opus 帧采样数（10 ms @ 48 kHz，单声道） */
        private const val FRAME = Udp2CalProtocol.OPUS_FRAME_SAMPLES

        /** ACK 轮询周期：只用于检测连接状态与发送保活 */
        private const val TICK_MS = 200L

        /** RTP 连续空闲超过该值：下个包到达时重置抖动缓冲，避免本地静音把播放头推过后被判 late */
        private const val REVERSE_IDLE_RESET_MS = 1_000L
    }

    private val binder = LocalBinder()
    private val serviceScope = CoroutineScope(Dispatchers.IO + SupervisorJob())

    @Volatile private var captureJob: Job? = null
    private var controlJob: Job? = null
    private var reverseRxJob: Job? = null
    private var reversePlayJob: Job? = null
    private var lastReverseStartAt = 0L
    /** 最近一次通过校验的反向 RTP 到达时刻（monotonic ms）；接收/播放协程共享 */
    @Volatile private var lastReversePacketAt = 0L
    private var notificationJob: Job? = null

    /** 主线程置位、IO 协程读取，必须 volatile */
    @Volatile private var hasNewCapture = false

    private val _status = MutableStateFlow(CaptureStatus())
    val status: StateFlow<CaptureStatus> = _status.asStateFlow()

    private var audioRecord: AudioRecord? = null
    private var encoder: OpusEncoder? = null
    // 跨协程（不同调度线程）访问，必须 volatile 保证可见性
    @Volatile private var udpSender: UdpSender? = null
    @Volatile private var reverseDecoder: ReverseOpusDecoder? = null
    @Volatile private var audioPlayer: AudioPlayer? = null
    /** Wi-Fi 低延迟锁：采集期间阻止网卡进入省电聚合 */
    private var wifiLock: WifiManager.WifiLock? = null
    private var aecInstance: AcousticEchoCanceler? = null

    // ─── 反向链路共享状态：接收协程插入，播放协程拉取 ───
    private val linkLock = java.util.concurrent.locks.ReentrantLock()
    private val linkSignal = linkLock.newCondition()
    private val jitter = Udp2CalProtocol.JitterBuffer()
    private val drift = Udp2CalProtocol.DriftController()
    private val deviceRate = Udp2CalProtocol.DeviceRateEstimator()
    @Volatile private var activeReverseSsrc = 0

    private data class CaptureParams(
        val sampleRateHz: Int,
        val bitrateKbps: Int,
        val targetIp: String,
        val targetPort: Int,
        val testTone: Boolean,
    )

    private var pendingRestart: CaptureParams? = null
    /** 当前会话的采集参数（重启时复用；码率以 Prefs 为准，见 restartCapture） */
    @Volatile private var activeParams: CaptureParams? = null

    // ─── 运行期参数热更新 ───
    //
    // 设置页改动会即时写入 Prefs，但编码器只在创建时读过一次参数，导致
    // "改了没反应"。发送循环每 100 ms 比对一次快照：
    //   * 编码器参数（码率/复杂度/带宽/DTX/VBR/FEC/丢包率/信号提示）→ OPUS_SET_*，下一帧生效；
    //   * 采集链路参数（语音↔音乐、测试音）→ AudioRecord 音源/AEC/AudioTrack 模式
    //     都是创建时定型的，只能重建链路。
    /** 已生效的编码器参数快照；null 表示尚未与编码器对齐 */
    @Volatile private var appliedCfg: EncoderCfg? = null
    @Volatile private var appliedVoiceMode: Boolean? = null
    @Volatile private var appliedTestTone: Boolean = false
    /** 已因参数变化投递重启，避免 100 ms 检查重复触发 */
    @Volatile private var restartPending: Boolean = false

    /**
     * 编码器参数快照。
     *
     * 推导规则必须与 [`OpusEncoder`] 构造函数**逐字一致**：
     * DTX 开启 → 强制 VBR；VBR 关闭 → 约束清零。否则热更新后的状态
     * 会与"从头创建"不一致，表现为"改了别的参数，VBR 又变回去了"。
     */
    private data class EncoderCfg(
        val bitrateKbps: Int,
        val complexity: Int,
        val signal: Int,
        val bandwidth: Int,
        val dtx: Int,
        val vbr: Int,
        val vbrConstraint: Int,
        val fec: Int,
        val packetLoss: Int,
    ) {
        companion object {
            fun fromPrefs(): EncoderCfg {
                val dtx = Prefs.opusDtx.coerceIn(0, 1)
                val vbr = if (dtx == 1) 1 else Prefs.opusVbr.coerceIn(0, 1)
                return EncoderCfg(
                    bitrateKbps = Udp2CalProtocol.clampBitrateKbps(Prefs.opusBitrateKbps),
                    complexity = Prefs.opusComplexity,
                    signal = Prefs.opusSignal,
                    bandwidth = Prefs.opusBandwidth,
                    dtx = dtx,
                    vbr = vbr,
                    vbrConstraint = if (vbr == 1) Prefs.opusVbrConstraint.coerceIn(0, 1) else 0,
                    fec = Prefs.opusFec,
                    packetLoss = Prefs.opusPacketLoss,
                )
            }
        }

        /** 状态卡展示用：当前码率控制模式的短标签 */
        fun vbrLabel(): String = when {
            vbr == 0 -> "CBR"
            vbrConstraint == 1 -> "VBR·约束"
            else -> "VBR"
        }
    }

    /** 音源描述枚举 */
    object AudioSourceLabel {
        const val MIC_RAW = "MIC直出"
        const val SYSTEM_NS = "系统硬件降噪"
        const val SYSTEM_NS_FALLBACK = "系统硬件降噪(已回退MIC)"
        const val AEC_ENABLED = " + AEC已启用"
        const val AEC_UNAVAILABLE = " + AEC不可用"
    }

    data class CaptureStatus(
        val isRunning: Boolean = false,
        val bitrateKbps: Float = 0f,
        val sampleRateHz: Int = 0,
        val bitrateTargetKbps: Int = 0,
        val vbrMode: String = "",
        val audioSource: String = "",
        val opusMode: String = "",
        val opusBandwidth: Int = 0,
        val errorMsg: String = "",
        val connected: Boolean = false,
        val deviceId: String = "",
        val reverseAudio: Boolean = false,
        val reverseBitrateKbps: Float = 0f,
        val reverseBw: String = "",
        /** 反向抖动缓冲摘要（迟到/溢出/PLC/重同步），每秒刷新，空串表示无数据 */
        val reverseJitter: String = "",
    )

    inner class LocalBinder : Binder() {
        fun getService(): CaptureService = this@CaptureService
    }

    override fun onBind(intent: Intent?): IBinder = binder

    override fun onCreate() {
        super.onCreate()
    }

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
                _status.collect {
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
    ) {
        if (captureJob?.isActive == true) {
            pendingRestart = CaptureParams(sampleRateHz, bitrateKbps, targetIp, targetPort, testTone)
            captureJob?.cancel()
            return
        }
        pendingRestart = null
        doStartCapture(sampleRateHz, bitrateKbps, targetIp, targetPort, testTone)
    }

    /** 语音模式：VOICE_COMMUNICATION + 硬件降噪 + AEC */
    private fun isVoiceMode(): Boolean = Prefs.opusSignal == 3001

    private fun doStartCapture(
        sampleRateHz: Int,
        bitrateKbps: Int,
        targetIp: String,
        targetPort: Int,
        testTone: Boolean,
    ) {
        val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
        val wakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "UDP2CAL:Capture")
        try {
            wakeLock.acquire()
        } catch (_: Exception) {
        }

        if (wifiLock == null) {
            try {
                val wm = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
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
        val voiceMode = isVoiceMode()
        val deviceId = DiscoveryManager.getOrCreateDeviceId()
        activeParams = CaptureParams(sampleRateHz, bitrateKbps, targetIp, targetPort, testTone)
        restartPending = false

        captureJob = serviceScope.launch {
            _status.value = _status.value.copy(isRunning = true, errorMsg = "")
            try {
                // ── 网络层 ──
                val sender = UdpSender(targetIp, targetPort)
                if (!sender.connect()) {
                    // 连接失败时 socket 可能已创建：必须显式关闭，否则句柄泄漏
                    sender.close()
                    fail("网络连接失败")
                    return@launch
                }
                udpSender = sender
                activeReverseSsrc = 0

                // ── 正向编码器（固定 48 kHz / 10 ms / 单声道）──
                //
                // 参数一律取**当前 Prefs 的快照**，不取入参：重建链路可能就发生在
                // 用户刚改完设置之后（例如语音↔音乐），用旧入参会把新设置丢掉。
                // 快照同时作为 appliedCfg 的初值，构造与"已生效"状态原子一致。
                val cfg0 = EncoderCfg.fromPrefs()
                val enc = OpusEncoder(
                    sampleRateHz = Udp2CalProtocol.OPUS_SAMPLE_RATE,
                    bitrateKbps = cfg0.bitrateKbps,
                    complexity = cfg0.complexity,
                    signalType = cfg0.signal,
                    bandwidth = cfg0.bandwidth,
                    dtx = cfg0.dtx,
                    vbrRaw = cfg0.vbr,
                    fec = cfg0.fec,
                    packetLoss = cfg0.packetLoss,
                    vbrConstraintRaw = cfg0.vbrConstraint,
                )
                encoder = enc
                appliedCfg = cfg0
                appliedVoiceMode = voiceMode
                appliedTestTone = testTone
                if (!enc.start()) {
                    fail("编码器初始化失败")
                    return@launch
                }
                if (enc.frameSize != FRAME) {
                    fail("编码器帧大小无效（${enc.frameSize}）")
                    return@launch
                }

                // ── 反向链路：解码器 + 播放器（PC→Phone，立体声）──
                reverseDecoder = ReverseOpusDecoder(Udp2CalProtocol.OPUS_SAMPLE_RATE, channels = 2).also {
                    if (!it.start()) Log.w(TAG, "反向解码器初始化失败，继续运行")
                }
                launchReverseReceiver()
                _status.value = _status.value.copy(
                    isRunning = true,
                    sampleRateHz = Udp2CalProtocol.OPUS_SAMPLE_RATE,
                    bitrateTargetKbps = cfg0.bitrateKbps,
                    vbrMode = cfg0.vbrLabel(),
                    // 状态卡片的「带宽」一行读的就是这个字段；以前从没赋过值，
                    // 于是永远渲染成 "—"，看起来像"改了带宽没反应"
                    opusBandwidth = cfg0.bandwidth,
                    opusMode = if (voiceMode) "语音模式" else "全频模式",
                    connected = false,
                    deviceId = Udp2CalProtocol.deviceIdToString(deviceId),
                )

                // ── 控制协程 ──
                controlJob = launch { controlLoop() }

                if (testTone) {
                    runTestToneLoop(enc, sender, voiceMode)
                } else {
                    runMicCaptureLoop(enc, sender, voiceMode)
                }
            } catch (_: CancellationException) {
            } catch (e: Exception) {
                Log.e(TAG, "采集异常", e)
                _status.value = _status.value.copy(isRunning = false, errorMsg = e.message ?: "未知错误")
                hasNewCapture = false
            } finally {
                controlJob?.cancel()
                controlJob = null
                stopReverseAudioAndJoin()
                try { audioRecord?.stop() } catch (_: Exception) {}
                try { audioRecord?.release() } catch (_: Exception) {}
                audioRecord = null
                encoder?.stop()
                encoder = null
                try { aecInstance?.release() } catch (_: Exception) {}
                aecInstance = null
                try { udpSender?.close() } catch (_: Exception) {}
                udpSender = null
                reverseRxJob?.cancel()
                reverseRxJob = null
                reverseDecoder?.stop()
                reverseDecoder = null
                audioPlayer?.stop()
                audioPlayer = null
                activeReverseSsrc = 0
                try { if (wakeLock.isHeld) wakeLock.release() } catch (_: Exception) {}

                // 硬件初始化硬故障时不得无限重启
                val fatal = _status.value.errorMsg.let {
                    it.contains("麦克风初始化失败") || it.contains("麦克风不支持") ||
                        it.contains("编码器初始化失败") || it.contains("编码器帧大小无效")
                }
                if (fatal) {
                    Log.e(TAG, "硬件初始化硬故障，清空挂起任务")
                    pendingRestart = null
                }

                val restart = pendingRestart
                pendingRestart = null
                if (restart != null) {
                    serviceScope.launch {
                        doStartCapture(
                            restart.sampleRateHz, restart.bitrateKbps,
                            restart.targetIp, restart.targetPort, restart.testTone
                        )
                    }
                } else if (!hasNewCapture) {
                    _status.value = _status.value.copy(isRunning = false)
                    try { stopForeground(STOP_FOREGROUND_REMOVE) } catch (_: Exception) {}
                    stopSelf()
                }
            }
        }
    }

    /** 反向播放器按需创建（AEC 需要麦克风的 sessionId） */
    private fun ensurePlayer(voiceMode: Boolean, sessionId: Int) {
        if (audioPlayer != null) return
        audioPlayer = AudioPlayer(this@CaptureService).also { player ->
            if (!player.start(sessionId, voiceMode)) {
                Log.w(TAG, "反向播放器初始化失败，继续运行")
            }
        }
    }

    private suspend fun fail(message: String) {
        _status.value = _status.value.copy(isRunning = false, errorMsg = message)
        hasNewCapture = false
        stopSelf()
    }

    // ══════════════════════════════════════════════════════════
    // 控制面：CONNECT 保活 + ACK 状态 + 反向开关
    // ══════════════════════════════════════════════════════════

    private suspend fun controlLoop() {
        var connected = false
        var sinceConnect = 0L
        while (coroutineContext.isActive) {
            val sender = udpSender ?: break

            if (sinceConnect <= 0) {
                val payload = buildConnectPayload(sender)
                val packet = Udp2CalProtocol.buildControl(
                    Udp2CalProtocol.TYPE_CONNECT,
                    DiscoveryManager.getOrCreateDeviceId(),
                    payload
                )
                sender.send(packet, 0, packet.size)
                sinceConnect = 1000
            }
            sinceConnect -= TICK_MS

            val fresh = sender.isAckFresh()
            if (fresh != connected) {
                connected = fresh
                if (fresh) {
                    _status.value = _status.value.copy(connected = true, errorMsg = "")
                } else {
                    _status.value = _status.value.copy(
                        connected = false, reverseAudio = false, errorMsg = "已断开（等待重连）"
                    )
                    stopReverseAudioAndJoin()
                    activeReverseSsrc = 0
                    // ACK 超时说明旧会话已不可用：换一个正向 SSRC，让对端在
                    // 下一次 CONNECT 时走显式新会话路径（清抖动缓冲/重建反向流）。
                    sender.regenerateForwardSsrc()
                }
            }

            if (connected) {
                val ssrc = sender.reverseSsrc
                if (ssrc != activeReverseSsrc) {
                    activeReverseSsrc = ssrc
                    if (ssrc == 0) {
                        // ACK 明确表示反向未启用：必须停止播放并清空缓冲
                        stopReverseAudioAndJoin()
                    } else {
                        startReverseAudio()
                    }
                } else if (
                    ssrc != 0 &&
                    reversePlayJob?.isActive != true &&
                    SystemClock.elapsedRealtime() - lastReverseStartAt > 2000
                ) {
                    // 播放协程可能因 AudioTrack/解码器异常退出，而 SSRC 没变；
                    // 不能只靠 activeReverseSsrc 变化触发重启，否则会永久静音。
                    Log.w(TAG, "反向播放协程已退出，自动重启")
                    startReverseAudio()
                }
            }

            delay(TICK_MS)
        }
    }

    /** CONNECT 负载固定 15 字节；Opus 参数只描述**反向**（Windows 编码器） */
    private fun buildConnectPayload(sender: UdpSender): ByteArray {
        // 自动档已移除：始终发送具体档位（0 仅作跨版本兼容保留在协议里）
        val dtx = Prefs.opusDtx == 1
        val vbr = Prefs.opusVbr == 1 || dtx
        val vbrConstraint = vbr && Prefs.opusVbrConstraint == 1
        var flags = 0
        if (vbr) flags = flags or Udp2CalProtocol.OPUS_FLAG_VBR
        if (dtx) flags = flags or Udp2CalProtocol.OPUS_FLAG_DTX
        if (vbrConstraint) flags = flags or Udp2CalProtocol.OPUS_FLAG_VBR_CONSTRAINT
        if (Prefs.opusFec > 0) flags = flags or Udp2CalProtocol.OPUS_FLAG_FEC

        return Udp2CalProtocol.ConnectPayload(
            reversePort = sender.reversePort,
            forwardSsrc = sender.forwardSsrc,
            complexity = Prefs.opusComplexity,
            signal = mapSignal(Prefs.opusSignal),
            bandwidth = mapBandwidth(Prefs.opusBandwidth),
            opusFlags = flags,
            bitrateKbps = Prefs.opusBitrateKbps.coerceAtMost(Udp2CalProtocol.BITRATE_MAX_KBPS),
            packetLoss = Prefs.opusPacketLoss,
        ).encode()
    }

    private fun mapSignal(sig: Int): Int = when (sig) {
        3001 -> 1 // OPUS_SIGNAL_VOICE
        3002 -> 2 // OPUS_SIGNAL_MUSIC
        else -> 0
    }

    private fun mapBandwidth(bw: Int): Int = when (bw) {
        1101 -> 0
        1102 -> 1
        1103 -> 2
        1104 -> 3
        else -> 4
    }

    // ══════════════════════════════════════════════════════════
    // 正向发送：采集 → 编码 → RTP → 主 socket（无应用层队列）
    // ══════════════════════════════════════════════════════════

    private suspend fun runMicCaptureLoop(enc: OpusEncoder, sender: UdpSender, voiceMode: Boolean) {
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            fail("麦克风初始化失败（未授权）")
            return
        }
        // 采集/编码/发送跑在同一条线程上：提到音频优先级，避免被后台任务抢占
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO)
        val minBufSize = AudioRecord.getMinBufferSize(
            Udp2CalProtocol.OPUS_SAMPLE_RATE,
            AudioFormat.CHANNEL_IN_MONO,
            AudioFormat.ENCODING_PCM_16BIT
        )
        if (minBufSize == AudioRecord.ERROR || minBufSize == AudioRecord.ERROR_BAD_VALUE) {
            fail("麦克风不支持 48 kHz 单声道采集")
            return
        }
        val bufferSize = minBufSize.coerceAtLeast(FRAME * 2 * 4)

        var sourceLabel: String
        if (voiceMode) {
            try {
                audioRecord = AudioRecord(
                    MediaRecorder.AudioSource.VOICE_COMMUNICATION,
                    Udp2CalProtocol.OPUS_SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO,
                    AudioFormat.ENCODING_PCM_16BIT, bufferSize
                )
            } catch (_: Exception) {
            }
            if (audioRecord?.state == AudioRecord.STATE_INITIALIZED) {
                sourceLabel = AudioSourceLabel.SYSTEM_NS
            } else {
                audioRecord?.release()
                audioRecord = AudioRecord(
                    MediaRecorder.AudioSource.MIC,
                    Udp2CalProtocol.OPUS_SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO,
                    AudioFormat.ENCODING_PCM_16BIT, bufferSize
                )
                sourceLabel = AudioSourceLabel.SYSTEM_NS_FALLBACK
            }
        } else {
            audioRecord = AudioRecord(
                MediaRecorder.AudioSource.MIC,
                Udp2CalProtocol.OPUS_SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO,
                AudioFormat.ENCODING_PCM_16BIT, bufferSize
            )
            sourceLabel = AudioSourceLabel.MIC_RAW
        }

        val record = audioRecord
        if (record == null || record.state != AudioRecord.STATE_INITIALIZED) {
            _status.value = _status.value.copy(audioSource = AudioSourceLabel.MIC_RAW)
            fail("麦克风初始化失败")
            return
        }

        if (voiceMode && AcousticEchoCanceler.isAvailable()) {
            try {
                val aec = AcousticEchoCanceler.create(record.audioSessionId)
                if (aec != null) {
                    aec.enabled = true
                    aecInstance = aec
                    sourceLabel += AudioSourceLabel.AEC_ENABLED
                } else {
                    sourceLabel += AudioSourceLabel.AEC_UNAVAILABLE
                }
            } catch (e: Exception) {
                sourceLabel += AudioSourceLabel.AEC_UNAVAILABLE
                Log.w(TAG, "AEC create 失败", e)
            }
        }

        // 反向播放器必须在 AudioRecord 之后创建：复用其 sessionId 才能让
        // AudioTrack 与麦克风同属一条 AEC 管线
        ensurePlayer(voiceMode, record.audioSessionId)

        record.startRecording()
        _status.value = _status.value.copy(audioSource = sourceLabel, isRunning = true)

        try {
            sendFrames(enc, sender) { buf, off, len ->
                record.read(buf, off, len, AudioRecord.READ_BLOCKING)
            }
        } finally {
            try { record.stop() } catch (_: Exception) {}
        }
    }

    private suspend fun runTestToneLoop(enc: OpusEncoder, sender: UdpSender, voiceMode: Boolean) {
        ensurePlayer(voiceMode, 0)
        _status.value = _status.value.copy(audioSource = "1kHz 测试音", isRunning = true)
        val omega = 2.0 * Math.PI * 1000.0 / Udp2CalProtocol.OPUS_SAMPLE_RATE
        var phase = 0.0
        // 节拍由 sendFrames 的 1 帧整形统一负责，这里只负责把样本填满
        sendFrames(enc, sender) { buf, off, len ->
            for (i in 0 until len) {
                buf[off + i] = (Math.sin(phase) * 16384.0).toInt().toShort()
                phase += omega
                if (phase > 2.0 * Math.PI) phase -= 2.0 * Math.PI
            }
            len
        }
    }

    /**
     * 运行期参数对齐（发送循环每 100 ms 调一次）。
     *
     * 分两类，因为"能不能热改"取决于参数落在哪一层：
     *
     * 1. **编码器参数** —— 码率 / 复杂度 / 带宽 / DTX / VBR / VBR 约束 / FEC / 丢包率 /
     *    信号提示：直接 `OPUS_SET_*`，下一帧生效，不断流、不重连。
     * 2. **采集链路参数** —— 语音↔音乐（决定 AudioRecord 音源是 VOICE_COMMUNICATION
     *    还是 MIC、是否挂 AEC、AudioTrack 是否走语音通路）、测试音开关：
     *    这些都在对象创建时定型，Opus 的 signal 提示改了也没用（音源不变）。
     *    只能重建链路——走既有的 restart 通道，代价是约 1 s 的重连。
     *
     * 第二类先判：重建期间不去打 ctl（新编码器还没建好）。
     */
    private fun applyRuntimeConfig(enc: OpusEncoder) {
        val voiceMode = isVoiceMode()
        val testTone = Prefs.testToneMode
        if (voiceMode != appliedVoiceMode || testTone != appliedTestTone) {
            if (!restartPending) {
                Log.i(
                    TAG,
                    "采集链路参数变化（语音模式 $appliedVoiceMode→$voiceMode，测试音 $appliedTestTone→$testTone），重建采集链路"
                )
                restartCapture()
            }
            return
        }

        val want = EncoderCfg.fromPrefs()
        if (want == appliedCfg) return
        val ok = enc.update(
            complexity = want.complexity,
            signalType = want.signal,
            bandwidth = want.bandwidth,
            dtx = want.dtx,
            vbr = want.vbr,
            bitrateKbps = want.bitrateKbps,
            fec = want.fec,
            packetLoss = want.packetLoss,
            vbrConstraint = want.vbrConstraint,
        )
        if (!ok) return // 编码器已销毁（退出中），下一轮自然不再调用
        val old = appliedCfg
        appliedCfg = want
        Log.i(
            TAG,
            "编码参数热更新: ${old?.bitrateKbps}→${want.bitrateKbps}k " +
                "cplx=${want.complexity} bw=${want.bandwidth} dtx=${want.dtx} " +
                "vbr=${want.vbr} vbrc=${want.vbrConstraint} fec=${want.fec} pl=${want.packetLoss}"
        )
        _status.value = _status.value.copy(
            bitrateTargetKbps = want.bitrateKbps,
            vbrMode = want.vbrLabel(),
            opusBandwidth = want.bandwidth,
        )
    }

    /**
     * 因参数变化重建采集链路。
     *
     * 复用 `startCapture` 已有的 restart 通道：置 `pendingRestart` 后取消当前任务，
     * `doStartCapture` 的 finally 会以**同一套参数**重新起一条采集链。
     * 码率取当前 Prefs（用户可能在触发重建的同一时刻也改了码率）。
     */
    private fun restartCapture() {
        // 先置位：即使 activeParams 已失效（正在退出），也不要每 100 ms 重复打日志
        restartPending = true
        val p = activeParams ?: return
        pendingRestart = p.copy(
            bitrateKbps = Prefs.opusBitrateKbps,
            testTone = Prefs.testToneMode,
        )
        captureJob?.cancel()
    }

    /**
     * 统一的 10 ms 帧发送循环。
     *
     * 规则（规范 4.3/4.4/8.1）：
     *   * 每帧时间戳 +480，**无条件推进**（DTX/断连也保留媒体时间）；
     *   * 序列号只属于真正发出的包：编码失败/发送失败/DTX/无新鲜 ACK 都不 +1；
     *   * 未连接（无新鲜 ACK）时不编码不发送，时间戳照常推进。
     *
     * **1 帧节拍整形**：AudioRecord 的阻塞读天然成簇返回（常见 20 ms 一次），
     * 直接发就是"两包挨着、然后静默 20 ms"，接收端水位呈锯齿。这里把发送时刻
     * 钉在 10 ms 网格上——不是攒队列，最多只压 1 帧；落后超过 4 帧就重置节拍，
     * 绝不追赶式突发（突发会让接收端水位瞬间抬高，正是要消灭的东西）。
     */
    private suspend fun sendFrames(
        enc: OpusEncoder,
        sender: UdpSender,
        readInto: suspend (ShortArray, Int, Int) -> Int,
    ) {
        val pcm = ShortArray(FRAME)
        val packet = ByteArray(Udp2CalProtocol.RTP_MAX_PACKET)
        val rnd = SecureRandom()
        var seq = rnd.nextInt() and 0xFFFF
        var timestamp = rnd.nextInt()
        var first = true
        var lastSentMono = 0L
        var byteCount = 0L
        var lastReport = System.currentTimeMillis()
        /** 已累积但还不足一帧的样本数 */
        var filled = 0
        /** 节拍网格上的下一个发送时刻（nanoTime） */
        val frameNs = FRAME * 1_000_000_000L / Udp2CalProtocol.OPUS_SAMPLE_RATE
        var nextSendAt = System.nanoTime()
        var lastCfgCheck = 0L
        // Plan §6：发送端隐藏积压打点。read/encode/send 分段计时 + 装配缓冲峰值，
        // 30 s 打一行，用来判断“接收端看着均匀，发送端却越排越多”。
        var statsAt = System.nanoTime()
        var readNs = 0L
        var encodeNs = 0L
        var sendNs = 0L
        var statFrames = 0L
        var maxFilled = 0

        while (coroutineContext.isActive) {
            // AudioRecord.read 允许返回不足请求量的样本（缓冲区尚未填满）。
            // 必须累积到整 480 采样再编码：补零发送等于丢音频，且会让帧率翻倍
            // （实测 190 包/秒 → 接收端缓冲被灌满，41% 的包被判超窗丢弃）。
            val readStart = System.nanoTime()
            val n = readInto(pcm, filled, FRAME - filled)
            readNs += System.nanoTime() - readStart
            if (n < 0) {
                // 设备失效：不空转，交给外层 finally 收尾
                _status.value = _status.value.copy(errorMsg = "麦克风读取失败，请重新开始采集")
                hasNewCapture = false
                return
            }
            filled += n
            if (filled > maxFilled) maxFilled = filled  // 仅未满一帧的装配余量，不是应用层队列
            if (filled < FRAME) continue
            filled = 0

            val now = System.currentTimeMillis()
            // 每 100 ms 对齐一次 Prefs：设置页的改动在下一帧就生效
            if (now - lastCfgCheck >= 100) {
                lastCfgCheck = now
                applyRuntimeConfig(enc)
            }
            val nowMono = SystemClock.elapsedRealtime()
            if (sender.isAckFresh(nowMono)) {
                val encStart = System.nanoTime()
                val n = enc.encodeTo(pcm, packet, Udp2CalProtocol.RTP_HEADER_SIZE)
                encodeNs += System.nanoTime() - encStart
                statFrames++
                if (n > 0) {
                    val marker =
                        first || (nowMono - lastSentMono > Udp2CalProtocol.RESYNC_GAP_MS.toLong())
                    Udp2CalProtocol.writeRtpHeader(
                        packet, 0, marker, seq, timestamp, sender.forwardSsrc
                    )
                    val sendStart = System.nanoTime()
                    val sent = sender.send(packet, 0, Udp2CalProtocol.RTP_HEADER_SIZE + n)
                    sendNs += System.nanoTime() - sendStart
                    if (sent) {
                        byteCount += n
                        lastSentMono = nowMono
                        first = false
                        // 序号只属于真正发出的包；DTX/断连不占号（RFC 3550）
                        seq = (seq + 1) and 0xFFFF
                    }
                }
            }
            // 时间戳无条件按媒体时间推进（DTX/断连期间也走），恢复后时间轴自然留缺口
            timestamp += FRAME

            if (now - lastReport >= 1000) {
                val elapsed = (now - lastReport).coerceAtLeast(1)
                _status.value = _status.value.copy(bitrateKbps = byteCount * 8f / elapsed)
                byteCount = 0
                lastReport = now
            }
            val statsElapsed = System.nanoTime() - statsAt
            if (statsElapsed >= 30_000_000_000L) {
                val ms = statsElapsed / 1_000_000.0
                Log.i(
                    TAG,
                    "tx30s frames=$statFrames read=${(readNs / 1e6 / ms * 1000).toInt()}us" +
                        " encode=${(encodeNs / 1e6 / ms * 1000).toInt()}us" +
                        " send=${(sendNs / 1e6 / ms * 1000).toInt()}us" +
                        " assemble_peak=${maxFilled * 1000 / Udp2CalProtocol.OPUS_SAMPLE_RATE}ms"
                )
                statsAt = System.nanoTime()
                readNs = 0
                encodeNs = 0
                sendNs = 0
                statFrames = 0
                maxFilled = 0
            }

            // ── 1 帧节拍整形 ──
            nextSendAt += frameNs
            val waitNs = nextSendAt - System.nanoTime()
            when {
                waitNs >= 1_000_000L -> delay(waitNs / 1_000_000L)
                waitNs > 0L -> yield() // 不足 1 ms：让出执行权，不忙等
                // 落后超过 4 帧（调度抖动/断连恢复）：重置节拍，
                // 绝不用连续突发把落后的时间补回来
                waitNs < -4 * frameNs -> nextSendAt = System.nanoTime()
            }
        }
    }

    // ══════════════════════════════════════════════════════════
    // 反向链路：接收（只插入） + 播放（按播放头拉取）
    // ══════════════════════════════════════════════════════════

    private suspend fun startReverseAudio() {
        // 先等旧循环真正退出：旧循环仍可能持有同一个 native 解码器和
        // AudioPlayer，直接并发启动会出现两个消费者抢写/解码。
        // 但控制循环不能被 AudioTrack/HAL 卡死，超时后记录并继续。
        val old = reversePlayJob
        reversePlayJob = null
        val joined = withTimeoutOrNull(1000) {
            old?.cancelAndJoin()
            true
        } ?: false
        if (!joined) Log.w(TAG, "旧反向播放协程 1 s 内未退出，继续启动新会话")
        linkLock.lock()
        try {
            jitter.reset()
            linkSignal.signalAll()
        } finally {
            linkLock.unlock()
        }
        // 接收协程与会话同生命周期（见 launchReverseReceiver）
        audioPlayer?.flush()
        lastReverseStartAt = SystemClock.elapsedRealtime()
        linkLock.lock()
        try {
            lastReversePacketAt = 0L
        } finally {
            linkLock.unlock()
        }
        reversePlayJob = serviceScope.launch(Dispatchers.IO) { reversePlayLoop() }
        _status.value = _status.value.copy(
            reverseAudio = true,
            reverseBw = "FB 48 kHz 立体声"
        )
    }

    /**
     * 反向接收协程在**整个会话**内只启动一次：
     * `receive()` 是阻塞调用，协程取消唤不醒它；若随反向开关反复创建，
     * 每次都会留下一个卡在 recvfrom 上的线程。会话结束时关闭 socket 即可退出。
     */
    private fun launchReverseReceiver() {
        if (reverseRxJob?.isActive == true) return
        reverseRxJob = serviceScope.launch(Dispatchers.IO) { reverseRxLoop() }
    }

    private fun stopReverseAudio() {
        // 只停播放协程；接收协程继续保持阻塞 receive，等会话结束关闭 socket。
        // 非 suspend 版本用于服务停止路径，协程退出由服务作用域取消兜底。
        reversePlayJob?.cancel()
        reversePlayJob = null
        linkLock.lock()
        try {
            jitter.reset()
            lastReversePacketAt = 0L
            linkSignal.signalAll()
        } finally {
            linkLock.unlock()
        }
        audioPlayer?.flush()
        _status.value = _status.value.copy(
            reverseAudio = false, reverseBitrateKbps = 0f, reverseJitter = ""
        )
    }

    /** 需要保证旧循环已退出时使用（会话切换/ACK 超时/播放循环退出）。 */
    private suspend fun stopReverseAudioAndJoin() = withContext(NonCancellable) {
        val old = reversePlayJob
        reversePlayJob = null
        old?.cancelAndJoin()
        linkLock.lock()
        try {
            jitter.reset()
            lastReversePacketAt = 0L
            linkSignal.signalAll()
        } finally {
            linkLock.unlock()
        }
        audioPlayer?.flush()
        _status.value = _status.value.copy(
            reverseAudio = false, reverseBitrateKbps = 0f, reverseJitter = ""
        )
    }

    /**
     * 反向接收：**阻塞** receive，不使用 `soTimeout` 驱动。
     * 只做校验与入队，绝不在此解码。
     */
    private suspend fun reverseRxLoop() {
        val sender = udpSender ?: return
        val sock = sender.reverseSock ?: return
        val expectedIp = sender.getRemoteIp()
        val buf = ByteArray(2048)
        val pkt = java.net.DatagramPacket(buf, buf.size)
        var byteCount = 0L
        var lastReport = System.currentTimeMillis()

        // 接收线程是音频数据通路上游：被后台线程抢占一次就等于丢包。
        // Dispatchers.IO 的线程是共享复用的，退出时必须还原优先级，否则会把
        // URGENT_AUDIO 留在池线程上。还原只在协程仍跑在同一个线程时生效
        // （只有 socket 报错路径会 delay 而可能换线程）。
        val rxTid = android.os.Process.myTid()
        val rxPreviousPriority = android.os.Process.getThreadPriority(rxTid)
        android.os.Process.setThreadPriority(
            rxTid, android.os.Process.THREAD_PRIORITY_URGENT_AUDIO
        )
        try {
            while (coroutineContext.isActive) {
                pkt.length = buf.size
                try {
                    sock.receive(pkt)
                } catch (e: Exception) {
                    if (!coroutineContext.isActive || sock.isClosed) break
                    // 不忙等：socket 持续报错时也要让出线程，避免占满 IO 调度器
                    delay(10)
                    continue
                }
                val len = pkt.length
                if (expectedIp != null && pkt.address?.hostAddress != expectedIp) continue
                val rtp = Udp2CalProtocol.parseRtp(buf, len) ?: continue
                val ssrc = activeReverseSsrc
                if (ssrc == 0 || rtp.ssrc != ssrc) continue

                val nowMono = SystemClock.elapsedRealtime()
                linkLock.lock()
                try {
                    // 长时间没有 RTP（Windows 暂停播放/端点重建）后，本地静音已经
                    // 把 nextTs 推到接近墙钟。恢复的第一个包必须先重置时间线，
                    // 否则会被当作 late 丢掉，听起来就是“恢复后一直没声音”。
                    if (lastReversePacketAt != 0L &&
                        nowMono - lastReversePacketAt > REVERSE_IDLE_RESET_MS
                    ) {
                        Log.i(TAG, "反向 RTP 空闲 ${nowMono - lastReversePacketAt}ms，重置抖动缓冲")
                        jitter.reset()
                    }
                    lastReversePacketAt = nowMono
                    jitter.insert(
                        rtp.seq, rtp.timestamp, rtp.marker,
                        buf, rtp.payloadOffset, rtp.payloadLength
                    )
                    linkSignal.signalAll()
                } finally {
                    linkLock.unlock()
                }
                byteCount += len

                val now = System.currentTimeMillis()
                if (now - lastReport >= 1000) {
                    if (activeReverseSsrc != 0) {
                        // 抖动缓冲的“生病”指标必须能被眼睛看到：只写在日志里没人翻。
                        // 每秒取一次（JNI getter，代价可忽略）。
                        val jbSummary = "迟到 ${jitter.late} · 溢出 ${jitter.overflow} · " +
                            "PLC ${jitter.plcFrames}(峰 ${jitter.maxPlcRun}) · 重同步 ${jitter.resyncs}"
                        _status.value = _status.value.copy(
                            reverseBitrateKbps = byteCount * 8f / (now - lastReport),
                            reverseJitter = jbSummary
                        )
                    }
                    byteCount = 0
                    lastReport = now
                }
            }
        } finally {
            val exitTid = android.os.Process.myTid()
            if (exitTid == rxTid) {
                android.os.Process.setThreadPriority(exitTid, rxPreviousPriority)
            }
        }
    }

    /**
     * 写 PCM；AudioTrack 失效时原地重建并重试，而不是让播放协程永久退出。
     * 长时间静音欠载后部分设备会回收 track，重建后链路无需整条重连。
     */
    private suspend fun writeOrRecover(player: AudioPlayer, pcm: ShortArray, length: Int): Boolean {
        var failures = 0
        while (coroutineContext.isActive) {
            if (player.writeBlocking(pcm, 0, length)) return true
            failures++
            if (failures == 1 || failures % 50 == 0) {
                Log.w(TAG, "反向 AudioTrack 写入失败（连续 $failures 次），重建后重试")
            }
            if (failures >= 100) {
                Log.e(TAG, "反向 AudioTrack 连续重建失败，交由播放协程监督者重启")
                return false
            }
            if (!player.recover()) delay(100) else delay(20)
        }
        return false
    }

    /**
     * 反向播放：播放头是唯一时钟。
     *
     * 预缓冲 4 帧后开始写；每帧的播放/PLC 决策完全由 `playhead_ts` 决定，
     * `linkLock.wait(5)` 只是"等包"的存活等待，不承担节拍。
     */
    private suspend fun reversePlayLoop() {
        // 播放器在 AudioRecord 之后创建（为了复用 sessionId 接 AEC）。
        // ACK 可能先于麦克风初始化到达，这里必须等依赖就绪而不是直接退出，
        // 否则反向串流会永久沉默。
        var player = audioPlayer
        var dec = reverseDecoder
        // 首次启动时麦克风/AudioTrack 可能比 ACK 晚很久才就绪（冷启动、
        // 权限/音频服务初始化）。这里不能用固定超时：一旦超时退出，
        // 之后 activeReverseSsrc 不再变化，就再也不会重启播放协程，
        // 表现为"首次连接反向没声音，重连一次才好"。
        while ((player == null || dec == null) && coroutineContext.isActive) {
            delay(10)
            player = audioPlayer
            dec = reverseDecoder
        }
        if (player == null || dec == null) return
        val payload = ByteArray(Udp2CalProtocol.OPUS_MAX_PAYLOAD)
        // FEC 前瞻用的独立缓冲：pull 的 payload 与它互不干扰
        val fecPayload = ByteArray(Udp2CalProtocol.OPUS_MAX_PAYLOAD)
        var fecFrames = 0 // 本统计窗口内用 LBRR 重建成功的帧数
        val frameSamples = Udp2CalProtocol.OPUS_FRAME_SAMPLES * 2 // 立体声交错
        val pcm = ShortArray(frameSamples)
        val silence = ShortArray(frameSamples)
        // 受控重采样：媒体 48 kHz、设备 48 kHz，基础比率 1.0，只做 ±0.5% 微调。
        // 它现在是**唯一**的执行手段（不再有样本级丢/插），重采样器本身是
        // 多相 FIR，所以比率怎么扫都不会在 12 kHz 处起伏。
        val resampler = StreamResampler(2)
        resampler.setSourceRate(Udp2CalProtocol.OPUS_SAMPLE_RATE, Udp2CalProtocol.OPUS_SAMPLE_RATE)
        var lastEpoch = -1L
        var lastLatencyLog = 0L
        var latencyMs = 0
        // 阶跃吸收（方案 C）：与 Windows 端同一套判据与限额
        val step = Udp2CalProtocol.StepAbsorber()
        // 起播直接预缓冲到目标水位，避免"30ms 起播后用几分钟追到 150ms"。
        jitter.setStartPrebufferMs(drift.targetMs())
        var adjust = 0.0
        // 冻结水位反馈（断流 / 预缓冲未出声）：这两种状态下 water 长期低于目标，
        // 若继续积分，积分会压到 -PI_INTEGRAL_MAX 的负饱和；恢复出声后只能按
        // ≈12 ppm/s 慢慢退，稳态水位被顶在目标之上 10–30 ms 长达几分钟，
        // 期间还带着几分音分的音高偏移。与 windows/src/main.rs 解码线程同源。
        var idleFeedback = false
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO)

        try {
        while (coroutineContext.isActive) {
            var code = Udp2CalProtocol.Pull.WAIT
            var pipeMs = 0
            var waterMs = 0
            var jbPlaying = false
            var pulledLen = 0
            var needReset = false
            var reason = ""
            var jbNextTs = 0L
            var jbResyncs = 0L
            var jbOverflow = 0L
            var arrivalNanos = -1L
            var head = 0L
            // 阶跃吸收的本 tick 结论（锁内决定，锁外记日志）
            var stepBefore = 0
            var stepWant = 0
            var stepSkipped = 0
            var stepTotal = 0L
            var stepLimited = false
            var trimFrames = 0

            linkLock.lock()
            try {
                if (jitter.epoch != lastEpoch) {
                    lastEpoch = jitter.epoch
                    needReset = true
                    reason = jitter.lastRebaseReason
                    jbNextTs = jitter.nextTs
                    jbResyncs = jitter.resyncs
                    jbOverflow = jitter.overflow
                }
                head = player.playbackHeadFrames()
                // Android 没有独立 PCM 环：解码输出直接阻塞写进 AudioTrack，
                // 所以"已解码未播完"的只有播放缓冲这一段，ring 恒为 0。
                // 生产节流因此天然由 writeBlocking 承担，水位只做缺口判定。
                val pipeFrames = player.pendingFrames(head)
                pipeMs = (pipeFrames * 1000L / Udp2CalProtocol.OPUS_SAMPLE_RATE).toInt()

                // ── 阶跃吸收（方案 C）：持续超额才裁，先按水位拉取 ──
                // 判据：occupancy + pipe 连续 STEP_TRIM_HOLD_MS 超过
                // target + STEP_TRIM_EXCESS_MS。单次 90 ms 攒批不触发。
                // 命中后逐帧解码再丢 PCM（见下），不是清槽位硬跳。
                val beforeWater = jitter.occupancyMs()
                val beforeTotal = beforeWater + pipeMs
                val decision = step.decide(
                    beforeWater,
                    pipeMs,
                    jitter.playing,
                    // 与漂移控制器同源：目标水位以控制器里的为准
                    drift.targetMs(),
                )
                if (decision.frames > 0) {
                    stepBefore = beforeTotal
                    stepWant = decision.frames
                    stepLimited = decision.limited
                    trimFrames = if (decision.limited) 0 else decision.frames
                }

                // 阶跃裁剪必须沿播放顺序**解码后丢弃 PCM**：只推进缓冲游标会让
                // 有状态 Opus 解码器与发送端脱节（接缝/音染）。这里最多 12 帧，
                // 且每分钟最多 3 次，锁内解码的代价可以接受。
                var guard = trimFrames
                while (guard-- > 0) {
                    val c = jitter.pullTrim(payload)
                    when (c) {
                        Udp2CalProtocol.Pull.PACKET -> {
                            dec.decode(payload, 0, jitter.pulledLength, pcm)
                            stepSkipped++
                        }
                        Udp2CalProtocol.Pull.PLC -> {
                            dec.plc(pcm)
                            stepSkipped++
                        }
                        Udp2CalProtocol.Pull.SILENCE -> stepSkipped++
                        else -> break
                    }
                }
                stepTotal = jitter.stepTrims

                // Android 没有独立 PCM 环：pendingFrames 就是"已提交、未播出"的
                // 全部余量，用它做 PLC 截止判据（设备缓冲不是固定延迟）。
                code = jitter.pull(pipeMs, pipeMs, payload)
                // 可听点之前还压着的媒体量（设备帧 → 媒体样本要用当前比率换算）
                val queuedMedia = (pipeFrames.toDouble() * resampler.ratio).toLong()
                val audibleTs = (jitter.nextTs - queuedMedia).coerceAtLeast(0L)
                // 延迟估算：可听点那一帧是多久以前到达的
                arrivalNanos = jitter.arrivalNanosAt(audibleTs)
                // 单一口径水位：抖动缓冲 + 解码/播放缓冲（声卡已接手的那段）
                waterMs = jitter.waterMs() + pipeMs
                jbPlaying = jitter.playing
                pulledLen = jitter.pulledLength
            } finally {
                linkLock.unlock()
            }

            if (needReset) {
                // 只有真正的重同步才值得告警（会 flush 掉已排队 PCM = 一次破音）；
                // 会话开始的 epoch 变化属于正常复位，不写日志
                if (reason.isNotEmpty()) {
                    Log.w(
                        TAG,
                        "反向重同步：$reason | nextTs=$jbNextTs 水位=${waterMs}ms " +
                            "重同步数=$jbResyncs 溢出=$jbOverflow"
                    )
                }
                dec.reset()
                drift.reset()
                deviceRate.reset()
                step.reset()
                resampler.reset()
                player.flush()
            }

            // 阶跃吸收的日志（锁外：Log 会走 binder）。水位一律用**总水位**
            // （jitter + pipe）口径，与判据一致；限额分支每 60 s 窗口只放行一条：
            // 被压制时水位不会自己降下来，不节流就是 ≈200 条/秒。
            if (stepWant > 0) {
                if (stepLimited) {
                    if (step.takeLimitedLog()) {
                        Log.w(
                            TAG,
                            "[Step] 水位 ${stepBefore}ms（目标 ${drift.targetMs()}）" +
                                "命中阶跃吸收，但每分钟 ${Udp2CalProtocol.STEP_TRIM_MAX_PER_MIN} 次限额已满" +
                                "（本窗口已用 ${step.trimsInWindow()} 次）：本次只记录不裁剪"
                        )
                    }
                } else if (stepSkipped > 0) {
                    Log.i(
                        TAG,
                        "[Step] 阶跃吸收：整帧跳 $stepSkipped / $stepWant 帧" +
                            "（${stepSkipped * Udp2CalProtocol.OPUS_FRAME_MS}ms），" +
                            "水位 ${stepBefore}→${waterMs}ms，累计 $stepTotal 帧"
                    )
                }
            }

            // ── 比率 = 设备时钟前馈(管速率) + 水位反馈(管长期残余) ──
            // 网络到达抖动只进 jitter，绝不进重采样比率。
            // 设备钟差观测：优先 AudioTrack 时间戳（位置+单调时刻），
            // 避免把协程调度抖动误当成设备速率差；不可用时退回播放头。
            val clock = player.deviceClockSample()
            if (clock != null) {
                deviceRate.observe(clock.first, clock.second)
            } else {
                deviceRate.observe(head, System.nanoTime())
            }
            val ff = deviceRate.feedforward() ?: 0.0
            val nowMono = SystemClock.elapsedRealtime()
            val rtpIdle = lastReversePacketAt == 0L ||
                nowMono - lastReversePacketAt > 500L
            // 反馈只在**真的在出声**时才评估。只看 rtpIdle 不够：预缓冲阶段
            // （jitter 还没攒够 startPrebuffer、playing=false）水位必然低于目标，
            // 照常喂 PI 会把积分压到负饱和；开播后只能按 ki·|err| 慢慢退，
            // 稳态水位因此停在目标之上 10–30 ms。现场日志指纹：开播瞬间
            // `反馈 -1100ppm`，之后 -457 → -294 → +21 → +65 缓慢爬升。
            val feedbackOn = !rtpIdle && jbPlaying
            val du = if (!feedbackOn) {
                if (!idleFeedback) {
                    idleFeedback = true
                    drift.reset()
                }
                0.0
            } else {
                if (idleFeedback) {
                    idleFeedback = false
                    drift.reset()
                }
                drift.update(waterMs, head)
            }
            adjust = ff + du
            // 两支路相合后仍须守比率钳位：前馈和反馈各自钳过了，乘积可能越界。
            adjust = adjust.coerceIn(
                -Udp2CalProtocol.RESAMPLE_MAX_ADJUST,
                Udp2CalProtocol.RESAMPLE_MAX_ADJUST,
            )
            resampler.setAdjust(adjust)

            // ── 总延迟估算 ──
            // 口径：正在播放的那一帧是多久以前到达本机的。AudioTrack 的播放头
            // 已是"混音器消费到的位置"，所以这个年龄天然包含播放缓冲，
            // 只少一个混音周期（5–20 ms，公开 SDK 无法读出）。
            if (arrivalNanos > 0L) {
                val ageMs = ((System.nanoTime() - arrivalNanos) / 1_000_000L).toInt()
                latencyMs = ageMs.coerceAtLeast(0)
                val now = System.currentTimeMillis()
                if (now - lastLatencyLog >= 10_000) {
                    lastLatencyLog = now
                    val ffPpm = ff * 1e6
                    Log.i(
                        TAG,
                        "反向延迟≈${latencyMs}ms 水位=${waterMs}ms（抖动 ${jitter.waterMs()} + 播放 $pipeMs，目标 ${drift.targetMs()}）" +
                            " 比率=${"%.3f".format(adjust * 100)}%（反馈 ${"%.0f".format(du * 1e6)}ppm / 设备前馈 ${"%.1f".format(ffPpm)}ppm，σ ${"%.1f".format(deviceRate.sigmaPpm())}ppm ${deviceRate.buckets()}桶）" +
                            " FEC=$fecFrames"
                    )
                    fecFrames = 0
                }
            }

            when (code) {
                Udp2CalProtocol.Pull.PACKET -> {
                    val n = dec.decode(payload, 0, pulledLen, pcm)
                    if (n <= 0) continue // 解码失败按丢包处理，水位见底后 PLC
                    // PLC 帧和真实帧走同一条时间线，所以都过同一个重采样器
                    val produced = resampler.process(pcm, 0, n / 2)
                    if (produced > 0) {
                        if (!writeOrRecover(player, resampler.buffer(), produced)) return
                    }
                }

                Udp2CalProtocol.Pull.PLC -> {
                    // 先用 Opus 带内 FEC 重建这一帧：丢帧的 LBRR 冗余在**下一个
                    // 包里**，所以前瞻一个包（不消费，下一轮 pull 仍会正常取它）
                    // 来还原；下一个包也还没到时才退回 PLC。发送端没开
                    // in-band FEC 时 libopus 自动退化成 PLC，故不会更差。
                    // 现场最常见的就是"包没丢、只是来晚了"（AP 攒批/省电）：
                    // 一次 >100 ms 的停顿会连丢 3 帧，以前全 PLC，听起来就是
                    // 一下音量塌陷。判据与 windows/src/main.rs 同源。
                    var fecLen = 0
                    linkLock.lock()
                    try {
                        fecLen = jitter.peek(fecPayload)
                    } finally {
                        linkLock.unlock()
                    }
                    var n = if (fecLen > 0) dec.decodeFec(fecPayload, 0, fecLen, pcm) else -1
                    if (n > 0) {
                        fecFrames++
                    } else {
                        n = dec.plc(pcm)
                    }
                    if (n > 0) {
                        val produced = resampler.process(pcm, 0, n / 2)
                        if (produced > 0) {
                            if (!writeOrRecover(player, resampler.buffer(), produced)) return
                        }
                    } else {
                        // 生成不出 PLC：补静音，同样过重采样器以保持相位连续
                        val produced = resampler.process(silence, 0, silence.size / 2)
                        if (produced > 0) {
                            if (!writeOrRecover(player, resampler.buffer(), produced)) return
                        }
                    }
                }

                Udp2CalProtocol.Pull.SILENCE -> {
                    // 连续 PLC 已达上限：输出静音但继续推进媒体时间
                    val produced = resampler.process(silence, 0, silence.size / 2)
                    if (produced > 0) {
                        if (!writeOrRecover(player, resampler.buffer(), produced)) return
                    }
                }

                else -> {
                    // 没有可播放的包（空闲/起播预缓冲）：本机补静音保持
                    // AudioTrack 不断流。发送端不再为“保活”合成静音，
                    // 因此这里的本地输出是唯一的空闲期供数来源。
                    if (pipeMs <= Udp2CalProtocol.PLC_LEAD_MS) {
                        val produced = resampler.process(silence, 0, silence.size / 2)
                        if (produced > 0 && !writeOrRecover(player, resampler.buffer(), produced)) return
                    } else {
                        // 播放缓冲还够：等接收协程的 signal（超时只防漏唤醒）
                        linkLock.lock()
                        try {
                            linkSignal.await(5, java.util.concurrent.TimeUnit.MILLISECONDS)
                        } catch (_: InterruptedException) {
                        } finally {
                            linkLock.unlock()
                        }
                    }
                }
            }
        }
        } finally {
            // 播放循环退出，把原生句柄还回去（本循环每连接新建一个）
            step.close()
        }
    }

    // ══════════════════════════════════════════════════════════
    // 生命周期
    // ══════════════════════════════════════════════════════════

    fun stopCapture() {
        pendingRestart = null
        activeParams = null
        restartPending = false
        hasNewCapture = false
        try { wifiLock?.release() } catch (_: Exception) {}
        wifiLock = null
        captureJob?.cancel()
        captureJob = null
        controlJob?.cancel()
        controlJob = null
        stopReverseAudio()
        _status.value = _status.value.copy(connected = false, reverseAudio = false)
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel("udp2cal_capture", "采集服务", NotificationManager.IMPORTANCE_LOW)
                    .apply { setShowBadge(false) }
            )
        }
    }

    private fun buildNotification(): Notification {
        val status = _status.value
        val modeLabel = if (status.opusMode.contains("语音")) "语音" else "音乐"
        val audioLabel = when {
            status.audioSource.contains("AEC已启用") -> "NS+AGC+AEC"
            status.audioSource.contains("硬件降噪") -> "NS+AGC"
            else -> "MIC-Direct"
        }
        val connStr = if (status.connected) "已连接" else "等待连接"
        return NotificationCompat.Builder(this, "udp2cal_capture")
            .setContentTitle("UDP2CAL")
            .setContentText("$connStr · $modeLabel $audioLabel")
            .setSmallIcon(android.R.drawable.ic_btn_speak_now)
            .setOngoing(true)
            .build()
    }

    override fun onDestroy() {
        val oldCapture = captureJob
        val oldPlay = reversePlayJob
        // 反向接收协程也必须等：它是唯一调用 jitter.insert（原生 jbInsert）
        // 的地方，若在 jitter.close() 之后仍在跑，就是跨线程 use-after-free。
        val oldRx = reverseRxJob
        stopCapture()
        serviceScope.cancel()
        // 顺序有意义：captureJob 的收尾会调用 udpSender.close() 关闭反向 socket，
        // 而 reverseRxLoop 阻塞在 sock.receive() 上、协程取消唤不醒它——所以先等
        // 采集协程退出（socket 被关掉），接收协程才会跟着退出。
        val allExited = runBlocking {
            withTimeoutOrNull(1000) {
                oldCapture?.join()
                oldRx?.join()
                oldPlay?.join()
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
        // 原生句柄用完之后必须归还：JitterBuffer/DriftController/RateEstimator 的
        // 状态在 Rust 堆上（protocol/jni.rs），Kotlin 只是持有 jlong 句柄。
        runCatching { jitter.close() }
        runCatching { drift.close() }
        runCatching { deviceRate.close() }
        super.onDestroy()
    }
}
