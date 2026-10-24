package com.udp2cal.app.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.media.audiofx.AcousticEchoCanceler
import android.net.wifi.WifiManager
import android.os.Binder
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import com.udp2cal.app.AudioPlayer
import com.udp2cal.app.DiscoveryManager
import com.udp2cal.app.Prefs
import com.udp2cal.app.Udp2CalProtocol
import com.udp2cal.app.UdpSender
import com.udp2cal.app.native.OpusDecoder as ReverseOpusDecoder
import com.udp2cal.app.native.OpusEncoder
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
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
    }

    private val binder = LocalBinder()
    private val serviceScope = CoroutineScope(Dispatchers.IO + SupervisorJob())

    @Volatile private var captureJob: Job? = null
    private var controlJob: Job? = null
    private var reverseRxJob: Job? = null
    private var reversePlayJob: Job? = null
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
    private val clock = Udp2CalProtocol.PlayheadClock()
    @Volatile private var activeReverseSsrc = 0

    private data class CaptureParams(
        val sampleRateHz: Int,
        val bitrateKbps: Int,
        val targetIp: String,
        val targetPort: Int,
        val testTone: Boolean,
    )

    private var pendingRestart: CaptureParams? = null

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

        captureJob = serviceScope.launch {
            _status.value = _status.value.copy(isRunning = true, errorMsg = "")
            try {
                // ── 网络层 ──
                val sender = UdpSender(targetIp, targetPort)
                if (!sender.connect()) {
                    fail("网络连接失败")
                    return@launch
                }
                udpSender = sender
                activeReverseSsrc = 0

                // ── 正向编码器（固定 48 kHz / 10 ms / 单声道）──
                val enc = OpusEncoder(
                    sampleRateHz = Udp2CalProtocol.OPUS_SAMPLE_RATE,
                    bitrateKbps = Udp2CalProtocol.clampBitrateKbps(bitrateKbps),
                )
                encoder = enc
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
                    bitrateTargetKbps = Udp2CalProtocol.clampBitrateKbps(bitrateKbps),
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
            } finally {
                controlJob?.cancel()
                controlJob = null
                stopReverseAudio()
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
                    stopReverseAudio()
                    activeReverseSsrc = 0
                }
            }

            if (connected) {
                val ssrc = sender.reverseSsrc
                if (ssrc != activeReverseSsrc) {
                    activeReverseSsrc = ssrc
                    if (ssrc == 0) {
                        // ACK 明确表示反向未启用：必须停止播放并清空缓冲
                        stopReverseAudio()
                    } else {
                        startReverseAudio()
                    }
                }
            }

            delay(TICK_MS)
        }
    }

    /** CONNECT 负载固定 15 字节；Opus 参数只描述**反向**（Windows 编码器） */
    private fun buildConnectPayload(sender: UdpSender): ByteArray {
        val autoBr = Prefs.opusBitrateKbps <= 0
        val dtx = Prefs.opusDtx == 1
        val vbr = Prefs.opusVbr == 1 || dtx || autoBr
        val vbrConstraint = vbr && !autoBr && Prefs.opusVbrConstraint == 1
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
            bitrateKbps = if (autoBr) 0 else Prefs.opusBitrateKbps.coerceAtMost(Udp2CalProtocol.BITRATE_MAX_KBPS),
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
        val frameNs = FRAME * 1_000_000_000L / Udp2CalProtocol.OPUS_SAMPLE_RATE
        var nextAt = System.nanoTime()
        sendFrames(enc, sender) { buf, off, len ->
            for (i in 0 until len) {
                buf[off + i] = (Math.sin(phase) * 16384.0).toInt().toShort()
                phase += omega
                if (phase > 2.0 * Math.PI) phase -= 2.0 * Math.PI
            }
            // 测试音没有硬件节拍，只能按媒体时间自走
            nextAt += frameNs * len / FRAME
            val waitNs = nextAt - System.nanoTime()
            if (waitNs > 0) delay(waitNs / 1_000_000)
            len
        }
    }

    /**
     * 统一的 10 ms 帧发送循环。
     *
     * 规则（规范 4.3/4.4/8.1）：
     *   * 每帧时间戳 +480，**无论是否真的发出**；
     *   * 编码失败/发送失败：序列号仍 +1，让接收端看到缺口并触发 PLC；
     *   * DTX 空帧（编码返回 0）：不递增序列号；
     *   * 未连接（无新鲜 ACK）时不编码不发送。
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
        var lastSentAt = 0L
        var byteCount = 0L
        var lastReport = System.currentTimeMillis()
        /** 已累积但还不足一帧的样本数 */
        var filled = 0

        while (coroutineContext.isActive) {
            // AudioRecord.read 允许返回不足请求量的样本（缓冲区尚未填满）。
            // 必须累积到整 480 采样再编码：补零发送等于丢音频，且会让帧率翻倍
            // （实测 190 包/秒 → 接收端缓冲被灌满，41% 的包被判超窗丢弃）。
            val n = readInto(pcm, filled, FRAME - filled)
            if (n < 0) {
                // 设备失效：不空转，交给外层 finally 收尾
                _status.value = _status.value.copy(errorMsg = "麦克风读取失败，请重新开始采集")
                hasNewCapture = false
                return
            }
            filled += n
            if (filled < FRAME) continue
            filled = 0

            val now = System.currentTimeMillis()
            if (sender.isAckFresh(now)) {
                val n = enc.encodeTo(pcm, packet, Udp2CalProtocol.RTP_HEADER_SIZE)
                if (n > 0) {
                    val marker = first || (now - lastSentAt > Udp2CalProtocol.RESYNC_GAP_MS.toLong())
                    Udp2CalProtocol.writeRtpHeader(
                        packet, 0, marker, seq, timestamp, sender.forwardSsrc
                    )
                    if (sender.send(packet, 0, Udp2CalProtocol.RTP_HEADER_SIZE + n)) {
                        byteCount += n
                        lastSentAt = System.currentTimeMillis()
                        first = false
                    }
                }
                // 编码成功（含 DTX 空帧）才推进序列号：DTX 未发包不递增
                if (n >= 0) seq = (seq + 1) and 0xFFFF
            }
            timestamp += FRAME

            if (now - lastReport >= 1000) {
                val elapsed = (now - lastReport).coerceAtLeast(1)
                _status.value = _status.value.copy(bitrateKbps = byteCount * 8f / elapsed)
                byteCount = 0
                lastReport = now
            }
        }
    }

    // ══════════════════════════════════════════════════════════
    // 反向链路：接收（只插入） + 播放（按播放头拉取）
    // ══════════════════════════════════════════════════════════

    private fun startReverseAudio() {
        reversePlayJob?.cancel()
        linkLock.lock()
        try {
            jitter.reset()
            linkSignal.signalAll()
        } finally {
            linkLock.unlock()
        }
        // 接收协程与会话同生命周期（见 launchReverseReceiver）
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
        // 只停播放协程；接收协程继续保持阻塞 receive，等会话结束关闭 socket
        reversePlayJob?.cancel()
        reversePlayJob = null
        linkLock.lock()
        try {
            jitter.reset()
            linkSignal.signalAll()
        } finally {
            linkLock.unlock()
        }
        audioPlayer?.flush()
        _status.value = _status.value.copy(reverseAudio = false, reverseBitrateKbps = 0f)
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

        while (coroutineContext.isActive) {
            pkt.length = buf.size
            try {
                sock.receive(pkt)
            } catch (e: Exception) {
                if (!coroutineContext.isActive || sock.isClosed) break
                continue
            }
            val len = pkt.length
            if (expectedIp != null && pkt.address?.hostAddress != expectedIp) continue
            val rtp = Udp2CalProtocol.parseRtp(buf, len) ?: continue
            val ssrc = activeReverseSsrc
            if (ssrc == 0 || rtp.ssrc != ssrc) continue

            linkLock.lock()
            try {
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
                    _status.value = _status.value.copy(
                        reverseBitrateKbps = byteCount * 8f / (now - lastReport)
                    )
                }
                byteCount = 0
                lastReport = now
            }
        }
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
        var waitedMs = 0
        while ((player == null || dec == null) && coroutineContext.isActive && waitedMs < 3000) {
            delay(10)
            waitedMs += 10
            player = audioPlayer
            dec = reverseDecoder
        }
        if (player == null || dec == null) return
        val payload = ByteArray(Udp2CalProtocol.OPUS_MAX_PAYLOAD)
        val frameSamples = Udp2CalProtocol.OPUS_FRAME_SAMPLES * 2 // 立体声交错
        val pcm = ShortArray(frameSamples)
        val silence = ShortArray(frameSamples)
        val insertBuf = ShortArray(Udp2CalProtocol.DRIFT_MAX_SAMPLES * 2)
        var mediaSamples = 0L
        var lastEpoch = -1L
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO)

        while (coroutineContext.isActive) {
            var code = Udp2CalProtocol.Pull.WAIT
            var waterMs = 0
            var pulledLen = 0
            var pulledTs = 0L
            var needReset = false
            var reason = ""
            var jbNextTs = 0L
            var jbResyncs = 0L
            var jbOverflow = 0L

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
                val head = player.playbackHeadFrames()
                val playhead = clock.update(head) ?: jitter.nextTs
                code = jitter.pull(playhead, payload)
                // 漂移校正看总缓冲：抖动缓冲里排队的 + 已写进 AudioTrack 还没播的
                val queuedMs = (player.pendingFrames(head) * 1000L /
                    Udp2CalProtocol.OPUS_SAMPLE_RATE).toInt()
                waterMs = jitter.waterMs() + queuedMs
                pulledLen = jitter.pulledLength
                pulledTs = jitter.pulledTs
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
                clock.reset()
                drift.reset()
                player.flush()
                mediaSamples = 0
            }

            when (code) {
                Udp2CalProtocol.Pull.PACKET -> {
                    val n = dec.decode(payload, 0, pulledLen, pcm)
                    if (n <= 0) continue // 解码失败按丢包处理，播放头到期后 PLC
                    if (!clock.isStarted) clock.start(pulledTs, player.playbackHeadFrames())

                    var writeLen = n
                    var insertLen = 0
                    val adj = drift.update(waterMs, mediaSamples)
                    if (adj > 0) {
                        // 水位过高：丢掉尾部，并做 1 ms 淡出避免接缝爆音
                        val k = minOf(adj, n / 2)
                        writeLen = n - k
                        val fade = minOf(48, writeLen)
                        for (i in 0 until fade) {
                            val idx = writeLen - 1 - i
                            pcm[idx] = (pcm[idx] * (1f - i.toFloat() / fade)).toInt().toShort()
                        }
                        clock.notePcmCorrection(k.toLong())
                        linkLock.lock()
                        try { jitter.notePcmCorrection(k.toLong()) } finally { linkLock.unlock() }
                        mediaSamples += k
                    } else if (adj < 0) {
                        // 水位过低：把尾部渐入重复一次
                        insertLen = minOf(-adj, insertBuf.size / 2) * 2
                        val tailStart = n - insertLen
                        for (i in 0 until insertLen) {
                            val gain = i.toFloat() / insertLen
                            val src = pcm[(tailStart + i).coerceIn(0, n - 1)]
                            insertBuf[i] = (src * gain).toInt().toShort()
                        }
                        clock.notePcmCorrection(-insertLen.toLong())
                        linkLock.lock()
                        try { jitter.notePcmCorrection(-insertLen.toLong()) } finally { linkLock.unlock() }
                    }

                    if (!player.writeBlocking(pcm, 0, writeLen)) return
                    if (insertLen > 0) {
                        if (!player.writeBlocking(insertBuf, 0, insertLen)) return
                        mediaSamples += insertLen
                    }
                    mediaSamples += n
                }

                Udp2CalProtocol.Pull.PLC -> {
                    val n = dec.plc(pcm)
                    if (!clock.isStarted) clock.start(pulledTs, player.playbackHeadFrames())
                    if (n > 0) {
                        if (!player.writeBlocking(pcm, 0, n)) return
                        mediaSamples += n
                    } else {
                        if (!player.writeBlocking(silence, 0, silence.size)) return
                        mediaSamples += silence.size
                    }
                }

                Udp2CalProtocol.Pull.SILENCE -> {
                    // 连续 PLC 已达上限：输出静音但继续推进媒体时间
                    if (!clock.isStarted) clock.start(pulledTs, player.playbackHeadFrames())
                    if (!player.writeBlocking(silence, 0, silence.size)) return
                    mediaSamples += silence.size
                }

                else -> {
                    // 没数据：等接收协程的 signal（超时只防漏唤醒，不承担节拍）
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

    // ══════════════════════════════════════════════════════════
    // 生命周期
    // ══════════════════════════════════════════════════════════

    fun stopCapture() {
        pendingRestart = null
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
        stopCapture()
        serviceScope.cancel()
        super.onDestroy()
    }
}
