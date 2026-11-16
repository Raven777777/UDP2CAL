package com.udp2cal.app.service

import android.content.Context
import android.os.SystemClock
import android.util.Log
import com.udp2cal.app.AudioPlayer
import com.udp2cal.app.StreamResampler
import com.udp2cal.app.Udp2CalProtocol
import com.udp2cal.app.UdpSender
import com.udp2cal.app.native.OpusDecoder as ReverseOpusDecoder
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import java.net.DatagramPacket
import java.util.concurrent.TimeUnit
import java.util.concurrent.locks.ReentrantLock
import kotlin.coroutines.coroutineContext

/**
 * 反向链路：接收（只插入）+ 播放（按播放头拉取）。
 *
 * 实例与 Service 同生命周期：抖动缓冲 / 漂移控制器 / 设备速率估计器的状态都在
 * Rust 堆上，原生句柄只在这里持有，服务停机时才归还（见 [closeNative]）。
 * 每个采集会话只重建解码器、播放器与两条协程（[startSession] / [teardownSession]）。
 *
 * 接收与播放协程各钉在一条专用单线程上（rxDispatcher / playDispatcher）：
 * 协程在 delay/yield 后可能被 Dispatchers.IO 换池线程，优先级无法钉住。
 */
internal class ReverseLink(
    private val context: Context,
    private val status: StatusBus,
    private val scope: CoroutineScope,
    private val rxDispatcher: CoroutineDispatcher,
    private val playDispatcher: CoroutineDispatcher,
) {
    companion object {
        /** RTP 连续空闲超过该值：下个包到达时重置抖动缓冲，避免本地静音把播放头推过后被判 late */
        private const val REVERSE_IDLE_RESET_MS = 1_000L

        private const val TAG = SERVICE_TAG
    }

    // ─── 接收协程插入，播放协程拉取 ───
    private val linkLock = ReentrantLock()
    private val linkSignal = linkLock.newCondition()
    private val jitter = Udp2CalProtocol.JitterBuffer()
    private val drift = Udp2CalProtocol.DriftController()
    private val deviceRate = Udp2CalProtocol.DeviceRateEstimator()

    /** 当前会话的反向 SSRC（0 = 反向未启用）；接收协程过滤，控制协程经 [applyRemoteSsrc] 写入 */
    @Volatile private var activeSsrc = 0
    // 创建于采集线程、读取于播放线程，必须 volatile
    @Volatile private var decoder: ReverseOpusDecoder? = null
    @Volatile private var player: AudioPlayer? = null
    @Volatile private var rxJob: Job? = null
    @Volatile private var playJob: Job? = null
    @Volatile private var lastReverseStartAt = 0L
    /** 最近一次通过校验的反向 RTP 到达时刻（monotonic ms）；接收/播放协程共享 */
    @Volatile private var lastReversePacketAt = 0L

    /** 新采集会话：建解码器并起接收协程 */
    fun startSession(sender: UdpSender) {
        activeSsrc = 0
        decoder = ReverseOpusDecoder(Udp2CalProtocol.OPUS_SAMPLE_RATE, channels = 2).also {
            if (!it.start()) Log.w(TAG, "反向解码器初始化失败，继续运行")
        }
        launchReverseReceiver(sender)
    }

    /**
     * 反向接收协程在**整个会话**内只启动一次：
     * `receive()` 是阻塞调用，协程取消唤不醒它；若随反向开关反复创建，
     * 每次都会留下一个卡在 recvfrom 上的线程。会话结束时关闭 socket 即可退出。
     */
    private fun launchReverseReceiver(sender: UdpSender) {
        if (rxJob?.isActive == true) return
        rxJob = scope.launch(rxDispatcher) { reverseRxLoop(sender) }
    }

    /** 反向播放器按需创建（AEC 需要麦克风的 sessionId） */
    fun ensurePlayer(voiceMode: Boolean, sessionId: Int) {
        if (player != null) return
        player = AudioPlayer(context).also { p ->
            if (!p.start(sessionId, voiceMode)) {
                Log.w(TAG, "反向播放器初始化失败，继续运行")
            }
        }
    }

    /**
     * 控制面下发的反向 SSRC。
     *
     * 变化到非零 → 起播放协程；变化到 0 → 停播放并清缓冲；不变但播放协程
     * 已退出（AudioTrack/解码器异常）→ 冷却 2 s 后自动重启。
     */
    suspend fun applyRemoteSsrc(ssrc: Int) {
        if (ssrc != activeSsrc) {
            activeSsrc = ssrc
            if (ssrc == 0) {
                // ACK 明确表示反向未启用：必须停止播放并清空缓冲
                stopAndJoin()
            } else {
                startPlayback()
            }
        } else if (
            ssrc != 0 &&
            playJob?.isActive != true &&
            SystemClock.elapsedRealtime() - lastReverseStartAt > 2000
        ) {
            // 播放协程可能因 AudioTrack/解码器异常退出，而 SSRC 没变；
            // 不能只靠 activeSsrc 变化触发重启，否则会永久静音。
            Log.w(TAG, "反向播放协程已退出，自动重启")
            startPlayback()
        }
    }

    /** ACK 超时：旧会话已不可用，停播放并清空 SSRC */
    suspend fun resetSession() {
        stopAndJoin()
        activeSsrc = 0
    }

    private suspend fun startPlayback() {
        // 先等旧循环真正退出：旧循环仍可能持有同一个 native 解码器和
        // AudioPlayer，直接并发启动会出现两个消费者抢写/解码。
        // 但控制循环不能被 AudioTrack/HAL 卡死，超时后记录并继续。
        val old = playJob
        playJob = null
        val joined = withTimeoutOrNull(1000) {
            old?.cancelAndJoin()
            true
        } ?: false
        if (!joined) Log.w(TAG, "旧反向播放协程 1 s 内未退出，继续启动新会话")
        linkLock.lock()
        try {
            jitter.reset()
            linkSignal.signalAll()
        } finally {
            linkLock.unlock()
        }
        // 接收协程与会话同生命周期（见 launchReverseReceiver）
        player?.flush()
        lastReverseStartAt = SystemClock.elapsedRealtime()
        linkLock.lock()
        try {
            lastReversePacketAt = 0L
        } finally {
            linkLock.unlock()
        }
        playJob = scope.launch(playDispatcher) { reversePlayLoop() }
        status.update {
            it.copy(reverseAudio = true, reverseBw = "FB 48 kHz 立体声")
        }
    }

    fun stop() {
        // 只停播放协程；接收协程继续保持阻塞 receive，等会话结束关闭 socket。
        // 非 suspend 版本用于服务停止路径，协程退出由服务作用域取消兜底。
        playJob?.cancel()
        playJob = null
        linkLock.lock()
        try {
            jitter.reset()
            lastReversePacketAt = 0L
            linkSignal.signalAll()
        } finally {
            linkLock.unlock()
        }
        player?.flush()
        status.update {
            it.copy(reverseAudio = false, reverseBitrateKbps = 0f, reverseJitter = "")
        }
    }

    /** 需要保证旧循环已退出时使用（会话切换/ACK 超时/播放循环退出）。 */
    suspend fun stopAndJoin() = withContext(NonCancellable) {
        val old = playJob
        playJob = null
        old?.cancelAndJoin()
        linkLock.lock()
        try {
            jitter.reset()
            lastReversePacketAt = 0L
            linkSignal.signalAll()
        } finally {
            linkLock.unlock()
        }
        player?.flush()
        status.update {
            it.copy(reverseAudio = false, reverseBitrateKbps = 0f, reverseJitter = "")
        }
    }

    /** 会话收尾：停接收协程与本次会话的音频对象（原生句柄保留到服务停机） */
    fun teardownSession() {
        rxJob?.cancel()
        rxJob = null
        decoder?.stop()
        decoder = null
        player?.stop()
        player = null
        activeSsrc = 0
    }

    /** 待 join 的协程快照；顺序与 onDestroy 的等待顺序一致 */
    fun jobs(): List<Job> = listOfNotNull(rxJob, playJob)

    /** 服务停机：归还 JitterBuffer/DriftController/DeviceRateEstimator 的 Rust 堆 */
    fun closeNative() {
        runCatching { jitter.close() }
        runCatching { drift.close() }
        runCatching { deviceRate.close() }
    }

    /**
     * 反向接收：**阻塞** receive，不使用 `soTimeout` 驱动。
     * 只做校验与入队，绝不在此解码。
     */
    private suspend fun reverseRxLoop(sender: UdpSender) {
        val sock = sender.reverseSock ?: return
        val expectedAddr = sender.getRemoteAddress()
        val buf = ByteArray(2048)
        val pkt = DatagramPacket(buf, buf.size)
        // 复用解析结果数组：本循环每 10 ms 一包，避免每包分配 LongArray/RtpPacket
        val rtp = LongArray(6)
        var byteCount = 0L
        var lastReport = System.currentTimeMillis()

        // 接收线程是音频数据通路上游：被后台线程抢占一次就等于丢包。
        // 专用线程（rxDispatcher）不迁移，设置一次即全程有效；
        // 线程只服务本循环，随会话结束停用，无需退出还原。
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO)
        while (coroutineContext.isActive) {
            pkt.length = buf.size
            try {
                sock.receive(pkt)
            } catch (e: Exception) {
                if (!coroutineContext.isActive || sock.isClosed) break
                // 不忙等：socket 持续报错时也要让出线程，避免占满 IO 调度器
                delay(10)
                continue
            }
            val len = pkt.length
            if (expectedAddr != null && pkt.address != expectedAddr) continue
            // 槽位顺序见 Udp2CalProtocol.parseRtpInto
            if (!Udp2CalProtocol.parseRtpInto(buf, len, rtp)) continue
            val ssrc = activeSsrc
            if (ssrc == 0 || rtp[3].toInt() != ssrc) continue
            val marker = rtp[0] != 0L
            val seq = rtp[1].toInt()
            val timestamp = rtp[2]
            val payloadOffset = rtp[4].toInt()
            val payloadLength = rtp[5].toInt()

            val nowMono = SystemClock.elapsedRealtime()
            linkLock.lock()
            try {
                // 长时间没有 RTP（Windows 暂停播放/端点重建）后，本地静音已经
                // 把 nextTs 推到接近墙钟。恢复的第一个包必须先重置时间线，
                // 否则会被当作 late 丢掉，听起来就是“恢复后一直没声音”。
                if (lastReversePacketAt != 0L &&
                    nowMono - lastReversePacketAt > REVERSE_IDLE_RESET_MS
                ) {
                    Log.i(TAG, "反向 RTP 空闲 ${nowMono - lastReversePacketAt}ms，重置抖动缓冲")
                    jitter.reset()
                }
                lastReversePacketAt = nowMono
                jitter.insert(
                    seq, timestamp, marker,
                    buf, payloadOffset, payloadLength
                )
                linkSignal.signalAll()
            } finally {
                linkLock.unlock()
            }
            byteCount += len

            val now = System.currentTimeMillis()
            if (now - lastReport >= 1000) {
                if (activeSsrc != 0) {
                    // 抖动缓冲的“生病”指标必须能被眼睛看到：只写在日志里没人翻。
                    // 每秒取一次（JNI getter，代价可忽略）。
                    val jbSummary = buildList {
                        if (jitter.late > 0) add("迟到 ${jitter.late}")
                        if (jitter.overflow > 0) add("溢出 ${jitter.overflow}")
                        if (jitter.plcFrames > 0) add("PLC ${jitter.plcFrames}(峰 ${jitter.maxPlcRun})")
                        if (jitter.resyncs > 0) add("重同步 ${jitter.resyncs}")
                    }.joinToString(" · ")
                    status.update {
                        it.copy(
                            reverseBitrateKbps = byteCount * 8f / (now - lastReport),
                            reverseJitter = jbSummary
                        )
                    }
                }
                byteCount = 0
                lastReport = now
            }
        }
    }

    /**
     * 写 PCM；AudioTrack 失效时原地重建并重试，而不是让播放协程永久退出。
     * 长时间静音欠载后部分设备会回收 track，重建后链路无需整条重连。
     */
    private suspend fun writeOrRecover(
        player: AudioPlayer,
        pcm: ShortArray,
        length: Int,
    ): Boolean {
        var failures = 0
        while (coroutineContext.isActive) {
            if (player.writeBlocking(pcm, 0, length)) return true
            failures++
            if (failures == 1 || failures % 50 == 0) {
                Log.w(TAG, "反向 AudioTrack 写入失败（连续 $failures 次），重建后重试")
            }
            if (failures >= 100) {
                Log.e(TAG, "反向 AudioTrack 连续重建失败，交由播放协程监督者重启")
                return false
            }
            if (!player.recover()) delay(100) else delay(20)
        }
        return false
    }

    /**
     * 反向播放：播放头是唯一时钟。
     *
     * 预缓冲 4 帧后开始写；每帧的播放/PLC 决策完全由 `playhead_ts` 决定，
     * `linkLock.wait(5)` 只是"等包"的存活等待，不承担节拍。
     */
    private suspend fun reversePlayLoop() {
        // 专用线程（playDispatcher）不迁移，线程体首行提优先级：
        // 设一次即全程有效。
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO)
        // 播放器在 AudioRecord 之后创建（为了复用 sessionId 接 AEC）。
        // ACK 可能先于麦克风初始化到达，这里必须等依赖就绪而不是直接退出，
        // 否则反向串流会永久沉默。
        var localPlayer = player
        var localDecoder = decoder
        // 首次启动时麦克风/AudioTrack 可能比 ACK 晚很久才就绪（冷启动、
        // 权限/音频服务初始化）。这里不能用固定超时：一旦超时退出，
        // 之后 activeSsrc 不再变化，就再也不会重启播放协程，
        // 表现为"首次连接反向没声音，重连一次才好"。
        while ((localPlayer == null || localDecoder == null) && coroutineContext.isActive) {
            delay(10)
            localPlayer = player
            localDecoder = decoder
        }
        val player = localPlayer ?: return
        val dec = localDecoder ?: return
        val payload = ByteArray(Udp2CalProtocol.OPUS_MAX_PAYLOAD)
        // FEC 前瞻用的独立缓冲：pull 的 payload 与它互不干扰
        val fecPayload = ByteArray(Udp2CalProtocol.OPUS_MAX_PAYLOAD)
        var fecFrames = 0 // 本统计窗口内用 LBRR 重建成功的帧数
        val frameSamples = Udp2CalProtocol.OPUS_FRAME_SAMPLES * 2 // 立体声交错
        val pcm = ShortArray(frameSamples)
        val silence = ShortArray(frameSamples)
        // 阶跃裁剪的锁外解码缓冲：上限 = PLC_MAX_MS / OPUS_FRAME_MS 帧；
        // trimLens[i] ≥ 0 是正常帧的负载长度，< 0 是缺口帧（锁外走 PLC）
        val trimMax = Udp2CalProtocol.PLC_MAX_MS / Udp2CalProtocol.OPUS_FRAME_MS
        val trimPayloads = Array(trimMax) { ByteArray(Udp2CalProtocol.OPUS_MAX_PAYLOAD) }
        val trimLens = IntArray(trimMax)
        // 受控重采样：媒体 48 kHz、设备 48 kHz，基础比率 1.0，只做 ±0.5% 微调。
        // 它现在是**唯一**的执行手段（不再有样本级丢/插），重采样器本身是
        // 多相 FIR，所以比率怎么扫都不会在 12 kHz 处起伏。
        val resampler = StreamResampler(2)
        resampler.setSourceRate(Udp2CalProtocol.OPUS_SAMPLE_RATE, Udp2CalProtocol.OPUS_SAMPLE_RATE)
        var lastEpoch = -1L
        var lastLatencyLog = 0L
        var latencyMs = 0
        // 阶跃吸收（方案 C）：与 Windows 端同一套判据与限额
        val step = Udp2CalProtocol.StepAbsorber()
        // 起播直接预缓冲到目标水位，避免"30ms 起播后用几分钟追到 150ms"。
        jitter.setStartPrebufferMs(drift.targetMs())
        var adjust = 0.0
        // 冻结水位反馈（断流 / 预缓冲未出声）：这两种状态下 water 长期低于目标，
        // 若继续积分，积分会压到 -PI_INTEGRAL_MAX 的负饱和；恢复出声后只能按
        // ≈12 ppm/s 慢慢退，稳态水位被顶在目标之上 10–30 ms 长达几分钟，
        // 期间还带着几分音分的音高偏移。与 windows/src/main.rs 解码线程同源。
        var idleFeedback = false
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO)

        try {
        while (coroutineContext.isActive) {
            var code = Udp2CalProtocol.Pull.WAIT
            var pipeMs = 0
            var waterMs = 0
            var jbPlaying = false
            var pulledLen = 0
            var needReset = false
            var reason = ""
            var jbNextTs = 0L
            var jbResyncs = 0L
            var jbOverflow = 0L
            var arrivalNanos = -1L
            var head = 0L
            // 阶跃吸收的本 tick 结论（锁内决定，锁外记日志）
            var stepBefore = 0
            var stepWant = 0
            var stepSkipped = 0
            var stepTotal = 0L
            var stepLimited = false
            var trimFrames = 0
            var trimN = 0

            linkLock.lock()
            try {
                if (jitter.epoch != lastEpoch) {
                    lastEpoch = jitter.epoch
                    needReset = true
                    reason = jitter.lastRebaseReason
                    jbNextTs = jitter.nextTs
                    jbResyncs = jitter.resyncs
                    jbOverflow = jitter.overflow
                }
                head = player.playbackHeadFrames()
                // Android 没有独立 PCM 环：解码输出直接阻塞写进 AudioTrack，
                // 所以"已解码未播完"的只有播放缓冲这一段，ring 恒为 0。
                // 生产节流因此天然由 writeBlocking 承担，水位只做缺口判定。
                val pipeFrames = player.pendingFrames(head)
                pipeMs = (pipeFrames * 1000L / Udp2CalProtocol.OPUS_SAMPLE_RATE).toInt()

                // ── 阶跃吸收（方案 C）：持续超额才裁，先按水位拉取 ──
                // 判据：occupancy + pipe 连续 STEP_TRIM_HOLD_MS 超过
                // target + STEP_TRIM_EXCESS_MS。单次 90 ms 攒批不触发。
                // 命中后逐帧解码再丢 PCM（见下），不是清槽位硬跳。
                val beforeWater = jitter.occupancyMs()
                val beforeTotal = beforeWater + pipeMs
                val decision = step.decide(
                    beforeWater,
                    pipeMs,
                    jitter.playing,
                    // 与漂移控制器同源：目标水位以控制器里的为准
                    drift.targetMs(),
                )
                if (decision.frames > 0) {
                    stepBefore = beforeTotal
                    stepWant = decision.frames
                    stepLimited = decision.limited
                    trimFrames = if (decision.limited) 0 else decision.frames
                }

                // 阶跃裁剪必须沿播放顺序**解码后丢弃 PCM**：只推进缓冲游标会让
                // 有状态 Opus 解码器与发送端脱节（接缝/音染）。这里最多 12 帧，
                // 且每分钟最多 3 次。锁内只收集负载与帧型，解码移到锁外（见
                // unlock 之后）：linkLock 同时保护网络的 insert，攒批交付时正是
                // 两边同时要锁的时刻，锁内解码（0.1–1 ms/帧 × ≤12）会拖住接收端。
                var guard = trimFrames
                while (guard-- > 0) {
                    val c = jitter.pullTrim(payload)
                    when (c) {
                        Udp2CalProtocol.Pull.PACKET -> {
                            val len = jitter.pulledLength
                            payload.copyInto(trimPayloads[trimN], 0, 0, len)
                            trimLens[trimN] = len
                            trimN++
                            stepSkipped++
                        }
                        Udp2CalProtocol.Pull.PLC -> {
                            trimLens[trimN] = -1
                            trimN++
                            stepSkipped++
                        }
                        Udp2CalProtocol.Pull.SILENCE -> stepSkipped++
                        else -> break
                    }
                }
                stepTotal = jitter.stepTrims

                // Android 没有独立 PCM 环：pendingFrames 就是"已提交、未播出"的
                // 全部余量，用它做 PLC 截止判据（设备缓冲不是固定延迟）。
                code = jitter.pull(pipeMs, pipeMs, payload)
                // 可听点之前还压着的媒体量（设备帧 → 媒体样本要用当前比率换算）
                val queuedMedia = (pipeFrames.toDouble() * resampler.ratio).toLong()
                val audibleTs = (jitter.nextTs - queuedMedia).coerceAtLeast(0L)
                // 延迟估算：可听点那一帧是多久以前到达的
                arrivalNanos = jitter.arrivalNanosAt(audibleTs)
                // 单一口径水位：抖动缓冲 + 解码/播放缓冲（声卡已接手的那段）
                waterMs = jitter.waterMs() + pipeMs
                jbPlaying = jitter.playing
                pulledLen = jitter.pulledLength
            } finally {
                linkLock.unlock()
            }

            // 阶跃裁剪帧的解码在锁外补做：只推进解码器状态，PCM 不进
            // AudioTrack。needReset 会重建解码器，此时不能补解码。
            if (!needReset) {
                for (i in 0 until trimN) {
                    val len = trimLens[i]
                    if (len >= 0) dec.decode(trimPayloads[i], 0, len, pcm) else dec.plc(pcm)
                }
            }

            if (needReset) {
                // 只有真正的重同步才值得告警（会 flush 掉已排队 PCM = 一次破音）；
                // 会话开始的 epoch 变化属于正常复位，不写日志
                if (reason.isNotEmpty()) {
                    Log.w(
                        TAG,
                        "反向重同步：$reason | nextTs=$jbNextTs 水位=${waterMs}ms " +
                            "重同步数=$jbResyncs 溢出=$jbOverflow"
                    )
                }
                dec.reset()
                drift.reset()
                deviceRate.reset()
                step.reset()
                resampler.reset()
                player.flush()
            }

            // 阶跃吸收的日志（锁外：Log 会走 binder）。水位一律用**总水位**
            // （jitter + pipe）口径，与判据一致；限额分支每 60 s 窗口只放行一条：
            // 被压制时水位不会自己降下来，不节流就是 ≈200 条/秒。
            if (stepWant > 0) {
                if (stepLimited) {
                    if (step.takeLimitedLog()) {
                        Log.w(
                            TAG,
                            "[Step] 水位 ${stepBefore}ms（目标 ${drift.targetMs()}）" +
                                "命中阶跃吸收，但每分钟 ${Udp2CalProtocol.STEP_TRIM_MAX_PER_MIN} 次限额已满" +
                                "（本窗口已用 ${step.trimsInWindow()} 次）：本次只记录不裁剪"
                        )
                    }
                } else if (stepSkipped > 0) {
                    Log.i(
                        TAG,
                        "[Step] 阶跃吸收：整帧跳 $stepSkipped / $stepWant 帧" +
                            "（${stepSkipped * Udp2CalProtocol.OPUS_FRAME_MS}ms），" +
                            "水位 ${stepBefore}→${waterMs}ms，累计 $stepTotal 帧"
                    )
                }
            }

            // ── 比率 = 设备时钟前馈(管速率) + 水位反馈(管长期残余) ──
            // 网络到达抖动只进 jitter，绝不进重采样比率。
            // 设备钟差观测：优先 AudioTrack 时间戳（位置+单调时刻），
            // 避免把协程调度抖动误当成设备速率差；不可用时退回播放头。
            val clock = player.deviceClockSample()
            if (clock != null) {
                deviceRate.observe(clock.first, clock.second)
            } else {
                deviceRate.observe(head, System.nanoTime())
            }
            val ff = deviceRate.feedforward() ?: 0.0
            val nowMono = SystemClock.elapsedRealtime()
            val rtpIdle = lastReversePacketAt == 0L ||
                nowMono - lastReversePacketAt > 500L
            // 反馈只在**真的在出声**时才评估。只看 rtpIdle 不够：预缓冲阶段
            // （jitter 还没攒够 startPrebuffer、playing=false）水位必然低于目标，
            // 照常喂 PI 会把积分压到负饱和；开播后只能按 ki·|err| 慢慢退，
            // 稳态水位因此停在目标之上 10–30 ms。现场日志指纹：开播瞬间
            // `反馈 -1100ppm`，之后 -457 → -294 → +21 → +65 缓慢爬升。
            val feedbackOn = !rtpIdle && jbPlaying
            val du = if (!feedbackOn) {
                if (!idleFeedback) {
                    idleFeedback = true
                    drift.reset()
                }
                0.0
            } else {
                if (idleFeedback) {
                    idleFeedback = false
                    drift.reset()
                }
                drift.update(waterMs, head)
            }
            adjust = ff + du
            // 两支路相合后仍须守比率钳位：前馈和反馈各自钳过了，乘积可能越界。
            adjust = adjust.coerceIn(
                -Udp2CalProtocol.RESAMPLE_MAX_ADJUST,
                Udp2CalProtocol.RESAMPLE_MAX_ADJUST,
            )
            resampler.setAdjust(adjust)

            // ── 总延迟估算 ──
            // 口径：正在播放的那一帧是多久以前到达本机的。AudioTrack 的播放头
            // 已是"混音器消费到的位置"，所以这个年龄天然包含播放缓冲，
            // 只少一个混音周期（5–20 ms，公开 SDK 无法读出）。
            if (arrivalNanos > 0L) {
                val ageMs = ((System.nanoTime() - arrivalNanos) / 1_000_000L).toInt()
                latencyMs = ageMs.coerceAtLeast(0)
                val now = System.currentTimeMillis()
                if (now - lastLatencyLog >= 10_000) {
                    lastLatencyLog = now
                    val ffPpm = ff * 1e6
                    Log.i(
                        TAG,
                        "反向延迟≈${latencyMs}ms 水位=${waterMs}ms（抖动 ${jitter.waterMs()} + 播放 $pipeMs，目标 ${drift.targetMs()}）" +
                            " 比率=${"%.3f".format(adjust * 100)}%（反馈 ${"%.0f".format(du * 1e6)}ppm / 设备前馈 ${"%.1f".format(ffPpm)}ppm，σ ${"%.1f".format(deviceRate.sigmaPpm())}ppm ${deviceRate.buckets()}桶）" +
                            " FEC=$fecFrames"
                    )
                    fecFrames = 0
                }
            }

            when (code) {
                Udp2CalProtocol.Pull.PACKET -> {
                    val n = dec.decode(payload, 0, pulledLen, pcm)
                    if (n <= 0) continue // 解码失败按丢包处理，水位见底后 PLC
                    // PLC 帧和真实帧走同一条时间线，所以都过同一个重采样器
                    val produced = resampler.process(pcm, 0, n / 2)
                    if (produced > 0) {
                        if (!writeOrRecover(player, resampler.buffer(), produced)) return
                    }
                }

                Udp2CalProtocol.Pull.PLC -> {
                    // 先用 Opus 带内 FEC 重建这一帧：丢帧的 LBRR 冗余在**下一个
                    // 包里**，所以前瞻一个包（不消费，下一轮 pull 仍会正常取它）
                    // 来还原；下一个包也还没到时才退回 PLC。发送端没开
                    // in-band FEC 时 libopus 自动退化成 PLC，故不会更差。
                    // 现场最常见的就是"包没丢、只是来晚了"（AP 攒批/省电）：
                    // 一次 >100 ms 的停顿会连丢 3 帧，以前全 PLC，听起来就是
                    // 一下音量塌陷。判据与 windows/src/main.rs 同源。
                    var fecLen = 0
                    linkLock.lock()
                    try {
                        fecLen = jitter.peek(fecPayload)
                    } finally {
                        linkLock.unlock()
                    }
                    var n = if (fecLen > 0) dec.decodeFec(fecPayload, 0, fecLen, pcm) else -1
                    if (n > 0) {
                        fecFrames++
                    } else {
                        n = dec.plc(pcm)
                    }
                    if (n > 0) {
                        val produced = resampler.process(pcm, 0, n / 2)
                        if (produced > 0) {
                            if (!writeOrRecover(player, resampler.buffer(), produced)) return
                        }
                    } else {
                        // 生成不出 PLC：补静音，同样过重采样器以保持相位连续
                        val produced = resampler.process(silence, 0, silence.size / 2)
                        if (produced > 0) {
                            if (!writeOrRecover(player, resampler.buffer(), produced)) return
                        }
                    }
                }

                Udp2CalProtocol.Pull.SILENCE -> {
                    // 连续 PLC 已达上限：输出静音但继续推进媒体时间
                    val produced = resampler.process(silence, 0, silence.size / 2)
                    if (produced > 0) {
                        if (!writeOrRecover(player, resampler.buffer(), produced)) return
                    }
                }

                else -> {
                    // 没有可播放的包（空闲/起播预缓冲）：本机补静音保持
                    // AudioTrack 不断流。发送端不再为“保活”合成静音，
                    // 因此这里的本地输出是唯一的空闲期供数来源。
                    if (pipeMs <= Udp2CalProtocol.PLC_LEAD_MS) {
                        val produced = resampler.process(silence, 0, silence.size / 2)
                        if (produced > 0 && !writeOrRecover(player, resampler.buffer(), produced)) return
                    } else {
                        // 播放缓冲还够：等接收协程的 signal（超时只防漏唤醒）
                        linkLock.lock()
                        try {
                            linkSignal.await(5, TimeUnit.MILLISECONDS)
                        } catch (_: InterruptedException) {
                        } finally {
                            linkLock.unlock()
                        }
                    }
                }
            }
        }
        } finally {
            // 播放循环退出，把原生句柄还回去（本循环每连接新建一个）
            step.close()
            resampler.close()
        }
    }
}
