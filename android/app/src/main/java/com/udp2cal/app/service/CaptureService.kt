package com.udp2cal.app.service

import android.app.*
import android.content.Context
import android.content.Intent
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.media.audiofx.AcousticEchoCanceler
import android.os.Binder
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import java.net.DatagramPacket
import com.udp2cal.app.AudioPlayer
import com.udp2cal.app.DiscoveryManager
import com.udp2cal.app.Prefs
import com.udp2cal.app.Udp2CalProtocol
import com.udp2cal.app.UdpSender
import com.udp2cal.app.native.OpusDecoder as ReverseOpusDecoder
import com.udp2cal.app.native.OpusEncoder
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

class CaptureService : Service() {
    companion object { private const val TAG = "CaptureService" }

    private val binder = LocalBinder()
    private val serviceScope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    @Volatile private var captureJob: Job? = null
    private var reverseAudioJob: Job? = null
    private var hasNewCapture = false
    /** 模式切换标记：true 时 finally 保留网络层和反向解码器 */
    @Volatile private var isModeRestart = false

    private val _status = MutableStateFlow(CaptureStatus())
    val status: StateFlow<CaptureStatus> = _status.asStateFlow()

    private var audioRecord: AudioRecord? = null
    private var encoder: OpusEncoder? = null
    private var udpSender: UdpSender? = null
    /** 音频帧通道引用，用于 stopCapture 时强制关闭 */
    private var audioChannel: Channel<ShortArray>? = null
    /** 反向音频（PC→Phone） */
    private var reverseDecoder: ReverseOpusDecoder? = null
    private var audioPlayer: AudioPlayer? = null

    private data class CaptureParams(
        val sampleRateHz: Int, val bitrateKbps: Int, val targetIp: String, val targetPort: Int,
        val testTone: Boolean
    )
    private var pendingRestart: CaptureParams? = null

    /** 音源描述枚举 */
    object AudioSourceLabel {
        const val MIC_RAW = "MIC直出"                    // 裸麦克风，硬件降噪关闭
        const val SYSTEM_NS = "系统硬件降噪"              // VOICE_COMMUNICATION，安卓系统原生降噪
        const val SYSTEM_NS_FALLBACK = "系统硬件降噪(已回退MIC)" // VOICE_COMMUNICATION失败，回退MIC
        const val AEC_ENABLED = " + AEC已启用"
        const val AEC_UNAVAILABLE = " + AEC不可用"
    }

    data class CaptureStatus(
        val isRunning: Boolean = false,
        val bitrateKbps: Float = 0f,
        val sampleRateHz: Int = 0,
        val bitrateTargetKbps: Int = 0,
        val vbrMode: String = "",                // "CBR" / "VBR" / "VBR约束" / "VBR(DTX)"
        val audioSource: String = "",            // 当前音源描述
        val opusMode: String = "",               // "语音模式" / "全频模式"
        val errorMsg: String = "",
        val connected: Boolean = false,          // P2P 独占连接状态
        val deviceId: String = "",               // 本机设备 ID
        val reverseAudio: Boolean = false,       // 反向串流是否活跃
        val reverseBitrateKbps: Float = 0f,      // 反向串流码率
        val reverseBw: String = ""               // 反向串流音频带宽
    )

    inner class LocalBinder : Binder() { fun getService(): CaptureService = this@CaptureService }
    override fun onBind(intent: Intent?): IBinder = binder
    override fun onCreate() { super.onCreate() }

