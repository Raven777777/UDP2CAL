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
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlin.math.*

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

    private data class CaptureParams(
        val sampleRateHz: Int, val bitrateKbps: Int, val targetIp: String, val targetPort: Int,
        val testTone: Boolean, val noiseGate: Boolean
    )
    private var pendingRestart: CaptureParams? = null

    // ── AGC 状态 ──
    private var agcSmoothedRms = 0.01f
    private var agcCurrentGain = 1.0f       
    private val agcTargetRms = 0.12f        
    private val agcMinGain = 1.0f           
    private var lastAgcEnabled = true       

    // ── 噪声门状态 ──
    private var ngNoiseFloor = 0.002f
    private var ngAttenuation = 1.0f
    private var ngHoldCounter = 0
    private val ngHoldFrames = 30
    private val ngThresholdMul = 3.5f
    private var ngFloorFrozen = false
    private var ngSilenceTimer = 0

    data class CaptureStatus(
        val isRunning: Boolean = false,
        val isConnected: Boolean = false,
        val bitrateKbps: Float = 0f,
        val sampleRateHz: Int = 0,
        val bitrateTargetKbps: Int = 0,
        val errorMsg: String = "",
        val agcGainDb: Float = 0f,
        val agcGainX: Float = 0f,
        val ngActive: Boolean = false
    )

    inner class LocalBinder : Binder() { fun getService(): CaptureService = this@CaptureService }
    override fun onBind(intent: Intent?): IBinder = binder
    override fun onCreate() { super.onCreate() ; createNotificationChannel() }

    fun startCapture(sampleRateHz: Int, bitrateKbps: Int, targetIp: String, targetPort: Int,
                     testTone: Boolean = false, noiseGate: Boolean = true) {
        if (captureJob?.isActive == true) {
            // 如果只是网络地址变了，我们可以动态处理，此处仅针对硬参数变更做物理重启
            pendingRestart = CaptureParams(sampleRateHz, bitrateKbps, targetIp, targetPort, testTone, noiseGate)
            captureJob?.cancel()
            return
        }
        pendingRestart = null
        doStartCapture(sampleRateHz, bitrateKbps, targetIp, targetPort, testTone, noiseGate)
    }

    private fun doStartCapture(
        sampleRateHz: Int, bitrateKbps: Int, targetIp: String, targetPort: Int,
        testTone: Boolean, noiseGate: Boolean
    ) {
        val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
        val wakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "UDP2Mic:Capture")
        try { wakeLock.acquire(10 * 60 * 1000L) } catch (_: Exception) {}

        try { startForeground(1, buildNotification()) } catch (e: Exception) {
            _status.value = _status.value.copy(errorMsg = "前台服务: ${e.message}")
            return
        }

        hasNewCapture = true
        captureJob = serviceScope.launch {
            _status.value = _status.value.copy(isRunning = true, errorMsg = "")

            try {
                var currentIp = targetIp
                var currentPort = targetPort
                
                udpSender = UdpSender(currentIp, currentPort)
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

                _status.value = _status.value.copy(ngActive = noiseGate, sampleRateHz = sampleRateHz, bitrateTargetKbps = bitrateKbps)
                var byteCount = 0L
                var lastReport = System.currentTimeMillis()
                var readSamplesTotal = 0L
                var firstReadTime = 0L
                var lastOpusConfigHash = 0

                if (testTone) {
                    val omega = 2.0 * Math.PI * 1000.0 / sampleRateHz
                    val testToneBuf = ShortArray(frameSize)
                    var phase = 0.0
                    val frameIntervalNs = (frameSize * 1_000_000_000L) / sampleRateHz
                    var nextFrameTime = System.nanoTime()

                    _status.value = _status.value.copy(isRunning = true, isConnected = true)

                    while (isActive) {
                        for (i in 0 until frameSize) {
                            val sample = (Math.sin(phase) * 16384.0).toInt().coerceIn(-32768, 32767)
                            testToneBuf[i] = sample.toShort()
                            phase += omega
                            if (phase > 2.0 * Math.PI) phase -= 2.0 * Math.PI
                        }

                        encoder?.encode(testToneBuf)?.let { packet ->
                            if (udpSender?.send(packet) == true) byteCount += packet.size
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
                    // === 正常麦克风采集 ===
                    val minBufSize = AudioRecord.getMinBufferSize(sampleRateHz, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT)
                    if (minBufSize == AudioRecord.ERROR || minBufSize == AudioRecord.ERROR_BAD_VALUE) {
                        _status.value = _status.value.copy(isRunning = false, errorMsg = "麦克风不支持请求的采样率")
                        stopSelf()
                        return@launch
                    }
                    val bufferSize = (minBufSize * 2).coerceAtLeast((sampleRateHz / 1000) * 40)

                    try {
                        audioRecord = AudioRecord(MediaRecorder.AudioSource.UNPROCESSED, sampleRateHz, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT, bufferSize)
                    } catch (_: Exception) {}

                    if (audioRecord == null || audioRecord?.state != AudioRecord.STATE_INITIALIZED) {
                        audioRecord?.release()
                        audioRecord = AudioRecord(MediaRecorder.AudioSource.MIC, sampleRateHz, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT, bufferSize)
                    }

                    if (audioRecord?.state != AudioRecord.STATE_INITIALIZED) {
                        _status.value = _status.value.copy(isRunning = false, errorMsg = "麦克风初始化失败")
                        stopSelf()
                        return@launch
                    }

                    // ── 重置状态 ──
                    agcSmoothedRms = 0.008f ; agcCurrentGain = 10.0f ; ngNoiseFloor = 0.002f
                    ngAttenuation = 1.0f ; ngHoldCounter = 0 ; ngFloorFrozen = false ; ngSilenceTimer = 0

                    val readSize = frameSize * 2
                    audioRecord?.startRecording()

                    val pcmBuffer = ShortArray(readSize)
                    val pcmAccum = ShortArray(frameSize)
                    var accumPos = 0

                    _status.value = _status.value.copy(isRunning = true, isConnected = true)

                    while (isActive) {
                        // ── 增加网络目标动态热重连判断 ──
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

                        val readStart = System.nanoTime()
                        val read = audioRecord?.read(pcmBuffer, 0, readSize) ?: -1
                        if (read <= 0) continue
                        if (firstReadTime == 0L) firstReadTime = readStart
                        readSamplesTotal += read

                        var srcPos = 0
                        while (srcPos < read) {
                            val n = minOf(read - srcPos, frameSize - accumPos)
                            pcmBuffer.copyInto(pcmAccum, accumPos, srcPos, srcPos + n)
                            accumPos += n
                            srcPos += n

                            if (accumPos == frameSize) {
                                // ── 步骤1: 计算帧 RMS ──
                                var sumSq = 0L
                                for (s in pcmAccum) { val v = s.toInt(); sumSq += (v * v).toLong() }
                                val frameRmsLinear = sqrt(sumSq.toDouble() / frameSize).toFloat() / 32768.0f

                                // ── 步骤2: 智能 AGC 模块 ──
                                val menuAgcEnabled = Prefs.agcEnabled
                                val userGainSetting = Prefs.agcMaxGain.toFloat().coerceIn(2.0f, 200f)

                                if (menuAgcEnabled && !lastAgcEnabled) {
                                    agcSmoothedRms = 0.01f
                                    agcCurrentGain = 10.0f
                                }
                                lastAgcEnabled = menuAgcEnabled 

                                if (menuAgcEnabled) {
                                    agcSmoothedRms = agcSmoothedRms * 0.85f + frameRmsLinear * 0.15f
                                    val AUTO_AGC_MAX_LIMIT = 100.0f
                                    val targetGain = (agcTargetRms / (agcSmoothedRms + 1e-5f)).coerceIn(agcMinGain, AUTO_AGC_MAX_LIMIT)
                                    val alpha = if (targetGain < agcCurrentGain) 0.20f else 0.02f
                                    agcCurrentGain = agcCurrentGain * (1f - alpha) + targetGain * alpha
                                } else {
                                    // 【固定增益模式】
                                    // 完全听从用户滑块设置的 userGainSetting，采用指数平滑消除调节滑块时的突变爆音
                                    agcCurrentGain = agcCurrentGain * 0.7f + userGainSetting * 0.3f
                                }

                                for (i in pcmAccum.indices) {
                                    val amplified = (pcmAccum[i] * agcCurrentGain).toInt().coerceIn(-32768, 32767)
                                    pcmAccum[i] = amplified.toShort()
                                }

                                // ── Opus 编码参数毫秒级动态同步 ──
                                val curCplx = Prefs.opusComplexity
                                val curSig = Prefs.opusSignal
                                val curBw = Prefs.opusBandwidth
                                val curDtx = Prefs.opusDtx
                                // 【策略防线】若 DTX 开启，JNI 灌入的 vbr 必须强制为 1 规避底层冲突
                                val curVbr = if (curDtx == 1) 1 else Prefs.opusVbr
                                val curBr = if (Prefs.opusBitrateKbps > 0) Prefs.opusBitrateKbps else {
                                    when (sampleRateHz) { 48000 -> 64; 24000 -> 40; 16000 -> 32; else -> 24 }
                                }
                                val curHash = java.util.Objects.hash(curCplx, curSig, curBw, curDtx, curVbr, curBr)
                                if (curHash != lastOpusConfigHash) {
                                    if (encoder?.update(curCplx, curSig, curBw, curDtx, curVbr, curBr) == true) {
                                        lastOpusConfigHash = curHash
                                        _status.value = _status.value.copy(bitrateTargetKbps = curBr)
                                    }
                                }

                                // ── 步骤3: 噪声门 ──
                                if (Prefs.noiseGate) {
                                    val threshold = ngNoiseFloor * ngThresholdMul
                                    if (frameRmsLinear > threshold) {
                                        ngFloorFrozen = true ; ngHoldCounter = ngHoldFrames ; ngSilenceTimer = 0
                                        ngAttenuation += (1.0f - ngAttenuation) * 0.35f
                                    } else {
                                        if (ngHoldCounter > 0) ngHoldCounter-- else ngAttenuation += (0.0f - ngAttenuation) * 0.04f
                                        ngSilenceTimer++
                                        if (ngSilenceTimer > 100) ngFloorFrozen = false
                                    }
                                    if (!ngFloorFrozen && frameRmsLinear < ngNoiseFloor * 2.5f) {
                                        ngNoiseFloor = ngNoiseFloor * 0.97f + frameRmsLinear * 0.03f
                                    }
                                } else { ngAttenuation = 1.0f }

                                if (ngAttenuation < 1.0f) {
                                    for (i in pcmAccum.indices) {
                                        val gated = (pcmAccum[i] * ngAttenuation).toInt().coerceIn(-32768, 32767)
                                        pcmAccum[i] = gated.toShort()
                                    }
                                }

                                encoder?.encode(pcmAccum)?.let { packet ->
                                    if (udpSender?.send(packet) == true) byteCount += packet.size
                                }
                                accumPos = 0
                            }
                        }

                        val now = System.currentTimeMillis()
                        if (now - lastReport >= 1000) {
                            val kbps = if (now - lastReport > 0) (byteCount * 8f) / (now - lastReport) else 0f
                            _status.value = _status.value.copy(
                                bitrateKbps = kbps,
                                agcGainDb = 20f * log10(agcCurrentGain.toDouble().coerceAtLeast(1e-6)).toFloat(),
                                agcGainX = agcCurrentGain
                            )
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
                // 确保干净释放
                try { audioRecord?.stop() } catch (_: Exception){}
                try { audioRecord?.release() } catch (_: Exception){}
                audioRecord = null
                encoder?.stop()
                encoder = null
                try { udpSender?.close() } catch (_: Exception){}
                udpSender = null
                try { if (wakeLock.isHeld) wakeLock.release() } catch (_: Exception){}
                
                val restart = pendingRestart
                pendingRestart = null
                if (restart != null) {
                    serviceScope.launch { doStartCapture(restart.sampleRateHz, restart.bitrateKbps, restart.targetIp, restart.targetPort, restart.testTone, restart.noiseGate) }
                } else if (!hasNewCapture) {
                    _status.value = _status.value.copy(isRunning = false, isConnected = false)
                    try { stopForeground(STOP_FOREGROUND_REMOVE) } catch (_: Exception){}
                    stopSelf()
                }
            }
        }
    }

    fun stopCapture() { pendingRestart = null; hasNewCapture = false; captureJob?.cancel(); captureJob = null }
    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            getSystemService(NotificationManager::class.java).createNotificationChannel(
                NotificationChannel("udp2mic_capture", "采集服务", NotificationManager.IMPORTANCE_LOW).apply { setShowBadge(false) }
            )
        }
    }
    private fun buildNotification(): Notification = NotificationCompat.Builder(this, "udp2mic_capture").setContentTitle("UDP2Mic 采集运行中").setSmallIcon(android.R.drawable.ic_btn_speak_now).setOngoing(true).build()
    override fun onDestroy() { stopCapture(); serviceScope.cancel(); super.onDestroy() }
}