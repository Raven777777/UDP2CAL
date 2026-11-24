package com.udp2cal.app.service

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.media.audiofx.AcousticEchoCanceler
import android.os.SystemClock
import android.util.Log
import androidx.annotation.StringRes
import androidx.core.content.ContextCompat
import com.udp2cal.app.EncoderCfg
import com.udp2cal.app.Prefs
import com.udp2cal.app.R
import com.udp2cal.app.Udp2CalProtocol
import com.udp2cal.app.UdpSender
import com.udp2cal.app.native.OpusEncoder
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.withContext
import kotlinx.coroutines.yield
import java.net.DatagramPacket
import java.security.SecureRandom
import kotlin.coroutines.coroutineContext

/** 语音模式：由编码器快照（而非再次读 Prefs）决定音源与 AEC 路由。 */
internal fun EncoderCfg.voiceMode(): Boolean = signal == 3001

/** 状态卡展示用：当前码率控制模式的**唯一推导点**。 */
internal fun vbrModeOf(vbr: Int, vbrConstraint: Int): VbrMode = when {
    vbr == 0 -> VbrMode.CBR
    vbrConstraint == 1 -> VbrMode.VBR_CONSTRAINED
    else -> VbrMode.VBR
}

/** 状态卡展示用：当前码率控制模式 */
internal fun EncoderCfg.vbrMode(): VbrMode = vbrModeOf(vbr, vbrConstraint)

/**
 * 读返回负值时是否作为设备失效上报。
 *
 * AudioRecord.read 阻塞等待期间调用 stop() 会唤醒它并返回负值；若采集协程
 * 此时已经取消，那是正常停止路径，必须安静退出，不能把“读取失败”写进 UI。
 * 只有协程仍在运行时出现的负返回才是真实设备失效。
 */
internal fun shouldReportCaptureStall(readResult: Int, stillActive: Boolean): Boolean =
    readResult < 0 && stillActive

/** 音源描述见 [AudioSource]；界面按枚举映射资源字符串。 */

/**
 * 正向链路：采集 → 编码 → RTP → 主 socket。
 *
 * 每个采集会话创建一个实例，只持有正向独占状态（编码器、AudioRecord、AEC、
 * 已生效参数快照）。跨链路的生命周期动作通过 [Host] 回给 CaptureLifecycle，
 * 不直接操作任务槽或 Service。
 */