    // startForeground 必须在 startService→onStartCommand 时立即调用（5s限制）
    //   NotificationChannel 必须在权限授予后创建 → 放这里而非 onCreate
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        createNotificationChannel()
        try { startForeground(1, buildNotification()) } catch (e: Exception) {
            Log.e(TAG, "startForeground failed", e)
            stopSelf()
            return START_NOT_STICKY
        }
        // 动态更新通知栏状态
        serviceScope.launch {
            _status.collect {
                try {
                    startForeground(1, buildNotification())
                } catch (_: Exception) {}
            }
        }
        return START_STICKY
    }

    fun startCapture(sampleRateHz: Int, bitrateKbps: Int, targetIp: String, targetPort: Int,
                     testTone: Boolean = false) {
        if (captureJob?.isActive == true) {
            pendingRestart = CaptureParams(sampleRateHz, bitrateKbps, targetIp, targetPort, testTone)
            captureJob?.cancel()
            return
        }
        pendingRestart = null
        isModeRestart = false
        doStartCapture(sampleRateHz, bitrateKbps, targetIp, targetPort, testTone)
    }

    /**
     * 判断当前 Opus 信号类型是否为语音模式，驱动音源选择
     * 仅此一个状态变量控制音源切换：
     *   true  → VOICE_COMMUNICATION（系统原生硬件降噪）
     *   false → MIC（裸麦克风，硬件降噪关闭）
     */
    private fun isVoiceMode(): Boolean = Prefs.opusSignal == 3001 // OPUS_SIGNAL_VOICE

    private fun doStartCapture(
        sampleRateHz: Int, bitrateKbps: Int, targetIp: String, targetPort: Int,
        testTone: Boolean
    ) {
        val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
        val wakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "UDP2CAL:Capture")
        try { wakeLock.acquire(10 * 60 * 1000L) } catch (_: Exception) {}

        hasNewCapture = true
        // 捕获启动时的 isVoiceMode，用于消费者协程检测变更
        val startIsVoiceMode = isVoiceMode()

        captureJob = serviceScope.launch {
            _status.value = _status.value.copy(isRunning = true, errorMsg = "")

            try {
                if (isModeRestart && udpSender != null) {
                    // 模式切换：复用已有 UdpSender，不发新 CONNECT（端口不变）
                    Log.i(TAG, "🔄 模式切换，复用已有 UdpSender")
                } else {
                    udpSender = UdpSender(targetIp, targetPort)
                    if (!udpSender!!.connect()) {
                        _status.value = _status.value.copy(isRunning = false, errorMsg = "网络连接失败")
                        stopSelf()
                        return@launch
                    }
                }
                encoder = OpusEncoder(sampleRateHz, bitrateKbps).also {
                    it.deviceId = udpSender!!.getDeviceId() ?: DiscoveryManager.getOrCreateDeviceId()
                }
                if (!encoder!!.start()) {
                    _status.value = _status.value.copy(isRunning = false, errorMsg = "编码器初始化失败，已保留原始PCM通路")
                    stopSelf()
                    return@launch
                }

                val frameSize = encoder!!.frameSize
                if (frameSize <= 0) {
                    _status.value = _status.value.copy(isRunning = false, errorMsg = "编码器帧大小无效")
                    stopSelf()
                    return@launch
                }

                _status.value = _status.value.copy(
                    sampleRateHz = sampleRateHz,
                    bitrateTargetKbps = bitrateKbps,
                    opusMode = if (startIsVoiceMode) "语音模式" else "全频模式"
                )

                if (testTone) {
                    var byteCount = 0L
                    var lastReport = System.currentTimeMillis()
                    val omega = 2.0 * Math.PI * 1000.0 / sampleRateHz
                    val testToneBuf = ShortArray(frameSize)
                    var phase = 0.0
                    val frameIntervalNs = (frameSize * 1_000_000_000L) / sampleRateHz
                    var nextFrameTime = System.nanoTime()

                    _status.value = _status.value.copy(isRunning = true)

                    val sendBuffer = ByteArray(Udp2CalProtocol.MAX_PACKET)

                    while (isActive) {
                        for (i in 0 until frameSize) {
                            val sample = (Math.sin(phase) * 16384.0).toInt().coerceIn(-32768, 32767)
                            testToneBuf[i] = sample.toShort()
                            phase += omega
                            if (phase > 2.0 * Math.PI) phase -= 2.0 * Math.PI
                        }

                        val written = encoder?.encodeTo(testToneBuf, sendBuffer, 0) ?: -1
                        if (written > 0) {
                            if (udpSender?.send(sendBuffer, 0, written) == true) byteCount += written
                        }

                        nextFrameTime += frameIntervalNs
                        var nsNow: Long
                        do {
                            nsNow = System.nanoTime()
                            if (nsNow >= nextFrameTime) break
                            val remainingUs = (nextFrameTime - nsNow) / 1000
                            if (remainingUs > 2000) Thread.sleep(remainingUs / 1000 - 1)
                        } while (System.nanoTime() < nextFrameTime)

                        val msNow = System.currentTimeMillis()
                        if (msNow - lastReport >= 1000) {
                            val kbps = (byteCount * 8f) / (msNow - lastReport)
                            _status.value = _status.value.copy(bitrateKbps = kbps)
                            byteCount = 0
                            lastReport = msNow
                        }
                    }
                } else {
                    // === 正常麦克风采集（生产-消费双协程模型）===
                    val minBufSize = AudioRecord.getMinBufferSize(sampleRateHz, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT)
                    if (minBufSize == AudioRecord.ERROR || minBufSize == AudioRecord.ERROR_BAD_VALUE) {
                        _status.value = _status.value.copy(isRunning = false, errorMsg = "麦克风不支持请求的采样率")
                        stopSelf()
                        return@launch
                    }
                    val bufferSize = (minBufSize * 2).coerceAtLeast((sampleRateHz / 1000) * 40)

                    // ═══ 音源选择：由 isVoiceMode（Opus信号类型）驱动 ═══
                    var audioSourceLabel: String
                    if (startIsVoiceMode) {
                        // 语音模式 → 尝试 VOICE_COMMUNICATION（系统原生硬件降噪）
                        try {
                            audioRecord = AudioRecord(
                                MediaRecorder.AudioSource.VOICE_COMMUNICATION,
                                sampleRateHz, AudioFormat.CHANNEL_IN_MONO,
                                AudioFormat.ENCODING_PCM_16BIT, bufferSize
                            )
                        } catch (_: Exception) {}

                        if (audioRecord?.state == AudioRecord.STATE_INITIALIZED) {
                            audioSourceLabel = AudioSourceLabel.SYSTEM_NS
                            Log.i(TAG, "音源: VOICE_COMMUNICATION（系统硬件降噪）")
                        } else {
                            // 异常回退：VOICE_COMMUNICATION 不可用 → 降级到 MIC
                            audioRecord?.release()
                            Log.w(TAG, "VOICE_COMMUNICATION 不可用，回退到 MIC 裸采集")
                            audioRecord = AudioRecord(
                                MediaRecorder.AudioSource.MIC,
                                sampleRateHz, AudioFormat.CHANNEL_IN_MONO,
                                AudioFormat.ENCODING_PCM_16BIT, bufferSize
                            )
                            audioSourceLabel = AudioSourceLabel.SYSTEM_NS_FALLBACK
                        }
                    } else {
                        // 全频模式 → MIC 裸麦克风（硬件降噪关闭）
                        audioRecord = AudioRecord(
                            MediaRecorder.AudioSource.MIC,
                            sampleRateHz, AudioFormat.CHANNEL_IN_MONO,
                            AudioFormat.ENCODING_PCM_16BIT, bufferSize
                        )
                        audioSourceLabel = AudioSourceLabel.MIC_RAW
                        Log.i(TAG, "音源: MIC（裸麦克风，硬件降噪关闭）")
                    }

                    if (audioRecord?.state != AudioRecord.STATE_INITIALIZED) {
                        _status.value = _status.value.copy(
                            isRunning = false,
                            errorMsg = "麦克风初始化失败",
                            audioSource = AudioSourceLabel.MIC_RAW
                        )
                        stopSelf()
                        return@launch
                    }

                    val readSize = frameSize * 2
                    audioRecord?.startRecording()

                    // ═══ 启用安卓原生 AEC（声学回声消除）═══
                    if (startIsVoiceMode && AcousticEchoCanceler.isAvailable()) {
                        try {
                            val aec = AcousticEchoCanceler.create(audioRecord!!.audioSessionId)
                            if (aec != null) {
                                aec.enabled = true
                                audioSourceLabel += AudioSourceLabel.AEC_ENABLED
                                Log.i(TAG, "AEC 已启用")
                            } else {
                                audioSourceLabel += AudioSourceLabel.AEC_UNAVAILABLE
                            }
                        } catch (e: Exception) {
                            audioSourceLabel += AudioSourceLabel.AEC_UNAVAILABLE
                            Log.w(TAG, "AEC create 失败", e)
                        }
                    } else {
                        audioSourceLabel += if (startIsVoiceMode) AudioSourceLabel.AEC_UNAVAILABLE else ""
                    }

                    // ═══ 初始化反向音频（PC→Phone 解码+播放，立体声）═══
                    if (reverseDecoder == null) {
                        // 首次创建解码器（模式切换时复用旧解码器）
                        reverseDecoder = ReverseOpusDecoder(sampleRateHz, channels = 2).also {
                            if (!it.start()) {
                                Log.w(TAG, "反向音频解码器初始化失败，继续运行")
                            }
                        }
                    }
                    // 每次采集启动都重建 AudioPlayer（确保路由/声道与新模式一致）
                    audioPlayer = AudioPlayer(this@CaptureService).also { player ->
                        val aecSessionId = (audioRecord?.audioSessionId ?: 0)
                        if (!player.start(aecSessionId, startIsVoiceMode)) {
                            Log.w(TAG, "反向音频播放器初始化失败，继续运行")
                        } else {
                            if (player.isVoiceCommActive) {
                                audioSourceLabel += " [AEC管线]"
                            }
                        }
                    }

                _status.value = _status.value.copy(
                    isRunning = true,
                    audioSource = audioSourceLabel,
                    connected = false,      // 尚未收到 CONNECT_ACK，不乐观标记连接
                    deviceId = Udp2CalProtocol.deviceIdToString(DiscoveryManager.getOrCreateDeviceId())
                )

                    // ── 音频帧通道（容量 3：无需无限增长，又提供足够并行度）──
                    val ch = Channel<ShortArray>(3)
                    audioChannel = ch // 保存引用给 stopCapture 关闭

                    // ── 乒乓发送缓冲区：2 个轮流使用，防止上一帧 send 未完成时被 encodeTo 覆盖 ──
                    val sendBuffers = arrayOf(
                        ByteArray(Udp2CalProtocol.MAX_PACKET),
                        ByteArray(Udp2CalProtocol.MAX_PACKET)
                    )
                    var bufIndex = 0

                    // ── ShortArray 复用池（容量 5，比 Channel 容量 3 大 2，彻底杜绝耗尽锁死风险）──
                    val framePool = ShortArrayPool(frameSize, 5)

                    // ═══ 生产者协程 ═══：仅做 AudioRecord.read + 帧组装
                    // Dispatchers.IO 确保硬件阻塞读不阻塞消费协程
                    val producerJob = launch(Dispatchers.IO) {
                        val pcmBuffer = ShortArray(readSize)
                        var accumPos = 0
                        var accumBuf = framePool.borrow() // 第一帧从池中借出

                        while (isActive) {
                            val read = audioRecord?.read(pcmBuffer, 0, readSize) ?: -1
                            if (read <= 0) continue

                            var srcPos = 0
                            while (srcPos < read) {
                                val n = minOf(read - srcPos, frameSize - accumPos)
                                pcmBuffer.copyInto(accumBuf, accumPos, srcPos, srcPos + n)
                                accumPos += n
                                srcPos += n

                                if (accumPos == frameSize) {
                                    // 直接将 accumBuf 送出（零拷贝），再从池中借出下一块
                                    ch.send(accumBuf)
                                    accumBuf = framePool.borrow()
                                    accumPos = 0
                                }
                            }
                        }
                    }

                    // ═══ 消费者协程 ═══：编码参数同步 → 编码 → 发送
                    val consumerJob = launch {
                        var currentIp = targetIp
                        var currentPort = targetPort
                        var byteCount = 0L
                        var lastReport = System.currentTimeMillis()
                        var lastOpusConfigHash = 0
                        var reconnectCounter = 0
                        val RECONNECT_INTERVAL = 50 // ~1秒(20ms每帧)
                        var p2pConnected = false // 初始未连接，收到 ACK 后方可发包
                        var reverseJob: Job? = null

                        // 模式重启时：如果有任何历史 ACK，立即恢复连接
                        if ((udpSender?.getLastAckTime() ?: 0L) > 0) {
                            p2pConnected = true
                            _status.value = _status.value.copy(connected = true, errorMsg = "")
                            reverseJob = launchReverseAudio()
                            reverseAudioJob = reverseJob
                        }

                        for (frame in ch) {
                            // ── 非阻塞漏极: 每帧检查 ACK ──
                            if (udpSender?.drainAck() == true && !p2pConnected) {
                                p2pConnected = true
                                _status.value = _status.value.copy(connected = true, errorMsg = "")
                                // 连接确认后启动反向音频协程（独立 socket，不干扰正向）
                                reverseJob = launchReverseAudio()
                                reverseAudioJob = reverseJob
                            }

                            // ── 保活 CONNECT：持续发送（Win 端对异设备静默拒绝，不影响已有连接）──
                            reconnectCounter++
                            if (reconnectCounter >= RECONNECT_INTERVAL) {
                                reconnectCounter = 0
                                try {
                                    val devId = DiscoveryManager.getOrCreateDeviceId()
                                    // 保活 CONNECT 携带反向端口，使 Win 重启后能正确恢复反向串流
                                    val revPort = udpSender?.reversePort ?: 0
                                    val payload = byteArrayOf((revPort shr 8).toByte(), revPort.toByte())
                                    val ping = Udp2CalProtocol.buildPacket(
                                        isAudio = false, msgType = Udp2CalProtocol.TYPE_CONNECT,
                                        sampleRate = 0, seqNum = 0, deviceId = devId, payload = payload
                                    )
                                    udpSender?.send(ping, 0, ping.size)
                                } catch (_: Exception) {}
                            }

                            // ── ACK 超时判定（优雅断连，不退出采集）──
                            val ackTime = udpSender?.getLastAckTime() ?: 0L
                            if (ackTime > 0 && p2pConnected) {
                                val elapsed = System.currentTimeMillis() - ackTime
                                if (elapsed > 3000) {
                                    p2pConnected = false
                                    reverseJob?.cancel()
                                    reverseJob = null
                                    _status.value = _status.value.copy(connected = false, reverseAudio = false, errorMsg = "已断开（等待重连）")
                                }
                            }

                            // ── 网络目标动态热重连 ──
                            val targetIpFromPrefs = Prefs.targetIp
                            val targetPortFromPrefs = Prefs.targetPort
                            if (targetIpFromPrefs != currentIp || targetPortFromPrefs != currentPort) {
                                try {
                                    Log.i(TAG, "📡 检测到 IP/端口 变更，执行热重连: $targetIpFromPrefs:$targetPortFromPrefs")
                                    udpSender?.close()
                                    udpSender = UdpSender(targetIpFromPrefs, targetPortFromPrefs)
                                    udpSender?.connect()
                                    currentIp = targetIpFromPrefs
                                    currentPort = targetPortFromPrefs
                                } catch (ex: Exception) {
                                    Log.e(TAG, "热重连失败", ex)
                                }
                            }

                            // ── 音源模式变更检测（isVoiceMode 驱动 AudioRecord 重建）──
                            val newIsVoiceMode = isVoiceMode()
                            if (newIsVoiceMode != startIsVoiceMode) {
                                Log.i(TAG, "🔄 Opus信号类型变更(isVoiceMode: $startIsVoiceMode→$newIsVoiceMode)，触发音频源重建")
                                isModeRestart = true
                                pendingRestart = CaptureParams(
                                    sampleRateHz, bitrateKbps,
                                    Prefs.targetIp, Prefs.targetPort, testTone
                                )
                                captureJob?.cancel()
                                return@launch
                            }

                            // ── Opus 编码参数毫秒级动态同步 ──
                            val curCplx = Prefs.opusComplexity
                            val curSig = Prefs.opusSignal
                            val curBw = Prefs.opusBandwidth
                            val curDtx = Prefs.opusDtx
                            val curVbr = if (curDtx == 1) 1 else Prefs.opusVbr
                            // 自动码率：48000→512k（OPUS 立体声协议上限），24000→256k，16000→128k
                            val curBr = if (Prefs.opusBitrateKbps > 0) Prefs.opusBitrateKbps else {
                                when (sampleRateHz) { 48000 -> 512; 24000 -> 256; 16000 -> 128; else -> 64 }
                            }
                            val curFec = Prefs.opusFec
                            val curPl = Prefs.opusPacketLoss
                            val curVbrc = if (curVbr == 1) Prefs.opusVbrConstraint else 0
                            val curHash = java.util.Objects.hash(curCplx, curSig, curBw, curDtx, curVbr, curBr, curFec, curPl, curVbrc)
                            if (curHash != lastOpusConfigHash) {
                                if (encoder?.update(curCplx, curSig, curBw, curDtx, curVbr, curBr, curFec, curPl, curVbrc) == true) {
                                    lastOpusConfigHash = curHash
                                    val mode = when {
                                        curDtx == 1 -> "VBR(DTX)"
                                        curVbr == 1 && curVbrc == 1 -> "VBR约束"
                                        curVbr == 1 -> "VBR"
                                        else -> "CBR"
                                    }
                                    _status.value = _status.value.copy(
                                        bitrateTargetKbps = curBr,
                                        vbrMode = mode,
                                        opusMode = if (isVoiceMode()) "语音模式" else "全频模式"
                                    )
                                }
                            }

                            // ── 编码 + 发送（断开时仅回收帧，不编不送）──
                            if (p2pConnected) {
                                val buf = sendBuffers[bufIndex]
                                bufIndex = (bufIndex + 1) % 2
                                val written = encoder?.encodeTo(frame, buf, 0) ?: -1
                                if (written > 0) {
                                    if (udpSender?.send(buf, 0, written) == true) {
                                        byteCount += written
                                    }
                                }
                            }

                            // ── 每秒状态汇报 ──
                            val now = System.currentTimeMillis()
                            if (now - lastReport >= 1000) {
                                val kbps = if (now - lastReport > 0) (byteCount * 8f) / (now - lastReport) else 0f
                                _status.value = _status.value.copy(bitrateKbps = kbps)
                                byteCount = 0
                                lastReport = now
                            }

                            framePool.recycle(frame)
                        }
                    }

                    // 等待生产者完成（被取消时），然后清理
                    try {
                        producerJob.join()
                    } finally {
                        consumerJob.cancel()
                        ch.close()
                        audioChannel = null
                    }
                }
            } catch (_: CancellationException) {
            } catch (e: Exception) {
                Log.e(TAG, "采集异常", e)
                _status.value = _status.value.copy(isRunning = false, errorMsg = e.message ?: "未知错误")
            } finally {
                // 确保干净释放
                try { audioRecord?.stop() } catch (_: Exception){}
                try { audioRecord?.release() } catch (_: Exception){}
                audioRecord = null
                encoder?.stop()
                encoder = null
                if (isModeRestart) {
                    // 模式切换：保留UdpSender和reverseDecoder，audioPlayer由doStartCapture重建
                    Log.i(TAG, "🔄 模式切换，保留UdpSender/reverseDecoder")
                    reverseAudioJob?.cancel()
                    audioPlayer?.stop()
                } else {
                    try { udpSender?.close() } catch (_: Exception){}
                    udpSender = null
                    reverseAudioJob?.cancel()
                    reverseDecoder?.stop()
                    audioPlayer?.stop()
                    reverseDecoder = null
                    audioPlayer = null
                }
                isModeRestart = false
                try { if (wakeLock.isHeld) wakeLock.release() } catch (_: Exception){}

                // ✅ 熔断保护：硬件初始化硬故障（麦克风被占用/不支持/编码器失败）时拒绝无限重启，防止 ANR
                if (_status.value.errorMsg.contains("麦克风初始化失败")
                    || _status.value.errorMsg.contains("麦克风不支持")
                    || _status.value.errorMsg.contains("编码器初始化失败")
                    || _status.value.errorMsg.contains("编码器帧大小无效")) {
                    Log.e(TAG, "🚨 发生硬件初始化硬故障，清空挂起任务，实施安全熔断。")
                    pendingRestart = null
                }

                val restart = pendingRestart
                pendingRestart = null
                if (restart != null) {
                    serviceScope.launch { doStartCapture(restart.sampleRateHz, restart.bitrateKbps, restart.targetIp, restart.targetPort, restart.testTone) }
                } else if (!hasNewCapture) {
                    _status.value = _status.value.copy(isRunning = false)
                    try { stopForeground(STOP_FOREGROUND_REMOVE) } catch (_: Exception){}
                    stopSelf()
                }
            }
        }
    }

    /** 启动独立协程，在反向 socket 上接收 PC→Phone 音频 */
    private fun launchReverseAudio(): Job {
        return serviceScope.launch(Dispatchers.IO) {
            val revSock = udpSender?.reverseSock ?: return@launch
            val dec = reverseDecoder ?: return@launch
            val player = audioPlayer ?: return@launch
            if (dec.frameSize <= 0) return@launch

            val recvBuf = ByteArray(Udp2CalProtocol.MAX_PACKET)
            val pkt = DatagramPacket(recvBuf, recvBuf.size)
            val pcmBuf = ShortArray(dec.pcmBufferSize)
            var hasAudio = false
            var lastAudioTime = 0L
            var byteCount = 0L
            var lastReport = System.currentTimeMillis()
            var bandwidthLabel = ""

            Log.i(TAG, "反向音频接收器已启动")

            while (isActive) {
                try {
                    revSock.soTimeout = 1000
                    pkt.length = recvBuf.size
                    revSock.receive(pkt)
                    val len = pkt.length
                    if (len < Udp2CalProtocol.HEADER_SIZE) continue

                    val hdr = Udp2CalProtocol.decodeHeader(
                        recvBuf.copyOfRange(0, len.coerceAtMost(Udp2CalProtocol.HEADER_SIZE))
                    ) ?: continue

                    if (!hdr.isAudio || hdr.payloadLen <= 0) continue

                    val plen = hdr.payloadLen.coerceAtMost(len - Udp2CalProtocol.HEADER_SIZE)
                    val ns = dec.decode(recvBuf, Udp2CalProtocol.HEADER_SIZE, plen, pcmBuf)
                    if (ns > 0) {
                        player.write(pcmBuf, 0, ns)
                        lastAudioTime = System.currentTimeMillis()
                        byteCount += len

                        if (!hasAudio) {
                            hasAudio = true
                            // 从 header 估算带宽
                            bandwidthLabel = estimateBandwidth(hdr.sampleRate)
                            _status.value = _status.value.copy(
                                reverseAudio = true,
                                reverseBw = bandwidthLabel
                            )
                        }
                    }

                    // 每秒更新反向码率
                    val now = System.currentTimeMillis()
                    if (now - lastReport >= 1000 && hasAudio) {
                        val kbps = (byteCount * 8f) / (now - lastReport)
                        _status.value = _status.value.copy(reverseBitrateKbps = kbps)
                        byteCount = 0
                        lastReport = now
                    }
                } catch (_: java.net.SocketTimeoutException) {
                    // 3 秒无反向音频 → 同步断开状态
                    if (hasAudio && System.currentTimeMillis() - lastAudioTime > 3000) {
                        hasAudio = false
                        _status.value = _status.value.copy(reverseAudio = false)
                    }
                    continue
                } catch (_: Exception) {
                    if (!isActive) break
                }
            }

            Log.i(TAG, "反向音频接收器已停止")
        }
    }

    /** 从 Opus 采样率编码推断带宽标签 */
    private fun estimateBandwidth(sampleRateId: Byte): String {
        val hz = Udp2CalProtocol.sampleRateToHz(sampleRateId)
        // PC 端始终发送立体声，固定附加 "立体声" 标识
        val base = when (hz) {
            8000 -> "NB 8kHz"
            12000 -> "MB 12kHz"
            16000 -> "WB 16kHz"
            24000 -> "SWB 24kHz"
            48000 -> "FB 48kHz"
            else -> "${hz / 1000}kHz"
        }
        return "$base 立体声"
    }

    fun stopCapture() {
        pendingRestart = null
        hasNewCapture = false
        // 停止后接收端依赖 3 秒超时自动释放 P2P 独占连接
        // 必须显式关闭 channel，防止消费者在 receive() 挂起无法退出
        audioChannel?.close()
        audioChannel = null
        captureJob?.cancel()
        captureJob = null
        reverseAudioJob?.cancel()
        reverseAudioJob = null
        _status.value = _status.value.copy(connected = false, reverseAudio = false)
    }
    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel("udp2cal_capture", "采集服务", NotificationManager.IMPORTANCE_LOW).apply { setShowBadge(false) }
            )
        }
    }
    private fun buildNotification(): Notification {
        val status = _status.value
        // 通知栏摘要（保持简洁，不显示实时码率）
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
        reverseDecoder?.stop()
        audioPlayer?.stop()
        serviceScope.cancel()
        super.onDestroy()
    }

    /**
     * 固定容量 ShortArray 对象复用池
     * 生产者从池中借出 → 填充 → 送 Channel → 消费者处理完 → 归还
     * 彻底消除每帧 ShortArray(960) 的堆分配
     */
    private class ShortArrayPool(val frameSize: Int, val capacity: Int = 3) {
        private val pool = arrayOfNulls<ShortArray>(capacity)
        private var head = 0
        private var count = 0

        /** 从池中借出缓冲区（池空时分配新的，仅初始热身期触发分配） */
        @Synchronized
        fun borrow(): ShortArray {
            return if (count > 0) {
                val idx = head
                head = (head + 1) % capacity
                count--
                pool[idx]!!
            } else {
                ShortArray(frameSize) // 池空时创建新对象
            }
        }

        /** 消费者处理完成后归还缓冲区 */
        @Synchronized
        fun recycle(buf: ShortArray) {
            val tail = (head + count) % capacity
            if (count < capacity) {
                pool[tail] = buf
                count++
            }
            // 池满则静默丢弃（让 GC 回收）
        }
    }
}