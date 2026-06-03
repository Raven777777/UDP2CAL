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
import com.udp2mic.app.Udp2MicProtocol
import com.udp2mic.app.UdpSender
import com.udp2mic.app.native.OpusEncoder
import android.media.audiofx.NoiseSuppressor
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlin.math.*

class CaptureService : Service() {
    companion object {
        private const val TAG = "CaptureService"
    }

    private val binder = LocalBinder()
    private val serviceScope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    private var captureJob: Job? = null

    private val _status = MutableStateFlow(CaptureStatus())
    val status: StateFlow<CaptureStatus> = _status.asStateFlow()

    private var audioRecord: AudioRecord? = null
    private var encoder: OpusEncoder? = null
    private var udpSender: UdpSender? = null
    private var noiseSuppressor: NoiseSuppressor? = null
    private var restarting = false

    // ── 音频处理状态 ──
    private var agcSmoothedRms = 0.008f  // EMA 平滑 RMS (线性, 0~1)
    private var agcCurrentGain = 10.0f   // 当前应用增益 (初始10x ≈ 20dB)
    private var agcTargetRms = 0.15f     // 目标 RMS ≈ -16.5dBFS (人声明亮区间)
    private var agcMinGain = 2.0f        // 最小增益
    private var agcMaxGain = 40.0f       // 最大增益 (UNPROCESSED 源极弱时可达32dB)
    // AGC 快攻/慢放: 慢放更慢让声音更自然
    private val agcAttackAlpha = 0.5f    // 攻击速度
    private val agcReleaseAlpha = 0.04f  // 释放速度 (慢, 避免呼吸感)

    // ── 噪声门状态 ──
    private var ngNoiseFloor = 0.002f
    private var ngAttenuation = 1.0f
    private var ngHoldCounter = 0
    private val ngHoldFrames = 30
    private val ngThresholdMul = 3.5f
    private var ngFloorFrozen = false
    private var ngSilenceTimer = 0

    // ── 人声增强 (Time-Domain Wiener 降噪, 持续去除背景杂音) ──
    private var veNoiseEnvelope = 0.001f   // 噪声 RMS 包络 (慢跟踪)
    private var veSignalEnvelope = 0.001f  // 信号 RMS 包络 (快跟踪)
    private var veLearnFrames = 80         // 启动后学习噪声 ~1.6s
    private var veEnabled = false

    data class CaptureStatus(
        val isRunning: Boolean = false,
        val isConnected: Boolean = false,
        val bitrateKbps: Float = 0f,
        val sampleRateHz: Int = 0,
        val bitrateTargetKbps: Int = 0,
        val errorMsg: String = "",
        val nsActive: Boolean = false,
        val agcGainDb: Float = 0f,
        val veActive: Boolean = false
    )

    inner class LocalBinder : Binder() {
        fun getService(): CaptureService = this@CaptureService
    }

