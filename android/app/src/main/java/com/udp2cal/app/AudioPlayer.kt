package com.udp2cal.app

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTimestamp
import android.media.AudioTrack
import android.util.Log

/**
 * 反向音频播放器（v3）
 *
 * 时钟模型：**AudioTrack 播放头是唯一音频时钟**。
 *   * 不再维护应用层 PCM 队列，也不做"高水位裁剪"——那只是把延迟藏起来；
 *   * `write` 使用**阻塞写**，写入节奏由 AudioTrack 缓冲决定，不额外 delay；
 *   * 上层（CaptureService 的反向播放线程）依据 [playbackHeadFrames] 拉取/PLC。
 *
 * 固定 48 kHz 立体声：设备若需要其它采样率，由系统混音器重采样，
 * 媒体时间线仍然是 48 kHz，因此播放头换算保持 1:1。
 */
class AudioPlayer(private val context: Context) {
    companion object {
        private const val TAG = "AudioPlayer"
        private const val SAMPLE_RATE = Udp2CalProtocol.OPUS_SAMPLE_RATE
        private const val CHANNELS = 2
        private const val BYTES_PER_SAMPLE = 2

        /**
         * 输出缓冲：**最小可行值**。
         *
         * 结构上我们每 10 ms 阻塞写一帧，所以缓冲至少要能装下"一帧 + 调度余量"；
         * 2 帧（20 ms）是 low-latency 模式下不因调度抖动周期性断音的下限。
         * 更小的缓冲不会降低听感延迟（延迟主要由抖动缓冲水位决定，
         * 目标是 150 ms），只会把欠载风险换成爆音。
         */
        private const val BUFFER_FRAMES = 2 * Udp2CalProtocol.OPUS_FRAME_SAMPLES
    }

    @Volatile private var audioTrack: AudioTrack? = null
    private var audioManager: AudioManager? = null

    /**
     * AudioTrack 的访问互斥。
     *
     * `writeBlocking` 由播放协程调用，`flush`/`stop` 由控制协程或收尾路径调用；
     * `AudioTrack.pause()/flush()/write()` 并发会抛 `IllegalStateException`，
     * 被 catch 后静默吞掉，表现为反向音频偶发中断且无日志。
     * 锁序恒为 linkLock → trackLock（trackLock 内绝不反向获取 linkLock）。
     */
    private val trackLock = Any()

    /** 是否成功启用 VOICE_COMMUNICATION 模式（含 AEC 能力） */
    @Volatile var isVoiceCommActive: Boolean = false

    /** 播放头换算：raw 为 32 位回绕计数，frames 为累计帧数 */
    private var headRaw: Int = 0
    private var headFrames: Long = 0
    /** 已写入 AudioTrack 的累计帧数（含尚未播出的部分） */
    private var writtenFrames: Long = 0

    /** 最近一次成功启动的参数，供 AudioTrack 失效后原地重建 */
    @Volatile private var startedVoiceMode: Boolean = true
    @Volatile private var startedSessionId: Int = 0

