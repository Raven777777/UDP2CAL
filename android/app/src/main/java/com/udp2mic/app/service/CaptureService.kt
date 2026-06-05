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
    /** 音频帧通道引用，用于 stopCapture 时强制关闭 */
    private var audioChannel: Channel<ShortArray>? = null

    private data class CaptureParams(
        val sampleRateHz: Int, val bitrateKbps: Int, val targetIp: String, val targetPort: Int,
        val testTone: Boolean, val noiseGate: Boolean
    )
    private var pendingRestart: CaptureParams? = null

    // ── AGC 状态（旧版已废弃，新版 AGC 使用消费者协程内的局部变量）

    // ── 自适应噪声门状态（v1.0.6 升级：硬静音 + 自动/手动模式）──
    private var ambientEnergy = 50.0       // 动态追踪的环境底噪 RMS 能量（Short 域）
    private val alphaTrack = 0.005         // 背景能量追踪系数（极慢跟随，约数秒平滑）
    private var silenceFrameCount = 0      // 连续静音帧计数，用于平滑释放
    private val HOLD_FRAMES = 8            // 说话停止后，保持门打开的帧数（8帧 * 20ms = 160ms 尾音保护）

    // ── Android 硬件级主动降噪 ──
    private var hardwareNoiseSuppressor: android.media.audiofx.NoiseSuppressor? = null

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

                _status.value = _status.value.copy(ngActive = noiseGate, sampleRateHz = sampleRateHz, bitrateTargetKbps = bitrateKbps)
                var byteCount = 0L
                var lastReport = System.currentTimeMillis()

                if (testTone) {
                    val omega = 2.0 * Math.PI * 1000.0 / sampleRateHz
                    val testToneBuf = ShortArray(frameSize)
                    var phase = 0.0
                    val frameIntervalNs = (frameSize * 1_000_000_000L) / sampleRateHz
                    var nextFrameTime = System.nanoTime()

                    _status.value = _status.value.copy(isRunning = true, isConnected = true)

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
                    ambientEnergy = 50.0 ; silenceFrameCount = 0

                    val readSize = frameSize * 2
                    audioRecord?.startRecording()
                    // 根据用户配置启动硬件降噪联动
                    updateHardwareNoiseCancellation(Prefs.noiseGate)

                    _status.value = _status.value.copy(isRunning = true, isConnected = true)

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
                            // 【硬件降噪联动】每帧检测 Prefs 状态变化，热生效
                            val shouldNs = Prefs.noiseGate
                            if (shouldNs != (hardwareNoiseSuppressor != null)) {
                                updateHardwareNoiseCancellation(shouldNs)
                            }

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

                    // ═══ 消费者协程 ═══：AGC → 编码参数更新 → 噪声门 → 编码 → 发送
                    // 与生产者在两个独立协程并行执行，流水线消除 IO 阻塞间隙
                    // ═══ 升级版：智能 AGC + 自适应噪声门 消费者协程 ═══
                    val consumerJob = launch {
                        var currentIp = targetIp
                        var currentPort = targetPort
                        var byteCount = 0L
                        var lastReport = System.currentTimeMillis()
                        var lastOpusConfigHash = 0
                        var highEnergyFrameCount = 0

                        // ── 智能 AGC 与底噪追踪核心状态 ──
                        var agcNoiseFloorDb = -50.0       // 动态追踪的底噪分贝基准 (dBFS)
                        val alphaTrackNoise = 0.002       // 极慢的底噪追踪系数
                        var agcCurrentGain = 1.0f         // 当前帧最终生效增益
                        var agcPreviousGain = 1.0f        // 上一帧的最终增益（用于插值平滑）

                        for (frame in ch) {
                            // ── 步骤0: 网络目标动态热重连 ──
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

                            // ── 步骤1: 计算原始音频帧 RMS 与真实 dBFS（基于未加工的输入） ──
                            var sumSq = 0L
                            for (s in frame) { val v = s.toInt(); sumSq += (v * v).toLong() }
                            val frameRmsLinear = sqrt(sumSq.toDouble() / frameSize)
                            
                            // 归一化到 32768.0 最大振幅，直接得到标准 dBFS (-100.0dB ~ 0.0dB)
                            val currentDb = if (frameRmsLinear > 0.0) 20.0 * log10(frameRmsLinear / 32768.0) else -100.0

                            // ── 步骤2: 智能自适应 AGC 模块 ──
                            val menuAgcEnabled = Prefs.agcEnabled
                            val userMaxGainLimit = Prefs.agcMaxGain.toFloat().coerceIn(1.0f, 200f) // 界面最大增益限制
                            // 安全区：自动模式固定 10dB，手动模式使用滑块值（0=关闭安全区），每帧热生效
                            val agcSafeZoneDb = if (menuAgcEnabled) 10.0 else Prefs.agcSafeZone.toDouble()

                            if (menuAgcEnabled) {
                                // A. 动态追踪底噪：当声音处于极低水平，或者连续多帧处于非突发状态时，让底噪基准咬合
                                if (currentDb < agcNoiseFloorDb + 3.0 || currentDb < -45.0) {
                                    agcNoiseFloorDb = agcNoiseFloorDb * (1.0 - alphaTrackNoise) + currentDb * alphaTrackNoise
                                }

                                // B. 核心智能控制：判定是否为“真人在说话区”
                                val isRealVoice = currentDb > (agcNoiseFloorDb + agcSafeZoneDb)

                                if (isRealVoice) {
                                    // 理想人声目标设为 -18.0 dBFS
                                    val targetVoiceDb = -18.0 
                                    val dbDeficit = targetVoiceDb - currentDb // 距离目标的能量缺口
                                    
                                    if (dbDeficit > 0) {
                                        // 计算理想放大倍数
                                        val idealGain = Math.pow(10.0, dbDeficit / 20.0).toFloat()
                                        // 受限于用户在界面设置的麦克风最大 AGC 增益限制
                                        val targetGain = idealGain.coerceAtMost(userMaxGainLimit)
                                        // 智能控制快进慢出：激活放大时，让增益温和跟进
                                        agcCurrentGain = agcCurrentGain * 0.8f + targetGain * 0.2f
                                    } else {
                                        // 声音已经足够大或超标，快速向 1.0f 沉降（起到压限 Limiter 保护作用）
                                        agcCurrentGain = agcCurrentGain * 0.5f + 1.0f * 0.5f
                                    }
                                } else {
                                    // 【环境底噪区】锁死增益：低于安全范围，强制让 AGC 增益平滑沉降回 1.0f，不放大底噪
                                    agcCurrentGain = agcCurrentGain * 0.7f + 1.0f * 0.3f
                                }
                            } else {
                                // 【固定增益模式】如果关闭智能 AGC，退化为通过滑块平滑调节固定增益
                                agcCurrentGain = agcCurrentGain * 0.7f + userMaxGainLimit * 0.3f
                            }

                            // C. 样点级线性插值：消除帧边界信号幅度断层导致的"咔哒"爆音
                            val gStart = agcPreviousGain
                            val gEnd = agcCurrentGain
                            if (gStart != gEnd) {
                                for (i in frame.indices) {
                                    val t = i.toFloat() / frameSize.toFloat()
                                    val gain = gStart + (gEnd - gStart) * t
                                    val amplified = (frame[i] * gain).toInt().coerceIn(-32768, 32767)
                                    frame[i] = amplified.toShort()
                                }
                            } else if (gEnd != 1.0f) {
                                for (i in frame.indices) {
                                    val amplified = (frame[i] * gEnd).toInt().coerceIn(-32768, 32767)
                                    frame[i] = amplified.toShort()
                                }
                            }
                            agcPreviousGain = agcCurrentGain // 滚动迭代增益历史

                            // ── Opus 编码参数毫秒级动态同步 ──
                            val curCplx = Prefs.opusComplexity
                            val curSig = Prefs.opusSignal
                            val curBw = Prefs.opusBandwidth
                            val curDtx = Prefs.opusDtx
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

                            // ── 步骤3: 自动/手动噪声门判定（此时操作的是经 AGC 放大后的音频） ──
                            var shouldMuteFrame = false
                            val ngManualThresholdDb = Prefs.noiseGateThreshold
                            val ngAutoEnabled = Prefs.noiseGate

                            if (ngAutoEnabled || ngManualThresholdDb > -60f) {
                                var postSumSq = 0L
                                for (s in frame) { val v = s.toInt(); postSumSq += (v * v).toLong() }
                                val postRms = sqrt(postSumSq.toDouble() / frameSize)
                                val postDb = if (postRms > 0.0) 20.0 * log10(postRms / 32768.0) else -100.0

                                val targetDb = if (ngAutoEnabled) {
                                    if (ambientEnergy <= 50.0 && postRms > 100.0) {
                                        ambientEnergy = postRms
                                    } else if (postRms > ambientEnergy * 4.0) {
                                        highEnergyFrameCount++
                                        if (highEnergyFrameCount > 6) { ambientEnergy = postRms }
                                    } else {
                                        highEnergyFrameCount = 0
                                    }

                                    if (postRms < ambientEnergy * 2.0) {
                                        ambientEnergy = (1.0 - alphaTrack) * ambientEnergy + alphaTrack * postRms
                                    }
                                    20.0 * log10((ambientEnergy * 1.5) / 32768.0)
                                } else {
                                    ngManualThresholdDb.toDouble()
                                }

                                if (postDb > targetDb) {
                                    silenceFrameCount = 0
                                } else {
                                    if (silenceFrameCount < HOLD_FRAMES) {
                                        silenceFrameCount++
                                    } else {
                                        shouldMuteFrame = true
                                    }
                                }
                            }

                            // ── 步骤4: 最终裁决（噪声门平滑衰减 10%）+ 编码 + 发送 ──
                            if (shouldMuteFrame) {
                                for (i in frame.indices) {
                                    frame[i] = (frame[i] * 0.1f).toInt().coerceIn(-32768, 32767).toShort()
                                }
                            }

                            val buf = sendBuffers[bufIndex]
                            bufIndex = (bufIndex + 1) % 2
                            val written = encoder?.encodeTo(frame, buf, 0) ?: -1
                            if (written > 0) {
                                if (udpSender?.send(buf, 0, written) == true) byteCount += written
                            }

                            // ── 步骤5: 每秒状态汇报 ──
                            val now = System.currentTimeMillis()
                            if (now - lastReport >= 1000) {
                                val kbps = if (now - lastReport > 0) (byteCount * 8f) / (now - lastReport) else 0f
                                _status.value = _status.value.copy(
                                    bitrateKbps = kbps,
                                    agcGainDb = 20f * log10(agcCurrentGain.toDouble().coerceAtLeast(1e-6)).toFloat(),
                                    agcGainX = agcCurrentGain,
                                    ngActive = Prefs.noiseGate
                                )
                                byteCount = 0
                                lastReport = now
                            }

                            framePool.recycle(frame) // 完美的零内存分配闭环归还
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

                // ✅ 熔断保护：硬件初始化硬故障（麦克风被占用/不支持）时拒绝无限重启，防止 ANR
                if (_status.value.errorMsg.contains("麦克风初始化失败") || _status.value.errorMsg.contains("麦克风不支持")) {
                    Log.e(TAG, "🚨 发生硬件初始化硬故障，清空挂起任务，实施安全熔断。")
                    pendingRestart = null
                }

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

    /** 动态管理 Android 硬件级主动降噪（与自动噪声门联动） */
    private fun updateHardwareNoiseCancellation(enabled: Boolean) {
        val audioRecordInstance = audioRecord ?: return
        if (!android.media.audiofx.NoiseSuppressor.isAvailable()) {
            Log.w(TAG, "当前设备硬件不支持 NoiseSuppressor")
            return
        }
        if (enabled) {
            if (hardwareNoiseSuppressor == null) {
                try {
                    hardwareNoiseSuppressor = android.media.audiofx.NoiseSuppressor.create(audioRecordInstance.audioSessionId)
                    hardwareNoiseSuppressor?.enabled = true
                    Log.i(TAG, ">>> 联动成功：已启动 Android 硬件级主动降噪 <<<")
                } catch (e: Exception) {
                    Log.e(TAG, "创建硬件降噪器失败", e)
                }
            }
        } else {
            if (hardwareNoiseSuppressor != null) {
                try {
                    hardwareNoiseSuppressor?.enabled = false
                    hardwareNoiseSuppressor?.release()
                    Log.i(TAG, ">>> 已关闭 Android 硬件级主动降噪 <<<")
                } catch (e: Exception) {
                    Log.e(TAG, "释放硬件降噪器失败", e)
                } finally {
                    hardwareNoiseSuppressor = null
                }
            }
        }
    }

    fun stopCapture() {
        pendingRestart = null
        hasNewCapture = false
        // 清理硬件降噪器
        try {
            hardwareNoiseSuppressor?.enabled = false
            hardwareNoiseSuppressor?.release()
        } catch (_: Exception) {}
        hardwareNoiseSuppressor = null
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