internal class ForwardLink(
    private val context: Context,
    private val status: StatusBus,
    private val dispatcher: CoroutineDispatcher,
    private val host: Host,
) {
    companion object {
        private const val TAG = SERVICE_TAG

        /** 正向 Opus 帧采样数（10 ms @ 48 kHz，单声道） */
        private const val FRAME = Udp2CalProtocol.OPUS_FRAME_SAMPLES
    }

    /** 正向链路向会话生命周期请求的动作 */
    interface Host {
        /** 硬件初始化失败：写错误并停止服务。错误用资源 id，便于切换语言后重取文案。 */
        fun fail(@StringRes id: Int, vararg args: Any)

        /** 采集设备失效：本次链路结束，不再自动重连 */
        fun onCaptureStalled()

        /** 采集链路参数变化，请求重建；返回 false 表示已有重启在途（静默） */
        fun requestLinkRestart(): Boolean

        /** 反向播放器按需创建（AEC 需要麦克风的 sessionId） */
        fun ensurePlayer(voiceMode: Boolean, sessionId: Int)
    }

    private var encoder: OpusEncoder? = null
    // stopCapture 在主线程调用，采集协程在专用线程写，必须 volatile
    @Volatile private var audioRecord: AudioRecord? = null
    private var aecInstance: AcousticEchoCanceler? = null

    /** 已生效的编码器参数快照；null 表示尚未与编码器对齐 */
    @Volatile private var appliedCfg: EncoderCfg? = null
    @Volatile private var appliedSignal: Int = Int.MIN_VALUE
    @Volatile private var appliedTestTone: Boolean = false

    /**
     * 创建并启动正向编码器。
     *
     * 参数一律取**当前 Prefs 的快照**，不取入参：重建链路可能就发生在
     * 用户刚改完设置之后（例如语音↔音乐），用旧入参会把新设置丢掉。
     * 快照同时作为 appliedCfg 的初值，构造与"已生效"状态原子一致。
     */
    fun prepare(testTone: Boolean): EncoderCfg? {
        val cfg0 = EncoderCfg.fromPrefs()
        val enc = OpusEncoder(
            sampleRateHz = Udp2CalProtocol.OPUS_SAMPLE_RATE,
            cfg = cfg0,
        )
        encoder = enc
        appliedCfg = cfg0
        appliedSignal = cfg0.signal
        appliedTestTone = testTone
        if (!enc.start()) {
            status.update { it.copy(fatal = true) }
            host.fail(R.string.error_encoder_init)
            return null
        }
        if (enc.frameSize != FRAME) {
            status.update { it.copy(fatal = true) }
            host.fail(R.string.error_encoder_frame_size, enc.frameSize)
            return null
        }
        return cfg0
    }

    /** 采集热循环；运行在 [dispatcher] 这条专用单线程上（见 CaptureLifecycle 注释） */
    suspend fun run(sender: UdpSender, cfg: EncoderCfg, testTone: Boolean) {
        val enc = encoder ?: return
        val voiceMode = cfg.voiceMode()
        // 会话开始统一清空上一次的音源/AEC 状态；失败分支不必逐处重置。
        status.update { it.copy(audioSource = AudioSource.MIC_DIRECT, aec = AecState.NONE) }
        withContext(dispatcher) {
            if (testTone) runTestToneLoop(enc, sender, voiceMode)
            else runMicCaptureLoop(enc, sender, cfg)
        }
    }

    /** 阻塞在 AudioRecord.read 里的采集线程只有 stop() 能唤醒 */
    fun stopRecord() {
        try { audioRecord?.stop() } catch (_: Exception) {}
    }

    /** 会话收尾：释放本次链路的全部音频对象 */
    fun teardown() {
        try { audioRecord?.stop() } catch (_: Exception) {}
        try { audioRecord?.release() } catch (_: Exception) {}
        audioRecord = null
        encoder?.stop()
        encoder = null
        try { aecInstance?.release() } catch (_: Exception) {}
        aecInstance = null
    }

    private suspend fun runMicCaptureLoop(enc: OpusEncoder, sender: UdpSender, cfg: EncoderCfg) {
        val voiceMode = cfg.voiceMode()
        if (ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            status.update { it.copy(fatal = true) }
            host.fail(R.string.error_mic_init_denied)
            return
        }
        // 采集/编码/发送跑在同一条线程上：提到音频优先级，避免被后台任务抢占
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO)
        val minBufSize = AudioRecord.getMinBufferSize(
            Udp2CalProtocol.OPUS_SAMPLE_RATE,
            AudioFormat.CHANNEL_IN_MONO,
            AudioFormat.ENCODING_PCM_16BIT
        )
        if (minBufSize == AudioRecord.ERROR || minBufSize == AudioRecord.ERROR_BAD_VALUE) {
            status.update { it.copy(fatal = true) }
            host.fail(R.string.error_mic_unsupported)
            return
        }
        val bufferSize = minBufSize.coerceAtLeast(FRAME * 2 * 4)

        // 音源候选（按优先级）：首选音源初始化失败时回退 MIC 直出。
        // 3003=UNPROCESSED / 3004=VOICE_RECOGNITION 都不做系统降噪处理，
        // 用于绕过系统级 AI 降噪对高频的削切。
        val attempts: List<Pair<Int, AudioSource>> = when (cfg.signal) {
            3001 -> listOf(
                MediaRecorder.AudioSource.VOICE_COMMUNICATION to AudioSource.SYSTEM_NS,
                MediaRecorder.AudioSource.MIC to AudioSource.SYSTEM_NS_FALLBACK,
            )
            3003 -> listOf(
                MediaRecorder.AudioSource.UNPROCESSED to AudioSource.RAW_UNPROCESSED,
                MediaRecorder.AudioSource.MIC to AudioSource.MIC_DIRECT_FALLBACK,
            )
            3004 -> listOf(
                MediaRecorder.AudioSource.VOICE_RECOGNITION to AudioSource.VOICE_RECOGNITION,
                MediaRecorder.AudioSource.MIC to AudioSource.MIC_DIRECT_FALLBACK,
            )
            else -> listOf(MediaRecorder.AudioSource.MIC to AudioSource.MIC_DIRECT)
        }
        var source: AudioSource = AudioSource.MIC_DIRECT
        for ((audioSource, label) in attempts) {
            try {
                audioRecord = AudioRecord(
                    audioSource,
                    Udp2CalProtocol.OPUS_SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO,
                    AudioFormat.ENCODING_PCM_16BIT, bufferSize
                )
            } catch (_: Exception) {
            }
            if (audioRecord?.state == AudioRecord.STATE_INITIALIZED) {
                source = label
                break
            }
            audioRecord?.release()
            audioRecord = null
        }

        val record = audioRecord
        if (record == null || record.state != AudioRecord.STATE_INITIALIZED) {
            status.update { it.copy(audioSource = AudioSource.MIC_DIRECT, aec = AecState.NONE, fatal = true) }
            host.fail(R.string.error_mic_init)
            return
        }

        var aecState = AecState.NONE
        if (voiceMode && AcousticEchoCanceler.isAvailable()) {
            try {
                val aec = AcousticEchoCanceler.create(record.audioSessionId)
                if (aec != null) {
                    aec.enabled = true
                    aecInstance = aec
                    aecState = AecState.ENABLED
                } else {
                    aecState = AecState.UNAVAILABLE
                }
            } catch (e: Exception) {
                aecState = AecState.UNAVAILABLE
                Log.w(TAG, "AEC create 失败", e)
            }
        }

        // 反向播放器必须在 AudioRecord 之后创建：复用其 sessionId 才能让
        // AudioTrack 与麦克风同属一条 AEC 管线
        host.ensurePlayer(voiceMode, record.audioSessionId)

        record.startRecording()
        status.update { it.copy(audioSource = source, aec = aecState, isRunning = true) }

        try {
            sendFrames(enc, sender) { buf, off, len ->
                record.read(buf, off, len, AudioRecord.READ_BLOCKING)
            }
        } finally {
            try { record.stop() } catch (_: Exception) {}
        }
    }

    private suspend fun runTestToneLoop(enc: OpusEncoder, sender: UdpSender, voiceMode: Boolean) {
        host.ensurePlayer(voiceMode, 0)
        status.update {
            it.copy(audioSource = AudioSource.TEST_TONE, aec = AecState.NONE, isRunning = true)
        }
        val omega = 2.0 * Math.PI * 1000.0 / Udp2CalProtocol.OPUS_SAMPLE_RATE
        var phase = 0.0
        // 节拍由 sendFrames 的 1 帧整形统一负责，这里只负责把样本填满
        sendFrames(enc, sender) { buf, off, len ->
            for (i in 0 until len) {
                buf[off + i] = (Math.sin(phase) * 16384.0).toInt().toShort()
                phase += omega
                if (phase > 2.0 * Math.PI) phase -= 2.0 * Math.PI
            }
            len
        }
    }

    /**
     * 运行期参数对齐（发送循环每 100 ms 调一次）。
     *
     * 分两类，因为"能不能热改"取决于参数落在哪一层：
     *
     * 1. **编码器参数** —— 码率 / 复杂度 / 带宽 / DTX / VBR / VBR 约束 / FEC / 丢包率 /
     *    信号提示：直接 `OPUS_SET_*`，下一帧生效，不断流、不重连。
     * 2. **采集链路参数** —— 语音↔音乐（决定 AudioRecord 音源是 VOICE_COMMUNICATION
     *    还是 MIC、是否挂 AEC、AudioTrack 是否走语音通路）、测试音开关：
     *    这些都在对象创建时定型，Opus 的 signal 提示改了也没用（音源不变）。
     *    只能重建链路——走既有的 restart 通道，代价是约 1 s 的重连。
     *
     * 第二类先判：重建期间不去打 ctl（新编码器还没建好）。
     */
    private fun applyRuntimeConfig(enc: OpusEncoder) {
        val signal = Prefs.opusSignal
        val testTone = Prefs.testToneMode
        if (signal != appliedSignal || testTone != appliedTestTone) {
            if (host.requestLinkRestart()) {
                Log.i(
                    TAG,
                    "采集链路参数变化（音源 $appliedSignal→$signal，测试音 $appliedTestTone→$testTone），重建采集链路"
                )
            }
            return
        }

        val want = EncoderCfg.fromPrefs()
        if (want == appliedCfg) return
        val ok = enc.update(
            complexity = want.complexity,
            signalType = want.signal,
            bandwidth = want.bandwidth,
            dtx = want.dtx,
            vbr = want.vbr,
            bitrateKbps = want.bitrateKbps,
            fec = want.fec,
            packetLoss = want.packetLoss,
            vbrConstraint = want.vbrConstraint,
        )
        if (!ok) return // 编码器已销毁（退出中），下一轮自然不再调用
        val old = appliedCfg
        appliedCfg = want
        Log.i(
            TAG,
            "编码参数热更新: ${old?.bitrateKbps}→${want.bitrateKbps}k " +
                "cplx=${want.complexity} bw=${want.bandwidth} dtx=${want.dtx} " +
                "vbr=${want.vbr} vbrc=${want.vbrConstraint} fec=${want.fec} pl=${want.packetLoss}"
        )
        status.update {
            it.copy(
                bitrateTargetKbps = want.bitrateKbps,
                vbrMode = want.vbrMode(),
                opusBandwidth = want.bandwidth,
            )
        }
    }

    /**
     * 统一的 10 ms 帧发送循环。
     *
     * 规则（规范 4.3/4.4/8.1）：
     *   * 每帧时间戳 +480，**无条件推进**（DTX/断连也保留媒体时间）；
     *   * 序列号只属于真正发出的包：编码失败/发送失败/DTX/无新鲜 ACK 都不 +1；
     *   * 未连接（无新鲜 ACK）时不编码不发送，时间戳照常推进。
     *
     * **1 帧节拍整形（发送前对齐）**：AudioRecord 的阻塞读天然成簇返回
     * （常见 20 ms 一次）。旧实现在 send 之后才等待，下一次阻塞读会把等待
     * 直接吃掉，同簇的两个 10 ms 帧仍会背靠背发出。这里在 encode/send
     * **之前**对齐 10 ms 网格：先等到本帧槽位再发；落后超过半帧就重锚，
     * 绝不用连续突发追赶。这样一簇两帧会被拆到相邻两个 10 ms 槽位。
     */
    private suspend fun sendFrames(
        enc: OpusEncoder,
        sender: UdpSender,
        readInto: suspend (ShortArray, Int, Int) -> Int,
    ) {
        val pcm = ShortArray(FRAME)
        val packet = ByteArray(Udp2CalProtocol.RTP_MAX_PACKET)
        // 复用同一 DatagramPacket（本函数只在采集线程执行，无跨线程共享）
        val rtpPacket = DatagramPacket(packet, 0)
        val rnd = SecureRandom()
        var seq = rnd.nextInt() and 0xFFFF
        var timestamp = rnd.nextInt()
        var first = true
        var lastSentMono = 0L
        var byteCount = 0L
        var lastReport = System.currentTimeMillis()
        /** 已累积但还不足一帧的样本数 */
        var filled = 0
        /** 节拍网格上的下一个发送时刻（nanoTime） */
        val frameNs = FRAME * 1_000_000_000L / Udp2CalProtocol.OPUS_SAMPLE_RATE
        var nextSendAt = System.nanoTime()
        var lastCfgCheck = 0L
        // Plan §6：发送端隐藏积压打点。read/encode/send 分段计时 + 装配缓冲峰值，
        // 30 s 打一行，用来判断“接收端看着均匀，发送端却越排越多”。
        var statsAt = System.nanoTime()
        var readNs = 0L
        var encodeNs = 0L
        var sendNs = 0L
        var statFrames = 0L
        var maxFilled = 0
        /** 30 s 窗口内最大的节拍迟到量（nanoTime 差；仅诊断） */
        var paceLateMaxNs = 0L
        /** 30 s 窗口内因落后半帧以上而重锚的次数（仅诊断） */
        var paceRebases = 0L

        while (coroutineContext.isActive) {
            // AudioRecord.read 允许返回不足请求量的样本（缓冲区尚未填满）。
            // 必须累积到整 480 采样再编码：补零发送等于丢音频，且会让帧率翻倍
            // （实测 190 包/秒 → 接收端缓冲被灌满，41% 的包被判超窗丢弃）。
            val readStart = System.nanoTime()
            val n = readInto(pcm, filled, FRAME - filled)
            readNs += System.nanoTime() - readStart
            if (n < 0) {
                // 设备失效：不空转，交给外层收尾。正常停止时 stopRecord() 会
                // 唤醒阻塞读并返回负值；此时协程已取消，不能误报“读取失败”。
                if (shouldReportCaptureStall(n, coroutineContext.isActive)) {
                    host.onCaptureStalled()
                }
                return
            }
            if (n == 0) {
                // READ_BLOCKING 下返回 0 很罕见（设备暂时没产出数据）。立即重试
                // 会变成忙等，让出 2 ms 给系统补数据，也给取消一个观察点。
                delay(2)
                continue
            }
            filled += n
            if (filled > maxFilled) maxFilled = filled  // 仅未满一帧的装配余量，不是应用层队列
            if (filled < FRAME) continue
            filled = 0

            val now = System.currentTimeMillis()
            // 每 100 ms 对齐一次 Prefs：设置页的改动在下一帧就生效
            if (now - lastCfgCheck >= 100) {
                lastCfgCheck = now
                applyRuntimeConfig(enc)
            }
            // ── 1 帧节拍整形（在 encode/send 之前）──
            // AudioRecord 的阻塞读通常按 20 ms 成簇返回；旧实现在 send 之后
            // 才等待，下一次 read 会把等待时间直接吃掉，同簇的两帧仍会
            // 背靠背发出。这里先对齐到本帧的 10 ms 槽位再发送。
            val paceNow = System.nanoTime()
            val lateNs = paceNow - nextSendAt
            if (!first && lateNs > paceLateMaxNs) paceLateMaxNs = lateNs
            if (lateNs > frameNs / 2) {
                // 采集/调度已落后超过半帧：重锚到当前时刻，不用突发补发
                nextSendAt = paceNow
                if (!first) paceRebases++
            } else if (lateNs < 0L) {
                val waitNs = -lateNs
                if (waitNs >= 1_000_000L) delay(waitNs / 1_000_000L)
                else yield() // 不足 1 ms：让出执行权，不忙等
            }
            nextSendAt += frameNs

            val nowMono = SystemClock.elapsedRealtime()
            if (sender.isAckFresh(nowMono)) {
                val encStart = System.nanoTime()
                val n = enc.encodeTo(pcm, packet, Udp2CalProtocol.RTP_HEADER_SIZE)
                encodeNs += System.nanoTime() - encStart
                statFrames++
                if (n > 0) {
                    val marker =
                        first || (nowMono - lastSentMono > Udp2CalProtocol.RESYNC_GAP_MS.toLong())
                    Udp2CalProtocol.writeRtpHeader(
                        packet, 0, marker, seq, timestamp, sender.forwardSsrc
                    )
                    val sendStart = System.nanoTime()
                    rtpPacket.length = Udp2CalProtocol.RTP_HEADER_SIZE + n
                    val sent = sender.send(rtpPacket)
                    sendNs += System.nanoTime() - sendStart
                    if (sent) {
                        byteCount += n
                        lastSentMono = nowMono
                        first = false
                        // 序号只属于真正发出的包；DTX/断连不占号（RFC 3550）
                        seq = (seq + 1) and 0xFFFF
                    }
                }
            }
            // 时间戳无条件按媒体时间推进（DTX/断连期间也走），恢复后时间轴自然留缺口
            timestamp += FRAME

            if (now - lastReport >= 1000) {
                val elapsed = (now - lastReport).coerceAtLeast(1)
                status.update { it.copy(bitrateKbps = byteCount * 8f / elapsed) }
                byteCount = 0
                lastReport = now
            }
            val statsElapsed = System.nanoTime() - statsAt
            if (statsElapsed >= 30_000_000_000L) {
                val ms = statsElapsed / 1_000_000.0
                Log.i(
                    TAG,
                    "tx30s frames=$statFrames read=${(readNs / 1e6 / ms * 1000).toInt()}us" +
                        " encode=${(encodeNs / 1e6 / ms * 1000).toInt()}us" +
                        " send=${(sendNs / 1e6 / ms * 1000).toInt()}us" +
                        " assemble_peak=${maxFilled * 1000 / Udp2CalProtocol.OPUS_SAMPLE_RATE}ms" +
                        " pace_late=${paceLateMaxNs / 1_000_000}ms rebase=$paceRebases"
                )
                statsAt = System.nanoTime()
                readNs = 0
                encodeNs = 0
                sendNs = 0
                statFrames = 0
                maxFilled = 0
                paceLateMaxNs = 0
                paceRebases = 0
            }
        }
    }
}
