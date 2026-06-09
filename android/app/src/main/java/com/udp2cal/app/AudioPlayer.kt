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
 */
class AudioPlayer(private val context: Context) {
    companion object {
        private const val TAG = "AudioPlayer"
        private const val SAMPLE_RATE = 48000
        private const val BUFFER_MS = 40
        private const val CHANNELS = 2 // 立体声
    }

    @Volatile private var audioTrack: AudioTrack? = null
    private var audioManager: AudioManager? = null

    /** 是否成功启用 VOICE_COMMUNICATION 模式（含 AEC 能力） */
    @Volatile var isVoiceCommActive: Boolean = false

    fun start(audioSessionId: Int = 0, voiceMode: Boolean = true): Boolean {
        audioManager = context.getSystemService(Context.AUDIO_SERVICE) as? AudioManager

        val bufferSize = (SAMPLE_RATE * BUFFER_MS / 1000) * 2 * CHANNELS
        val minBufSize = AudioTrack.getMinBufferSize(
            SAMPLE_RATE,
            AudioFormat.CHANNEL_OUT_STEREO,
            AudioFormat.ENCODING_PCM_16BIT
        )
        val finalBufSize = bufferSize.coerceAtLeast(minBufSize)

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

    fun write(pcm: ShortArray, offset: Int, length: Int) {
        val track = audioTrack ?: return
        try {
            track.write(pcm, offset, length)
        } catch (e: Exception) {
            Log.e(TAG, "write 失败", e)
        }
    }

    fun stop() {
        try { audioTrack?.stop() } catch (_: Exception) {}
        try { audioTrack?.release() } catch (_: Exception) {}
        audioTrack = null
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

    fun isPlaying(): Boolean = audioTrack?.playState == AudioTrack.PLAYSTATE_PLAYING
}
