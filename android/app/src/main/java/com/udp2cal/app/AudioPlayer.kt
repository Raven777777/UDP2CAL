package com.udp2cal.app

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.os.Build
import android.util.Log

/**
 * 反向音频播放器
 * 支持通用适配：优先 VOICE_COMMUNICATION + 听筒路由实现 AEC，
 * 失败自动回退 MEDIA + 扬声器。
 *
 * 时钟模型（重要）：
 * PC 端采集时钟与手机 DAC 时钟不同源。历史实现使用阻塞式 `write()`，
 * 当 PC 略快时 AudioTrack 缓冲被填满 → 接收协程被阻塞 → socket 缓冲溢出 →
 * 成片丢包，且延迟只增不减。现在改为：
 *   1. 本地队列吸收抖动（非阻塞写入，永不阻塞接收协程）；
 *   2. 队列超过高水位时丢最旧数据，把端到端延迟钉在目标值附近；
 *   3. 写入用 WRITE_NON_BLOCKING，按实际写入量出队。
 */
class AudioPlayer(private val context: Context) {
    companion object {
        private const val TAG = "AudioPlayer"
        private const val SAMPLE_RATE = 48000
        private const val BUFFER_MS = 40
        private const val CHANNELS = 2 // 立体声

        /** 单次向 AudioTrack 投递的最大样本数（立体声交错） */
        private const val CHUNK = 2048
        /** 队列高水位：超过就裁剪，避免延迟累积（240ms） */
        private const val HIGH_WATER = SAMPLE_RATE * CHANNELS * 240 / 1000
        /** 裁剪目标（120ms） */
        private const val TARGET_WATER = SAMPLE_RATE * CHANNELS * 120 / 1000
        /** 队列容量（1s），仅作兜底 */
        private const val QUEUE_CAPACITY = SAMPLE_RATE * CHANNELS
    }

    @Volatile private var audioTrack: AudioTrack? = null
    private var audioManager: AudioManager? = null

    /** 是否成功启用 VOICE_COMMUNICATION 模式（含 AEC 能力） */
    @Volatile var isVoiceCommActive: Boolean = false

    private val lock = Any()
    private val queue = ShortQueue(QUEUE_CAPACITY)
    private val chunkBuf = ShortArray(CHUNK)

    /** 因队列溢出/裁剪被丢弃的样本数（诊断用） */
    @Volatile var droppedSamples: Long = 0
        private set

    fun start(audioSessionId: Int = 0, voiceMode: Boolean = true): Boolean {
        audioManager = context.getSystemService(Context.AUDIO_SERVICE) as? AudioManager

        val bufferSize = (SAMPLE_RATE * BUFFER_MS / 1000) * 2 * CHANNELS
        val minBufSize = AudioTrack.getMinBufferSize(
            SAMPLE_RATE,
            AudioFormat.CHANNEL_OUT_STEREO,
            AudioFormat.ENCODING_PCM_16BIT
        )
        val finalBufSize = bufferSize.coerceAtLeast(minBufSize)

        synchronized(lock) { queue.clear() }

        if (voiceMode) {
            // 语音模式：VOICE_COMMUNICATION + 听筒路由（启用 AEC 管线）
            Log.i(TAG, "AudioPlayer 语音模式")
            routeToEarpiece(true)
            if (!tryCreateTrack(AudioAttributes.USAGE_VOICE_COMMUNICATION, AudioAttributes.CONTENT_TYPE_SPEECH,
                    AudioManager.STREAM_VOICE_CALL, audioSessionId, finalBufSize)) {
                Log.w(TAG, "VOICE_COMMUNICATION 不可用，回退到 MEDIA")
                isVoiceCommActive = false
                routeToEarpiece(false)
                if (!tryCreateTrack(AudioAttributes.USAGE_MEDIA, AudioAttributes.CONTENT_TYPE_MUSIC,
                        AudioManager.STREAM_MUSIC, 0, finalBufSize)) {
                    Log.e(TAG, "AudioTrack 创建彻底失败")
                    return false
                }
            }
        } else {
            // 音乐模式：MEDIA + 默认扬声器（不改变音频路由，避免干扰 AudioRecord）
            Log.i(TAG, "AudioPlayer 音乐模式")
            isVoiceCommActive = false
            if (!tryCreateTrack(AudioAttributes.USAGE_MEDIA, AudioAttributes.CONTENT_TYPE_MUSIC,
                    AudioManager.STREAM_MUSIC, 0, finalBufSize)) {
                Log.e(TAG, "AudioTrack 创建彻底失败")
                return false
            }
        }

        try {
            audioTrack?.play()
        } catch (e: Exception) {
            Log.e(TAG, "AudioTrack.play() 失败", e)
            audioTrack?.release()
            audioTrack = null
            if (voiceMode) routeToEarpiece(false)
            return false
        }

        Log.i(TAG, "AudioPlayer 已启动（${if (voiceMode) "语音模式" else "音乐模式"}·${if (isVoiceCommActive) "VOICE_COMMUNICATION" else "MEDIA"}）")
        return true
    }

