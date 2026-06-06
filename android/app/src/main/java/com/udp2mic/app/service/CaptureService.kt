package com.udp2mic.app.service

import android.app.*
import android.content.Context
import android.content.Intent
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.os.Binder
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import com.udp2mic.app.Prefs
import com.udp2mic.app.Udp2MicProtocol
import com.udp2mic.app.UdpSender
import com.udp2mic.app.native.OpusEncoder
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
    private var hasNewCapture = false

    private val _status = MutableStateFlow(CaptureStatus())
    val status: StateFlow<CaptureStatus> = _status.asStateFlow()

    private var audioRecord: AudioRecord? = null
    private var encoder: OpusEncoder? = null
    private var udpSender: UdpSender? = null
    /** 音频帧通道引用，用于 stopCapture 时强制关闭 */
    private var audioChannel: Channel<ShortArray>? = null

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
    }

    data class CaptureStatus(
        val isRunning: Boolean = false,
        val bitrateKbps: Float = 0f,
        val sampleRateHz: Int = 0,
        val bitrateTargetKbps: Int = 0,
        val vbrMode: String = "",                // "CBR" / "VBR" / "VBR约束" / "VBR(DTX)"
        val audioSource: String = "",            // 当前音源描述
        val opusMode: String = "",               // "语音模式" / "全频模式"
        val errorMsg: String = ""
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
        val wakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "UDP2Mic:Capture")
        try { wakeLock.acquire(10 * 60 * 1000L) } catch (_: Exception) {}

        hasNewCapture = true
        // 捕获启动时的 isVoiceMode，用于消费者协程检测变更
        val startIsVoiceMode = isVoiceMode()

        captureJob = serviceScope.launch {
            _status.value = _status.value.copy(isRunning = true, errorMsg = "")

            try {
                udpSender = UdpSender(targetIp, targetPort)
                if (!udpSender!!.connect()) {
                    _status.value = _status.value.copy(isRunning = false, errorMsg = "网络连接失败")
                    stopSelf()
                    return@launch
                }

                encoder = OpusEncoder(sampleRateHz, bitrateKbps)
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

                    val sendBuffer = ByteArray(Udp2MicProtocol.MAX_PACKET)

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

                    _status.value = _status.value.copy(
                        isRunning = true,
                        audioSource = audioSourceLabel
                    )

                    // ── 音频帧通道（容量 3：无需无限增长，又提供足够并行度）──
                    val ch = Channel<ShortArray>(3)
                    audioChannel = ch // 保存引用给 stopCapture 关闭

                    // ── 乒乓发送缓冲区：2 个轮流使用，防止上一帧 send 未完成时被 encodeTo 覆盖 ──
                    val sendBuffers = arrayOf(
                        ByteArray(Udp2MicProtocol.MAX_PACKET),
                        ByteArray(Udp2MicProtocol.MAX_PACKET)
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
                    // 与生产者在两个独立协程并行执行，流水线消除 IO 阻塞间隙
                    val consumerJob = launch {
                        var currentIp = targetIp
                        var currentPort = targetPort
                        var byteCount = 0L
                        var lastReport = System.currentTimeMillis()
                        var lastOpusConfigHash = 0

                        for (frame in ch) {
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

                            // ── 编码 + 发送 ──
                            val buf = sendBuffers[bufIndex]
                            bufIndex = (bufIndex + 1) % 2
                            val written = encoder?.encodeTo(frame, buf, 0) ?: -1
                            if (written > 0) {
                                if (udpSender?.send(buf, 0, written) == true) byteCount += written
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

                    // 等待生产者完成（被取消时），然后清理消费者
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
                try { udpSender?.close() } catch (_: Exception){}
                udpSender = null
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

    fun stopCapture() {
        pendingRestart = null
        hasNewCapture = false
        // 必须显式关闭 channel，防止消费者在 receive() 挂起无法退出
        audioChannel?.close()
        audioChannel = null
        captureJob?.cancel()
        captureJob = null
    }
    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel("udp2mic_capture", "采集服务", NotificationManager.IMPORTANCE_LOW).apply { setShowBadge(false) }
            )
        }
    }
    private fun buildNotification(): Notification = NotificationCompat.Builder(this, "udp2mic_capture").setContentTitle("UDP2Mic 采集运行中").setSmallIcon(android.R.drawable.ic_btn_speak_now).setOngoing(true).build()
    override fun onDestroy() { stopCapture(); serviceScope.cancel(); super.onDestroy() }

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