    override fun onBind(intent: Intent?): IBinder = binder

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
    }

    fun startCapture(sampleRateHz: Int, bitrateKbps: Int, targetIp: String, targetPort: Int, testTone: Boolean = false, noiseReduction: Boolean = false, voiceEnhance: Boolean = false) {
        if (captureJob?.isActive == true) {
            restarting = true
            captureJob?.cancel()
            serviceScope.launch {
                delay(500)
                restarting = false
                startCapture(sampleRateHz, bitrateKbps, targetIp, targetPort, testTone, noiseReduction, voiceEnhance)
            }
            return
        }

        val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
        val wakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "UDP2Mic:Capture")
        try { wakeLock.acquire(10 * 60 * 1000L) } catch (_: Exception) {}

        try {
            startForeground(1, buildNotification())
        } catch (e: Exception) {
            _status.value = _status.value.copy(errorMsg = "前台服务: ${e.message}")
            return
        }

        captureJob = serviceScope.launch {
            _status.value = _status.value.copy(isRunning = true, errorMsg = "")

            // === 通用: 连接 UDP + 创建编码器 ===
            try {
                udpSender = UdpSender(targetIp, targetPort)
                if (!udpSender!!.connect()) {
                    _status.value = _status.value.copy(isRunning = false, errorMsg = "网络连接失败")
                    stopSelf()
                    return@launch
                }

                encoder = OpusEncoder(sampleRateHz, bitrateKbps)
                if (!encoder!!.start()) {
                    _status.value = _status.value.copy(isRunning = false, errorMsg = "编码器初始化失败")
                    stopSelf()
                    return@launch
                }

                val frameSize = encoder!!.frameSize
                if (frameSize <= 0) {
                    _status.value = _status.value.copy(isRunning = false, errorMsg = "编码器帧大小无效")
                    stopSelf()
                    return@launch
                }

                val srId = encoder!!.sampleRateId
                val srName = when (srId.toInt()) {
                    Udp2MicProtocol.SAMPLE_RATE_8K.toInt() -> "8kHz"
                    Udp2MicProtocol.SAMPLE_RATE_12K.toInt() -> "12kHz"
                    Udp2MicProtocol.SAMPLE_RATE_16K.toInt() -> "16kHz"
                    Udp2MicProtocol.SAMPLE_RATE_24K.toInt() -> "24kHz"
                    Udp2MicProtocol.SAMPLE_RATE_48K.toInt() -> "48kHz"
                    else -> "${sampleRateHz}Hz"
                }
                Log.i(TAG, "启动: sr=${sampleRateHz}Hz($srName) br=${bitrateKbps}kbps testTone=$testTone frameSize=$frameSize")

                _status.value = _status.value.copy(
                    sampleRateHz = sampleRateHz,
                    bitrateTargetKbps = bitrateKbps
                )

                var byteCount = 0L
                var lastReport = System.currentTimeMillis()
                var readTimeTotal = 0L
                var readTimeCount = 0L
                var readSamplesTotal = 0L
                var readOps = 0L

                if (testTone) {
                    // === H7: 1kHz 正弦波测试音 (验证编解码链路频率完整性) ===
                    val omega = 2.0 * Math.PI * 1000.0 / sampleRateHz  // 1kHz 角频率
                    val testToneBuf = ShortArray(frameSize)
                    var phase = 0.0
                    val frameIntervalNs = (frameSize * 1_000_000_000L) / sampleRateHz  // 960*1e9/48000 = 20ms
                    var nextFrameTime = System.nanoTime()
                    Log.i(TAG, "🎵 测试音模式: 1kHz @ ${sampleRateHz}Hz frameSize=$frameSize interval=${frameIntervalNs/1_000_000}ms")

                    _status.value = _status.value.copy(isRunning = true, isConnected = true)

                    while (isActive) {
                        // 生成一帧 1kHz 正弦波 (-6dB amplitude)
                        for (i in 0 until frameSize) {
                            val sample = (Math.sin(phase) * 16384.0).toInt().coerceIn(-32768, 32767)
                            testToneBuf[i] = sample.toShort()
                            phase += omega
                            if (phase > 2.0 * Math.PI) phase -= 2.0 * Math.PI
                        }

                        encoder?.encode(testToneBuf)?.let { packet ->
                            if (udpSender?.send(packet) == true) {
                                byteCount += packet.size
                            }
                        }

                        // ★ 精确等待到下一帧 deadline (nanoTime补偿)
                        nextFrameTime += frameIntervalNs
                        var nsNow: Long
                        do {
                            nsNow = System.nanoTime()
                            if (nsNow >= nextFrameTime) break
                            val remainingUs = (nextFrameTime - nsNow) / 1000
                            if (remainingUs > 2000) {  // >2ms: 用sleep
                                Thread.sleep(remainingUs / 1000 - 1)
                            }
                            // <2ms: 忙等 (nanobusy)
                        } while (System.nanoTime() < nextFrameTime)
                        // 如果落后了，跳过追赶避免积累
                        if (nsNow - nextFrameTime > frameIntervalNs) {
                            Log.w(TAG, "测试音掉帧: behind=${(nsNow-nextFrameTime)/1_000_000}ms")
                            nextFrameTime = nsNow
                        }

                        val msNow = System.currentTimeMillis()
                        val elapsed = msNow - lastReport
                        if (elapsed >= 1000) {
                            val kbps = if (elapsed > 0) (byteCount * 8f) / elapsed * 1000f / 1000f else 0f
                            val encCount = encoder?.encodeCount ?: 0
                            Log.d(TAG, "🔊 测试音 sr=${srName} br=${kbps.toInt()}kbps frames=$encCount")
                            _status.value = _status.value.copy(bitrateKbps = kbps)
                            byteCount = 0
                            lastReport = msNow
                        }
                    }

                } else {
                    // === 正常麦克风采集 ===
                    val minBufSize = AudioRecord.getMinBufferSize(
                        sampleRateHz, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT
                    )
                    if (minBufSize == AudioRecord.ERROR || minBufSize == AudioRecord.ERROR_BAD_VALUE) {
                        _status.value = _status.value.copy(isRunning = false, errorMsg = "麦克风不支持请求的采样率")
                        stopSelf()
                        return@launch
                    }
                    val bufferSize = (minBufSize * 2).coerceAtLeast((sampleRateHz / 1000) * 40)

                    // H5: 优先使用 UNPROCESSED 源 (API 26+, 绕过手机AGC/降噪/DSP)
                    var sourceLabel = "MIC"
                    try {
                        val ar = AudioRecord(
                            MediaRecorder.AudioSource.UNPROCESSED, sampleRateHz,
                            AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT, bufferSize
                        )
                        if (ar.state == AudioRecord.STATE_INITIALIZED) {
                            audioRecord = ar
                            sourceLabel = "UNPROCESSED"
                            Log.i(TAG, "使用 UNPROCESSED 音频源 (绕过手机DSP)")
                        } else {
                            ar.release()
                        }
                    } catch (_: Exception) { }

                    if (audioRecord == null) {
                        Log.w(TAG, "UNPROCESSED 不可用, 回退到 MIC")
                        try {
                            audioRecord = AudioRecord(
                                MediaRecorder.AudioSource.MIC, sampleRateHz,
                                AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT, bufferSize
                            )
                        } catch (e: IllegalArgumentException) {
                            _status.value = _status.value.copy(isRunning = false, errorMsg = "麦克风权限未授予")
                            stopSelf()
                            return@launch
                        } catch (e: Exception) {
                            _status.value = _status.value.copy(isRunning = false, errorMsg = "麦克风: ${e.message}")
                            stopSelf()
                            return@launch
                        }
                    }

                    if (audioRecord?.state != AudioRecord.STATE_INITIALIZED) {
                        _status.value = _status.value.copy(isRunning = false, errorMsg = "麦克风初始化失败")
                        stopSelf()
                        return@launch
                    }

                    val actualSampleRateHz = audioRecord?.sampleRate ?: sampleRateHz
                    if (actualSampleRateHz != sampleRateHz) {
                        Log.w(TAG, "⚠️ 采样率不匹配! 请求=${sampleRateHz}Hz 实际=${actualSampleRateHz}Hz — 重建编码器")
                        encoder?.stop()
                        encoder = OpusEncoder(actualSampleRateHz, bitrateKbps)
                        if (!encoder!!.start()) {
                            _status.value = _status.value.copy(isRunning = false, errorMsg = "编码器重建失败")
                            stopSelf()
                            return@launch
                        }
                    }
                    Log.i(TAG, "音频源: $sourceLabel sr=$actualSampleRateHz bufSize=$bufferSize")

                    // ── 降噪: Android NoiseSuppressor (硬件加速, 针对人声语音) ──
                    var nsActive = false
                    if (noiseReduction) {
                        try {
                            val sessionId = audioRecord?.audioSessionId ?: 0
                            val ns = NoiseSuppressor.create(sessionId)
                            if (ns != null) {
                                ns.enabled = true
                                noiseSuppressor = ns
                                nsActive = true
                                Log.i(TAG, "✓ NoiseSuppressor 已激活 (audioSession=$sessionId)")
                            } else {
                                Log.w(TAG, "NoiseSuppressor 不可用 (设备不支持), 回退软件噪声门")
                            }
                        } catch (e: Exception) {
                            Log.w(TAG, "NoiseSuppressor 创建失败: ${e.message}, 回退软件噪声门")
                        }
                    }
                    _status.value = _status.value.copy(nsActive = nsActive)

                    // ── 人声增强: 学习阶段标记 ──
                    veEnabled = voiceEnhance
                    veNoiseEnvelope = 0.001f
                    veSignalEnvelope = 0.001f
                    veLearnFrames = if (voiceEnhance) 80 else 0
                    _status.value = _status.value.copy(veActive = voiceEnhance)

                    // ── 重置 AGC / Noise Gate 状态 ──
                    agcSmoothedRms = 0.008f
                    agcCurrentGain = 10.0f
                    ngNoiseFloor = 0.002f
                    ngAttenuation = 1.0f
                    ngHoldCounter = 0
                    ngFloorFrozen = false
                    ngSilenceTimer = 0

                    val readSize = frameSize * 2
                    audioRecord?.startRecording()

                    val pcmBuffer = ShortArray(readSize)
                    val pcmAccum = ShortArray(frameSize)
                    var accumPos = 0
                    var dropCount = 0
                    var totalTimerCounter = 0L
                    var firstReadTime = 0L

                    _status.value = _status.value.copy(isRunning = true, isConnected = true)

                    while (isActive) {
                        val readStart = System.nanoTime()
                        val read = audioRecord?.read(pcmBuffer, 0, readSize) ?: -1
                        val readDur = System.nanoTime() - readStart

                        if (read <= 0) { dropCount++; continue }

                        readTimeTotal += readDur
                        readTimeCount++
                        readSamplesTotal += read
                        readOps++
                        if (firstReadTime == 0L) firstReadTime = readStart

                        // ★ 审计: AudioRecord.read() 返回样本数
                        if (read != frameSize && read != readSize) {
                            Log.w(TAG, "⚠️ AudioRecord.read()=$read (期望=$frameSize 或 $readSize)")
                        }

                        var srcPos = 0
                        while (srcPos < read) {
                            val n = minOf(read - srcPos, frameSize - accumPos)
                            pcmBuffer.copyInto(pcmAccum, accumPos, srcPos, srcPos + n)
                            accumPos += n
                            srcPos += n

                            if (accumPos == frameSize) {
                                // ── 步骤1: 计算帧 RMS (归一化 0~1) ──
                                var sumSq = 0L
                                for (s in pcmAccum) { val v = s.toInt(); sumSq += (v * v).toLong() }
                                val frameRmsLinear = sqrt(sumSq.toDouble() / frameSize).toFloat() / 32768.0f

                                // ── 步骤2: 自适应 AGC (慢跟踪 RMS → 快攻击/慢释放增益) ──
                                agcSmoothedRms = agcSmoothedRms * 0.82f + frameRmsLinear * 0.18f
                                val targetGain = (agcTargetRms / (agcSmoothedRms + 1e-6f))
                                    .coerceIn(agcMinGain, agcMaxGain)
                                val alpha = if (targetGain > agcCurrentGain) agcAttackAlpha else agcReleaseAlpha
                                agcCurrentGain = agcCurrentGain * (1f - alpha) + targetGain * alpha

                                // ── 步骤3: 软件噪声门 (改进: 噪声底冻结 + 高阈值 + 长保持) ──
                                if (noiseReduction) {
                                    val threshold = ngNoiseFloor * ngThresholdMul

                                    if (frameRmsLinear > threshold) {
                                        // 有语音: 冻结噪声底, 开门
                                        ngFloorFrozen = true
                                        ngHoldCounter = ngHoldFrames
                                        ngSilenceTimer = 0
                                        ngAttenuation += (1.0f - ngAttenuation) * 0.35f  // 平稳开门
                                    } else {
                                        // 无声区间
                                        if (ngHoldCounter > 0) {
                                            ngHoldCounter--
                                        } else {
                                            // hold 耗尽后才开始关
                                            ngAttenuation += (0.0f - ngAttenuation) * 0.04f  // 慢关(很慢)
                                        }
                                        ngSilenceTimer++
                                        // 连续 2 秒真静音后才解冻噪声底
                                        if (ngSilenceTimer > 100) {
                                            ngFloorFrozen = false
                                        }
                                    }

                                    // 噪声底自适应: 仅静音且解冻时更新
                                    if (!ngFloorFrozen && frameRmsLinear < ngNoiseFloor * 2.5f) {
                                        ngNoiseFloor = ngNoiseFloor * 0.97f + frameRmsLinear * 0.03f
                                    }
                                } else {
                                    ngAttenuation = 1.0f
                                }

                                // ── 步骤3.5: 人声增强 (Wiener降噪, 持续去除背景杂音突出人声) ──
                                if (veEnabled) {
                                    if (veLearnFrames > 0) {
                                        // 快速学习初始噪声底
                                        veNoiseEnvelope = veNoiseEnvelope * 0.88f + frameRmsLinear * 0.12f
                                        veLearnFrames--
                                    } else {
                                        // 慢更新噪声底 (仅当信号弱时)
                                        if (frameRmsLinear < veNoiseEnvelope * 2.5f) {
                                            veNoiseEnvelope = veNoiseEnvelope * 0.995f + frameRmsLinear * 0.005f
                                        }
                                        // 快跟踪信号包络
                                        veSignalEnvelope = veSignalEnvelope * 0.65f + frameRmsLinear * 0.35f

                                        // Wiener 增益: G = SNR² / (SNR² + 1)
                                        val snr = (veSignalEnvelope / (veNoiseEnvelope + 1e-6f)).coerceIn(0.1f, 50f)
                                        val wienerGain = (snr * snr) / (snr * snr + 1f)
                                        // 增益下限 0.2 (-14dB), 确保微弱人声仍有留存
                                        val veGain = wienerGain.coerceIn(0.2f, 1.0f)

                                        for (i in pcmAccum.indices) {
                                            val reduced = (pcmAccum[i] * veGain).toInt().coerceIn(-32768, 32767)
                                            pcmAccum[i] = reduced.toShort()
                                        }
                                    }
                                }

                                // ── 步骤4: 应用 AGC × Noise Gate 总增益 ──
                                val totalGain = agcCurrentGain * ngAttenuation
                                for (i in pcmAccum.indices) {
                                    val amplified = (pcmAccum[i] * totalGain).toInt().coerceIn(-32768, 32767)
                                    pcmAccum[i] = amplified.toShort()
                                }

                                encoder?.encode(pcmAccum)?.let { packet ->
                                    if (udpSender?.send(packet) == true) {
                                        byteCount += packet.size
                                    }
                                }
                                accumPos = 0
                            }
                        }

                        val now = System.currentTimeMillis()
                        val elapsed = now - lastReport
                        if (elapsed >= 1000) {
                            val kbps = if (elapsed > 0) (byteCount * 8f) / elapsed * 1000f / 1000f else 0f
                            val encCount = encoder?.encodeCount ?: 0
                            val encBytes = encoder?.totalEncodedBytes ?: 0

                            // ★ 时钟漂移审计: 比较实际经过时间 vs 期望采集的样本量
                            val elapsedNs = System.nanoTime() - firstReadTime
                            val expectedSamples = (elapsedNs * sampleRateHz / 1_000_000_000L)
                            val samplesDelta = readSamplesTotal - expectedSamples
                            val readAvgUs = if (readTimeCount > 0) readTimeTotal / readTimeCount / 1000 else 0

                            Log.d(TAG, "sr=${srName} br=${kbps.toInt()}kbps frames=$encCount drop=$dropCount clockDrift=${samplesDelta}smpl readAvg=${readAvgUs}us")

                            // 每分钟报告时钟漂移
                            totalTimerCounter++
                            if (totalTimerCounter % 60 == 0L) {
                                val driftMs = samplesDelta * 1000 / sampleRateHz
                                Log.i(TAG, "⏰ 时钟漂移审计: ${samplesDelta}样本 ≈ ${driftMs}ms 累积 (${encCount}帧)")
                            }

                            _status.value = _status.value.copy(bitrateKbps = kbps, agcGainDb = 20f * log10(agcCurrentGain.toDouble().coerceAtLeast(1e-6)).toFloat())
                            byteCount = 0
                            lastReport = now
                        }
                    }
                }

            } catch (_: CancellationException) {
            } catch (e: Exception) {
                Log.e(TAG, "采集异常", e)
                _status.value = _status.value.copy(isRunning = false, errorMsg = e.message ?: "未知错误")
            } finally {
                try { noiseSuppressor?.enabled = false } catch (_: Exception) {}
                try { noiseSuppressor?.release() } catch (_: Exception) {}
                noiseSuppressor = null
                try { audioRecord?.stop() } catch (_: Exception) {}
                try { audioRecord?.release() } catch (_: Exception) {}
                audioRecord = null
                encoder?.stop()
                encoder = null
                try { udpSender?.close() } catch (_: Exception) {}
                udpSender = null
                try { if (wakeLock.isHeld) wakeLock.release() } catch (_: Exception) {}
                if (!restarting) {
                    _status.value = _status.value.copy(isRunning = false, isConnected = false)
                    try { stopForeground(STOP_FOREGROUND_REMOVE) } catch (_: Exception) {}
                    stopSelf()
                }
            }
        }
    }

    fun stopCapture() {
        restarting = false
        captureJob?.cancel()
        captureJob = null
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel("udp2mic_capture", "采集服务", NotificationManager.IMPORTANCE_LOW)
                    .apply { description = "音频采集服务运行中"; setShowBadge(false) }
            )
        }
    }

    private fun buildNotification(): Notification =
        NotificationCompat.Builder(this, "udp2mic_capture")
            .setContentTitle("UDP2Mic 采集运行中")
            .setContentText("正在采集并发送音频...")
            .setSmallIcon(android.R.drawable.ic_btn_speak_now)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .build()

    override fun onDestroy() {
        stopCapture()
        serviceScope.cancel()
        super.onDestroy()
    }
}