    private fun tryCreateTrack(usage: Int, contentType: Int, streamType: Int,
                               sessionId: Int, bufSize: Int): Boolean {
        return try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                val attrs = AudioAttributes.Builder()
                    .setUsage(usage)
                    .setContentType(contentType)
                    .build()
                val format = AudioFormat.Builder()
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .setSampleRate(SAMPLE_RATE)
                    .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                    .build()
                audioTrack = AudioTrack.Builder()
                    .setAudioAttributes(attrs)
                    .setAudioFormat(format)
                    .setBufferSizeInBytes(bufSize)
                    .setTransferMode(AudioTrack.MODE_STREAM)
                    .apply {
                        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                            setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
                        }
                        if (sessionId > 0) setSessionId(sessionId)
                    }
                    .build()
            } else {
                @Suppress("DEPRECATION")
                audioTrack = AudioTrack(
                    streamType, SAMPLE_RATE,
                    AudioFormat.CHANNEL_OUT_STEREO,
                    AudioFormat.ENCODING_PCM_16BIT,
                    bufSize, AudioTrack.MODE_STREAM
                )
            }
            isVoiceCommActive = (usage == AudioAttributes.USAGE_VOICE_COMMUNICATION)
            true
        } catch (e: Exception) {
            audioTrack = null
            false
        }
    }

    /**
     * 写入解码后的 PCM（立体声交错）。
     * 非阻塞：数据先入本地队列，再按 AudioTrack 当前可接收量投递。
     */
    fun write(pcm: ShortArray, offset: Int, length: Int) {
        if (length <= 0) return
        synchronized(lock) {
            droppedSamples += queue.push(pcm, offset, length)
            // 高水位裁剪：把积压的延迟拉回目标值
            if (queue.count > HIGH_WATER) {
                droppedSamples += queue.trimTo(TARGET_WATER)
            }
        }
        pump()
    }

    /** 把队列中可写的数据尽量推给 AudioTrack（不阻塞） */
    private fun pump() {
        val track = audioTrack ?: return
        var guard = 0
        while (guard++ < 8) {
            val n = synchronized(lock) { queue.peek(chunkBuf, CHUNK) }
            if (n <= 0) return
            val written = try {
                track.write(chunkBuf, 0, n, AudioTrack.WRITE_NON_BLOCKING)
            } catch (e: Exception) {
                Log.e(TAG, "write 失败", e)
                return
            }
            if (written <= 0) return // 缓冲区已满，下一帧继续
            synchronized(lock) { queue.drop(written) }
        }
    }

    fun stop() {
        try { audioTrack?.pause() } catch (_: Exception) {}
        try { audioTrack?.flush() } catch (_: Exception) {}
        try { audioTrack?.stop() } catch (_: Exception) {}
        try { audioTrack?.release() } catch (_: Exception) {}
        audioTrack = null
        synchronized(lock) { queue.clear() }
        routeToEarpiece(false)
    }

    private fun routeToEarpiece(toEarpiece: Boolean) {
        try {
            audioManager?.mode = if (toEarpiece) AudioManager.MODE_IN_COMMUNICATION else AudioManager.MODE_NORMAL
            @Suppress("DEPRECATION")
            audioManager?.isSpeakerphoneOn = !toEarpiece
        } catch (e: Exception) {
            Log.w(TAG, "路由设置失败", e)
        }
    }

    /**
     * 定长循环队列（立体声交错 16bit 样本），只由 write/pump 与 stop 访问，
     * 外部通过 lock 串行化。
     */
    private class ShortQueue(private val capacity: Int) {
        private val buf = ShortArray(capacity)
        private var head = 0
        private var size = 0

        val count: Int get() = size

        /** 入队；空间不足时淘汰最旧样本，返回被淘汰的样本数 */
        fun push(src: ShortArray, offset: Int, length: Int): Int {
            var dropped = 0
            var i = 0
            while (i < length) {
                if (size + 1 > capacity) {
                    // 立体声交错：必须成对淘汰。只丢 1 个样本会让后续 L/R 永久错位
                    // （每个样本都被当成另一个声道播放），听感上是长期声道互换。
                    head = (head + 2) % capacity
                    size -= 2
                    dropped += 2
                }
                buf[(head + size) % capacity] = src[offset + i]
                size++
                i++
            }
            return dropped
        }

        /** 复制最多 max 个样本到 dst，不出队 */
        fun peek(dst: ShortArray, max: Int): Int {
            val n = minOf(max, size)
            for (i in 0 until n) {
                dst[i] = buf[(head + i) % capacity]
            }
            return n
        }

        fun drop(n: Int) {
            val k = minOf(n, size)
            head = (head + k) % capacity
            size -= k
        }

        /** 保留最新 keep 个样本，返回被丢弃的数量（按偶数对齐，保持声道相位） */
        fun trimTo(keep: Int): Int {
            if (size <= keep) return 0
            val dropped = (size - keep) and 1.inv() // 向下取偶
            drop(dropped)
            return dropped
        }

        fun clear() {
            head = 0
            size = 0
        }
    }
}