    fun start(audioSessionId: Int = 0, voiceMode: Boolean = true): Boolean {
        startedVoiceMode = voiceMode
        startedSessionId = audioSessionId
        audioManager = context.getSystemService(Context.AUDIO_SERVICE) as? AudioManager

        val minBufSize = AudioTrack.getMinBufferSize(
            SAMPLE_RATE,
            AudioFormat.CHANNEL_OUT_STEREO,
            AudioFormat.ENCODING_PCM_16BIT
        )
        val wanted = BUFFER_FRAMES * BYTES_PER_SAMPLE * CHANNELS
        val finalBufSize = wanted.coerceAtLeast(
            if (minBufSize > 0) minBufSize else wanted
        )

        headRaw = 0
        headFrames = 0

        if (voiceMode) {
            // 语音模式：VOICE_COMMUNICATION + 听筒路由（启用 AEC 管线）
            Log.i(TAG, "AudioPlayer 语音模式")
            routeToEarpiece(true)
            if (!tryCreateTrack(
                    AudioAttributes.USAGE_VOICE_COMMUNICATION, AudioAttributes.CONTENT_TYPE_SPEECH,
                    audioSessionId, finalBufSize
                )
            ) {
                Log.w(TAG, "VOICE_COMMUNICATION 不可用，回退到 MEDIA")
                isVoiceCommActive = false
                routeToEarpiece(false)
                if (!tryCreateTrack(
                        AudioAttributes.USAGE_MEDIA, AudioAttributes.CONTENT_TYPE_MUSIC,
                        0, finalBufSize
                    )
                ) {
                    Log.e(TAG, "AudioTrack 创建彻底失败")
                    return false
                }
            }
        } else {
            // 音乐模式：MEDIA + 默认扬声器（不改变音频路由，避免干扰 AudioRecord）
            Log.i(TAG, "AudioPlayer 音乐模式")
            isVoiceCommActive = false
            if (!tryCreateTrack(
                    AudioAttributes.USAGE_MEDIA, AudioAttributes.CONTENT_TYPE_MUSIC,
                    0, finalBufSize
                )
            ) {
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

        Log.i(TAG, "AudioPlayer 已启动（${if (voiceMode) "语音模式" else "音乐模式"}·${if (isVoiceCommActive) "VOICE_COMMUNICATION" else "MEDIA"}·缓冲 ${finalBufSize / 4} 帧·low-latency)")
        return true
    }

    /**
     * 固定 48 kHz 立体声 + 低延迟路径。
     *
     * `PERFORMANCE_MODE_LOW_LATENCY`（API 26+，本项目 minSdk=29）让系统优先
     * 走 fast mixer 通路：AudioTrack 不再被塞进 192 帧的常规混音块，
     * `getPlaybackHeadPosition()` 的粒度也随之变细，反向播放的播放头换算
     * 因此更贴近真实出声位置。
     */
    private fun tryCreateTrack(
        usage: Int, contentType: Int,
        sessionId: Int, bufSize: Int
    ): Boolean {
        return try {
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
                .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
                .apply { if (sessionId > 0) setSessionId(sessionId) }
                .build()
            isVoiceCommActive = (usage == AudioAttributes.USAGE_VOICE_COMMUNICATION)
            true
        } catch (e: Exception) {
            Log.e(TAG, "AudioTrack 创建失败", e)
            audioTrack = null
            false
        }
    }

    /**
     * 阻塞写入解码后的 PCM（立体声交错）。
     * 返回 false 表示 track 不可用或写入失败（上层据此停止本次播放）。
     */
    fun writeBlocking(pcm: ShortArray, offset: Int, length: Int): Boolean {
        if (length <= 0) return true
        return synchronized(trackLock) {
            val track = audioTrack ?: return@synchronized false
            var off = offset
            var remaining = length
            while (remaining > 0) {
                val n = try {
                    track.write(pcm, off, remaining)
                } catch (e: Exception) {
                    Log.e(TAG, "write 失败", e)
                    return@synchronized false
                }
                if (n <= 0) return@synchronized false
                off += n
                remaining -= n
                writtenFrames += n / CHANNELS
            }
            true
        }
    }

    /** 已写入但尚未播放的帧数（AudioTrack 缓冲深度） */
    fun pendingFrames(playedFrames: Long): Long = synchronized(trackLock) {
        (writtenFrames - playedFrames).coerceAtLeast(0)
    }

    /**
     * 实际协商到的缓冲深度（帧），用于启动日志。
     *
     * 注意：Android **没有**公开 `AudioTrack.getLatency()`（该 API 是 system-api，
     * 不在公开 SDK 里）。本端不做"声卡自报延迟"的单独测量——
     * [playbackHeadFrames] 返回的已经是混音器消费到的位置，
     * 由此算出的"播放头那一帧的到达年龄"天然包含 AudioTrack 缓冲，
     * 只少一个混音周期（典型 5–20 ms）。
     */
    fun bufferFrames(): Int = try {
        audioTrack?.getBufferSizeInFrames() ?: 0
    } catch (_: Exception) {
        0
    }

    /**
     * 设备时钟采样：优先用 [AudioTrack.getTimestamp] 给出的
     * “播放位置 + CLOCK_MONOTONIC 时刻”，它不包含协程被调度回来的抖动；
     * 不可用时返回 null，由调用方退回 `(playbackHeadFrames, System.nanoTime())`。
     */
    fun deviceClockSample(): Pair<Long, Long>? = synchronized(trackLock) {
        val track = audioTrack ?: return@synchronized null
        val ts = AudioTimestamp()
        try {
            if (track.getTimestamp(ts) && ts.nanoTime > 0L) {
                return@synchronized ts.framePosition to ts.nanoTime
            }
        } catch (_: Exception) {
        }
        null
    }

    /**
     * 已播放的设备帧数（处理 32 位回绕）。
     *
     * `getPlaybackHeadPosition()` 只在 track flush/reset 时归零，
     * 因此这里用 Int 模运算求增量，天然跨回绕。
     */
    fun playbackHeadFrames(): Long = synchronized(trackLock) {
        val track = audioTrack
        if (track == null) {
            headFrames
        } else {
            // 读失败时回落到上次 raw，delta = 0，不推进播放头
            val raw = try {
                track.playbackHeadPosition
            } catch (_: Exception) {
                headRaw
            }
            val delta = raw - headRaw
            headRaw = raw
            if (delta > 0) headFrames += delta.toLong()
            headFrames
        }
    }

    /** 丢弃未播放数据并重置播放头（重同步 / 新会话） */
    fun flush() {
        synchronized(trackLock) {
            val track = audioTrack ?: return@synchronized
            try {
                track.pause()
                track.flush()
                track.play()
            } catch (_: Exception) {
            }
            headRaw = 0
            headFrames = 0
            writtenFrames = 0
        }
    }

    /**
     * AudioTrack 被系统回收/失效后原地重建。
     *
     * 长时间静音欠载后，部分设备会让 `write()` 返回错误；上层不能因此永久
     * 退出反向播放协程，否则只能整条链路重连。这里复用最近一次启动参数
     * （语音模式 / AEC sessionId）重建 track，调用方随后重试写入。
     */
    fun recover(): Boolean {
        Log.w(TAG, "AudioTrack 失效，尝试原地重建")
        // 保留累计播放头：DriftController 用 device_frames 做差分，若重建后
        // 归零会被 saturating_sub 卡死直到重新追平旧计数。新 track 从 0 开始
        // 计数，这里把旧累计值作为基准，后续 raw 增量继续累加。
        val baseHead = synchronized(trackLock) { headFrames }
        stop()
        val ok = start(startedSessionId, startedVoiceMode)
        if (ok) {
            synchronized(trackLock) {
                headRaw = 0
                headFrames = baseHead
                // 旧 track 未播出的数据已丢弃，新 track 初始欠载为 0
                writtenFrames = baseHead
            }
        }
        return ok
    }

    fun stop() {
        synchronized(trackLock) {
            try { audioTrack?.pause() } catch (_: Exception) {}
            try { audioTrack?.flush() } catch (_: Exception) {}
            try { audioTrack?.stop() } catch (_: Exception) {}
            try { audioTrack?.release() } catch (_: Exception) {}
            audioTrack = null
            headRaw = 0
            headFrames = 0
            writtenFrames = 0
        }
        routeToEarpiece(false)
    }

    private fun routeToEarpiece(toEarpiece: Boolean) {
        try {
            audioManager?.mode =
                if (toEarpiece) AudioManager.MODE_IN_COMMUNICATION else AudioManager.MODE_NORMAL
            @Suppress("DEPRECATION")
            audioManager?.isSpeakerphoneOn = !toEarpiece
        } catch (e: Exception) {
            Log.w(TAG, "路由设置失败", e)
        }
    }
}
