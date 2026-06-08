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
import com.udp2cal.app.Udp2CalProtocol
import com.udp2cal.app.UdpSender
import com.udp2cal.app.native.OpusEncoder
import com.udp2cal.app.native.OpusDecoder as ReverseOpusDecoder
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

    private val _status = MutableStateFlow(CaptureStatus())
    val status: StateFlow<CaptureStatus> = _status.asStateFlow()

    private var audioRecord: AudioRecord? = null
    private var encoder: OpusEncoder? = null
    private var udpSender: UdpSender? = null
    private var audioChannel: Channel<ShortArray>? = null

    // 反向音频（PC→Phone）
    private var reverseDecoder: ReverseOpusDecoder? = null
    private var audioPlayer: AudioPlayer? = null

    private data class CaptureParams(
        val sampleRateHz: Int, val bitrateKbps: Int, val targetIp: String, val targetPort: Int
    )
    private var pendingRestart: CaptureParams? = null

    object AudioSourceLabel {
        const val VOICE_COMMUNICATION = "系统硬件降噪(NS+AGC)"
        const val MIC_FALLBACK = "裸麦克风(系统降噪不可用)"
        const val AEC_ENABLED = " + AEC已启用"
        const val AEC_UNAVAILABLE = " + AEC不可用"
    }

    data class CaptureStatus(
        val isRunning: Boolean = false,
        val errorMsg: String = "",
        val connected: Boolean = false,
        val audioSource: String = "",
        val reverseAudio: Boolean = false
    )

    inner class LocalBinder : Binder() { fun getService(): CaptureService = this@CaptureService }
    override fun onBind(intent: Intent?): IBinder = binder
    override fun onCreate() { super.onCreate() }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        createNotificationChannel()
        try { startForeground(1, buildNotification()) } catch (e: Exception) {
            Log.e(TAG, "startForeground failed", e)
            stopSelf()
            return START_NOT_STICKY
        }
        return START_STICKY
    }

    fun startCapture(sampleRateHz: Int, bitrateKbps: Int, targetIp: String, targetPort: Int) {
        if (captureJob?.isActive == true) {
            pendingRestart = CaptureParams(sampleRateHz, bitrateKbps, targetIp, targetPort)
            captureJob?.cancel()
            return
        }
        pendingRestart = null
        doStartCapture(sampleRateHz, bitrateKbps, targetIp, targetPort)
    }

    private fun doStartCapture(
        sampleRateHz: Int, bitrateKbps: Int, targetIp: String, targetPort: Int
    ) {
        val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
        val wakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "UDP2CAL:Capture")
        try { wakeLock.acquire(10 * 60 * 1000L) } catch (_: Exception) {}

        hasNewCapture = true

        captureJob = serviceScope.launch {
            _status.value = CaptureStatus(isRunning = true, errorMsg = "")

            try {
                udpSender = UdpSender(targetIp, targetPort)
                if (!udpSender!!.connect()) {
                    _status.value = CaptureStatus(isRunning = false, errorMsg = "网络连接失败")
                    stopSelf()
                    return@launch
                }
                encoder = OpusEncoder(sampleRateHz, bitrateKbps).also {
                    it.deviceId = udpSender!!.getDeviceId() ?: DiscoveryManager.getOrCreateDeviceId()
                }
                if (!encoder!!.start()) {
                    _status.value = CaptureStatus(isRunning = false, errorMsg = "编码器初始化失败")
                    stopSelf()
                    return@launch
                }

                val frameSize = encoder!!.frameSize
                if (frameSize <= 0) {
                    _status.value = CaptureStatus(isRunning = false, errorMsg = "编码器帧大小无效")
                    stopSelf()
                    return@launch
                }

                // 先尝试 VOICE_COMMUNICATION（系统硬件降噪），失败回退 MIC
                val minBufSize = AudioRecord.getMinBufferSize(sampleRateHz, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT)
                if (minBufSize == AudioRecord.ERROR || minBufSize == AudioRecord.ERROR_BAD_VALUE) {
                    _status.value = CaptureStatus(isRunning = false, errorMsg = "麦克风不支持请求的采样率")
                    stopSelf()
                    return@launch
                }
                val bufferSize = (minBufSize * 2).coerceAtLeast((sampleRateHz / 1000) * 40)

                var audioSourceLabel: String
                try {
                    audioRecord = AudioRecord(
                        MediaRecorder.AudioSource.VOICE_COMMUNICATION,
                        sampleRateHz, AudioFormat.CHANNEL_IN_MONO,
                        AudioFormat.ENCODING_PCM_16BIT, bufferSize
                    )
                } catch (_: Exception) {}

                if (audioRecord?.state == AudioRecord.STATE_INITIALIZED) {
                    audioSourceLabel = AudioSourceLabel.VOICE_COMMUNICATION
                    Log.i(TAG, "音源: VOICE_COMMUNICATION（系统硬件降噪）")
                } else {
                    audioRecord?.release()
                    Log.w(TAG, "VOICE_COMMUNICATION 不可用，回退到 MIC 裸采集")
                    audioRecord = AudioRecord(
                        MediaRecorder.AudioSource.MIC,
                        sampleRateHz, AudioFormat.CHANNEL_IN_MONO,
                        AudioFormat.ENCODING_PCM_16BIT, bufferSize
                    )
                    audioSourceLabel = AudioSourceLabel.MIC_FALLBACK
                }

                if (audioRecord?.state != AudioRecord.STATE_INITIALIZED) {
                    _status.value = CaptureStatus(isRunning = false, errorMsg = "麦克风初始化失败")
                    stopSelf()
                    return@launch
                }

                // 启用安卓原生 AEC（声学回声消除），仅在 VOICE_COMMUNICATION 播放时有效
                val aecState: String
                if (AcousticEchoCanceler.isAvailable()) {
                    val aec = try {
                        AcousticEchoCanceler.create(audioRecord!!.audioSessionId)
                    } catch (_: Exception) { null }
                    if (aec != null) {
                        aec.enabled = true
                        aecState = AudioSourceLabel.AEC_ENABLED
                        Log.i(TAG, "AEC 已启用")
                    } else {
                        aecState = AudioSourceLabel.AEC_UNAVAILABLE
                        Log.w(TAG, "AEC isAvailable()=true 但 create()=null")
                    }
                } else {
                    aecState = AudioSourceLabel.AEC_UNAVAILABLE
                    Log.w(TAG, "AEC 不可用")
                }
                audioSourceLabel += aecState

                val readSize = frameSize * 2
                audioRecord?.startRecording()

                // 初始化反向音频（PC→Phone 解码+播放）
                reverseDecoder = ReverseOpusDecoder(sampleRateHz).also {
                    if (!it.start()) {
                        Log.w(TAG, "反向音频解码器初始化失败，继续运行")
                    }
                }
                audioPlayer = AudioPlayer(this@CaptureService).also { player ->
                    val aecSessionId = (audioRecord?.audioSessionId ?: 0)
                    if (!player.start(aecSessionId)) {
                        Log.w(TAG, "反向音频播放器初始化失败，继续运行")
                    } else {
                        // 如果是 VOICE_COMMUNICATION 模式，追加提示
                        if (player.isVoiceCommActive) {
                            audioSourceLabel += " [AEC管线]"
                        }
                    }
                }

                _status.value = CaptureStatus(
                    isRunning = true,
                    connected = false,
                    errorMsg = "",
                    audioSource = audioSourceLabel
                )

                val ch = Channel<ShortArray>(3)
                audioChannel = ch

                val sendBuffers = arrayOf(
                    ByteArray(Udp2CalProtocol.MAX_PACKET),
                    ByteArray(Udp2CalProtocol.MAX_PACKET)
                )
                var bufIndex = 0

                val framePool = ShortArrayPool(frameSize, 5)

                val producerJob = launch(Dispatchers.IO) {
                    val pcmBuffer = ShortArray(readSize)
                    var accumPos = 0
                    var accumBuf = framePool.borrow()

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
                                ch.send(accumBuf)
                                accumBuf = framePool.borrow()
                                accumPos = 0
                            }
                        }
                    }
                }

                val consumerJob = launch {
                    var reconnectCounter = 0
                    val RECONNECT_INTERVAL = 50
                    var p2pConnected = false

                    var reverseJob: Job? = null

                    for (frame in ch) {
                        // 只读 ACK（不读音频——反向音频走独立 socket）
                        if (udpSender?.drainAck() == true && !p2pConnected) {
                            p2pConnected = true
                            _status.value = _status.value.copy(connected = true, errorMsg = "")
                            // 连接确认后启动反向音频协程（独立 socket，不干扰正向）
                            reverseJob = launchReverseAudio()
                        }

                        reconnectCounter++
                        if (reconnectCounter >= RECONNECT_INTERVAL) {
                            reconnectCounter = 0
                            try {
                                val devId = DiscoveryManager.getOrCreateDeviceId()
                                // 保活 CONNECT 携带反向端口 + 低性能标志
                                val revPort = udpSender?.reversePort ?: 0
                                val payload = byteArrayOf(
                                    (revPort shr 8).toByte(), revPort.toByte(),
                                    1 // 低性能模式
                                )
                                val ping = Udp2CalProtocol.buildPacket(
                                    isAudio = false, msgType = Udp2CalProtocol.TYPE_CONNECT,
                                    sampleRate = 0, seqNum = 0, deviceId = devId, payload = payload
                                )
                                udpSender?.send(ping, 0, ping.size)
                            } catch (_: Exception) {}
                        }

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

                        if (p2pConnected) {
                            val buf = sendBuffers[bufIndex]
                            bufIndex = (bufIndex + 1) % 2
                            val written = encoder?.encodeTo(frame, buf, 0) ?: -1
                            if (written > 0) {
                                udpSender?.send(buf, 0, written)
                            }
                        }

                        framePool.recycle(frame)
                    }
                }

                try {
                    producerJob.join()
                } finally {
                    consumerJob.cancel()
                    ch.close()
                    audioChannel = null
                }
            } catch (_: CancellationException) {
            } catch (e: Exception) {
                Log.e(TAG, "采集异常", e)
                _status.value = CaptureStatus(isRunning = false, errorMsg = e.message ?: "未知错误")
            } finally {
                try { audioRecord?.stop() } catch (_: Exception){}
                try { audioRecord?.release() } catch (_: Exception){}
                audioRecord = null
                encoder?.stop()
                encoder = null
                try { udpSender?.close() } catch (_: Exception){}
                udpSender = null
                reverseAudioJob?.cancel()
                reverseDecoder?.stop()
                audioPlayer?.stop()

                try { if (wakeLock.isHeld) wakeLock.release() } catch (_: Exception){}

                if (_status.value.errorMsg.contains("麦克风初始化失败")
                    || _status.value.errorMsg.contains("麦克风不支持")
                    || _status.value.errorMsg.contains("编码器初始化失败")
                    || _status.value.errorMsg.contains("编码器帧大小无效")) {
                    pendingRestart = null
                }

                val restart = pendingRestart
                pendingRestart = null
                if (restart != null) {
                    serviceScope.launch { doStartCapture(restart.sampleRateHz, restart.bitrateKbps, restart.targetIp, restart.targetPort) }
                } else if (!hasNewCapture) {
                    _status.value = CaptureStatus(isRunning = false)
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
            val pcmBuf = ShortArray(dec.frameSize)
            var hasAudio = false
            var lastAudioTime = 0L

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
                        if (!hasAudio) {
                            hasAudio = true
                            _status.value = _status.value.copy(reverseAudio = true)
                        }
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

    fun stopCapture() {
        pendingRestart = null
        hasNewCapture = false
        audioChannel?.close()
        audioChannel = null
        captureJob?.cancel()
        captureJob = null
        reverseAudioJob?.cancel()
        reverseAudioJob = null
        _status.value = CaptureStatus(connected = false)
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            nm.createNotificationChannel(
                NotificationChannel("udp2cal_capture", "采集服务", NotificationManager.IMPORTANCE_LOW).apply { setShowBadge(false) }
            )
        }
    }

    private fun buildNotification(): Notification = NotificationCompat.Builder(this, "udp2cal_capture")
        .setContentTitle("UDP2CAL 运行中")
        .setSmallIcon(android.R.drawable.ic_btn_speak_now)
        .setOngoing(true)
        .build()

    override fun onDestroy() { stopCapture(); serviceScope.cancel(); super.onDestroy() }

    private class ShortArrayPool(val frameSize: Int, val capacity: Int = 3) {
        private val pool = arrayOfNulls<ShortArray>(capacity)
        private var head = 0
        private var count = 0

        @Synchronized
        fun borrow(): ShortArray {
            return if (count > 0) {
                val idx = head
                head = (head + 1) % capacity
                count--
                pool[idx]!!
            } else {
                ShortArray(frameSize)
            }
        }

        @Synchronized
        fun recycle(buf: ShortArray) {
            val tail = (head + count) % capacity
            if (count < capacity) {
                pool[tail] = buf
                count++
            }
        }
    }
}
