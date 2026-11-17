//! 接收侧音频：抖动缓冲、解码/播放线程与音频初始化状态。
//!
//! 状态全部挂在 [`Session`] 上：本文件只实现解码线程的启动与运行循环。

use super::Session;
use crate::audio;
use crate::decoder;
use crate::diag::{
    AUDIO_LEVEL_DB, DECODE_BUCKETS, DECODE_DEVICE_MS, DECODE_DU_PPM_BITS, DECODE_FF_PPM_BITS,
    DECODE_JB_WATER_MS, DECODE_PIPE_MS, DECODE_SIGMA_PPM_BITS, DECODE_WATER_TARGET_MS,
    DEVICE_LATENCY_MS, DRIFT_ADJUST_BITS, FEC_FRAMES, LATENCY_MS, STEP_BEFORE_MS, STEP_EVENT,
    STEP_EVENT_LIMITED, STEP_EVENT_TRIMMED, STEP_SKIPPED, STEP_TRIMS, STEP_WANT, STEP_WATER_MS,
    STEP_WINDOW_TRIMS,
};
use crate::logf;
use crate::power;
use crate::protocol;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};

// ══════════════════════════════════════════════
// 接收侧：抖动缓冲 + 解码线程
// ══════════════════════════════════════════════

/// 音频初始化状态。
/// 历史实现用裸 u32 + 魔数 0/1/2/3 表达四态，读写点分散在三个函数里，
/// 新增状态时极易漏改。改为枚举后编译器可覆盖穷举检查。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum AudioInitState {
    /// 初始化中（含冷启动等待）
    Pending = 0,
    /// Opus 解码器创建失败
    DecoderFailed = 1,
    /// WASAPI 输出设备打开失败
    DeviceFailed = 2,
    /// 就绪
    Ready = 3,
}

impl AudioInitState {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::DecoderFailed,
            2 => Self::DeviceFailed,
            3 => Self::Ready,
            _ => Self::Pending,
        }
    }
}

/// 启动（或重启）音频解码/播放线程。
///
/// 播放节拍完全由声卡播放头决定：
///   * 解码线程只在 `AudioWriter` 有空位时拉取帧，绝不无限预解码；
///   * 是否需要 PLC 由 `JitterBuffer::pull` 依据 `playhead_ts` 判定，
///     不存在"超过 N 毫秒没收到包就补"的计时器；
///   * 等待用 `park`/`unpark`（网络线程与音频回调共同唤醒），
///     超时值只是兜底唤醒，不参与播放决策。
pub(crate) fn init_audio_worker(session: Arc<Session>) {
    session.set_audio_state(AudioInitState::Pending);
    let stats = session.audio_stats().clone();
    let spawn_session = Arc::clone(&session);

    let spawn_result = std::thread::Builder::new()
        .name("udp2cal-decode".into())
        .spawn(move || {
            // 解码线程的节拍由声卡播放头驱动：被后台线程抢占一次就是一个
            // 10 ms 块的迟写，负载高的机器上表现为周期性 underrun（规范 8.2）。
            // 优先注册 MMCSS "Pro Audio" 获取音频调度配额；失败（音频服务被
            // 裁剪/组策略限制）才回退到 THREAD_PRIORITY_HIGHEST。
            let mmcss = power::MmcssGuard::acquire("Audio");
            if !mmcss.is_active() {
                // SAFETY: GetCurrentThread 返回伪句柄，不转移所有权、无需关闭。
                unsafe {
                    if let Err(e) = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) {
                        logf!("[Audio] 解码线程提权失败（{e}），继续用默认优先级");
                    }
                }
            }
            spawn_session.set_decode_thread(std::thread::current());

            let mut dec = match decoder::OpusDecoder::new() {
                Ok(d) => d,
                Err(e) => {
                    logf!("[Audio] Opus 解码器初始化失败: {e:?}");
                    spawn_session.set_audio_state(AudioInitState::DecoderFailed);
                    return;
                }
            };
            let mut aw = match audio::start_audio(stats.clone()) {
                Ok(a) => a,
                Err(e) => {
                    logf!("[Audio] 初始化失败: {e}");
                    spawn_session.set_audio_state(AudioInitState::DeviceFailed);
                    return;
                }
            };
            aw.set_waker(std::thread::current());
            spawn_session.set_audio_state(AudioInitState::Ready);
            logf!(
                "[Audio] 就绪：输出设备 {} Hz（解码 48 kHz 单声道 / 10 ms），声卡缓冲 {} ms",
                aw.device_rate,
                aw.device_latency_ms()
            );

            // ── ASRC：唯一时钟是声卡，唯一观测量是缓冲水位，唯一手段是比率 ──
            //
            // 这一层不再有任何"设备帧 → 媒体时间戳"的换算：播放决策只看
            // 缓冲里还压着多少媒体，比率只改"一帧媒体换多少设备帧"。
            // 设备采样率作除数（ms 换算、ppm 归一、漂移控制器基准）。
            // `DeviceRateEstimator::with_rate` / `DriftController::new` 与
            // audio.rs 里的同类换算都要求非零；cpal 理论上不会给出 0，
            // 但这里是解码线程顶层，除零 = 线程 panic = 整条音频链死亡，
            // 必须栅位而不是赌。
            let mut device_rate = aw.device_rate.max(1);
            let mut drift = protocol::rtp::DriftController::with_target(
                device_rate,
                spawn_session.water_target_ms(),
            );
            // 前馈只看**声卡设备进度 vs QPC**：网络攒批/重排不进入比率。
            // 发送端媒体时间线 vs Instant 的旧 RateEstimator 已从比率路径移除。
            let mut device_rate_est =
                protocol::rtp::DeviceRateEstimator::with_rate(device_rate);
            // 阶跃吸收（方案 C）：判据与限额与 Android 端同构
            let mut step = protocol::rtp::StepAbsorber::new();
            // 实际生效的目标水位（托盘可运行时修改）
            let mut water_target = spawn_session.water_target_ms();
            // 启动预缓冲：先攒够本方向目标再开播，之后靠 ASRC 慢慢收敛；
            // 不再出现“3 帧就开播、再用 24 s 爬坡”的隐性瞬态。
            spawn_session
                .jitter()
                .set_start_prebuffer_ms(water_target);
            let mut payload = [0u8; protocol::rtp::OPUS_MAX_PAYLOAD];
            let mut trim_payload = [0u8; protocol::rtp::OPUS_MAX_PAYLOAD];
            let silence_frame = [0.0f32; protocol::rtp::FRAME_SAMPLES as usize];
            let mut last_epoch = 0u64;
            let mut idle = false;
            let mut rms_sum = 0.0f64;
            let mut rms_count = 0u64;
            let mut last_stats = Instant::now();
            let mut last_packet_at = Instant::now();
            let mut last_newest_ts = 0u64;
            let mut idle_feedback = false;
            // PLC→Silence 接缝的 10 ms 淡出：记录上一块**成功写出**的末样本。
            // 解码失败补零 / 重同步 / 设备重建时清空，避免从过时电平起坡。
            let mut last_pcm: Option<f32> = None;

            // 最近一次下发的总比率微调（反馈 Δu 与前馈 r̂ 的合成）
            let mut adjust: f64;

            loop {
                // 托盘可能随时改目标：与会话原子值对比，变化时实时重定控制器
                let target = spawn_session.water_target_ms();
                if target != water_target {
                    water_target = target;
                    drift.set_target(target);
                    spawn_session.jitter().set_start_prebuffer_ms(target);
                }
                if !spawn_session.running() {
                    if !idle {
                        idle = true;
                        spawn_session.jitter().reset();
                        drift.reset();
                        device_rate_est.reset();
                        step.reset();
                        dec.reset();
                        aw.flush();
                        last_packet_at = Instant::now();
                        idle_feedback = false;
                        last_pcm = None;
                        AUDIO_LEVEL_DB.store((-60.0f32).to_bits(), Ordering::Relaxed);
                    }
                    std::thread::park_timeout(Duration::from_millis(50));
                    continue;
                }
                idle = false;

                // 输出流失效（设备失效/音频服务重启/VB-Cable 重装）时重建，
                // 否则 played/设备时钟冻结、环永不排空，而 UI 仍显示正常。
                if aw.failed() {
                    logf!("[Audio] 输出流失效（设备失效/音频服务重启），尝试重建…");
                    let mut rebuilt = None;
                    for (attempt, delay_ms) in [0u64, 500, 1500].into_iter().enumerate() {
                        if delay_ms > 0 {
                            std::thread::sleep(Duration::from_millis(delay_ms));
                        }
                        match audio::start_audio(stats.clone()) {
                            Ok(a) => {
                                a.set_waker(std::thread::current());
                                rebuilt = Some(a);
                                break;
                            }
                            Err(e) => {
                                logf!("[Audio] 重建失败（第 {} 次）: {e}", attempt + 1);
                            }
                        }
                    }
                    match rebuilt {
                        Some(a) => {
                            aw = a;
                            // 重建期间解码线程停止 pull，但网络线程仍在持续入包：
                            // 不清空会留下一个“旧播放头 … 最新包”之间永不闭合的空洞，
                            // 恢复后只能靠 ASRC + PLC 慢慢排水。epoch 变化会让下一轮
                            // 顺带重建解码器与全部估计器。
                            spawn_session.jitter().reset();
                            device_rate = aw.device_rate.max(1);
                            // 新流的设备时钟/播放头从 0 重新开始，旧估计器历史必须作废。
                            drift = protocol::rtp::DriftController::with_target(
                                device_rate,
                                spawn_session.water_target_ms(),
                            );
                            device_rate_est =
                                protocol::rtp::DeviceRateEstimator::with_rate(device_rate);
                            step = protocol::rtp::StepAbsorber::new();
                            last_pcm = None;
                            spawn_session.set_audio_state(AudioInitState::Ready);
                            logf!("[Audio] 输出流已重建（{} Hz）", aw.device_rate);
                        }
                        None => {
                            logf!("[Audio] 输出流重建失败；请检查音频设备/服务后重新开始");
                            spawn_session.set_audio_state(AudioInitState::DeviceFailed);
                            return;
                        }
                    }
                }

                // 新会话 / 重同步：epoch 变化 → 重建解码器与全部估计器。
                // 速率估计器必须在这里一并复位：resync 会重建 highest_ts 基准。
                let epoch = spawn_session.jitter().epoch;
                if epoch != last_epoch {
                    last_epoch = epoch;
                    dec.reset();
                    drift.reset();
                    device_rate_est.reset();
                    step.reset();
                    aw.flush();
                    last_pcm = None;
                }

                // ── 水位（唯一观测量）──
                // ring = 已解码、已写进 PCM 环、还没被声卡接手的媒体量
                // pipe = ring + 声卡已接手还没播完的（= 可听点之前压着的总量）
                let ring_frames = aw.filled_frames() as u64;
                let dev_frames = aw.device_latency_frames();
                let pipe_frames = ring_frames + dev_frames;
                let ring_ms = ((ring_frames * 1000) / device_rate as u64) as u32;
                let pipe_ms = ((pipe_frames * 1000) / device_rate as u64) as u32;
                // 把解码缓冲折回媒体样本，才知道"此刻出声的那一帧"是哪个时间戳
                let queued_media = (pipe_frames as f64 * aw.current_ratio()) as u64;

                let (pull, jb_water, newest_ts, arrival, trims, step_hit, playing) = {
                    let mut jb = spawn_session.jitter();
                    // 播放头在 pull / 裁剪之前取值：它们都会推进 next_ts，
                    // 用推进后的值扣 queued_media 会把可听点算新一帧。
                    let play_ts = jb.next_ts();

                    // ── 阶跃吸收（方案 C）：持续超额才裁，且逐帧解码后再丢 PCM ──
                    // 判据：总水位持续 STEP_TRIM_HOLD_MS 超过 target+STEP_TRIM_EXCESS_MS。
                    // 单次 90 ms 攒批不再触发。裁剪必须按播放顺序把每一帧交给
                    // Opus 解码器（PLC 缺口走 conceal），只丢弃解码后的 PCM；
                    // 直接清槽位会让有状态解码器与发送端错位，接缝处爆音。
                    let before_water = jb.occupancy_ms();
                    let before_total = before_water.saturating_add(pipe_ms);
                    let decision = step.decide(before_water, pipe_ms, jb.is_playing(), water_target);
                    let mut hit: Option<(u32, u32, u32, bool)> = None;
                    if decision.frames > 0 {
                        let mut trimmed = 0u32;
                        if !decision.limited {
                            for _ in 0..decision.frames {
                                match jb.pull_trim(&mut trim_payload) {
                                    protocol::rtp::Pull::Packet { len, .. } => {
                                        // 解码只为推进状态；PCM 不写环 = 裁掉
                                        let _ = dec.decode(&trim_payload[..len]);
                                        trimmed += 1;
                                    }
                                    protocol::rtp::Pull::Plc { .. } => {
                                        let _ = dec.conceal();
                                        trimmed += 1;
                                    }
                                    protocol::rtp::Pull::Silence { .. } => {
                                        trimmed += 1;
                                    }
                                    protocol::rtp::Pull::Wait => break,
                                }
                            }
                        }
                        hit = Some((before_total, decision.frames, trimmed, decision.limited));
                    }

                    let r = jb.pull(ring_ms, pipe_ms, &mut payload);
                    let highest = jb.highest_ts();
                    let audible_ts = play_ts.saturating_sub(queued_media);
                    (
                        r,
                        jb.water_ms(),
                        highest,
                        jb.arrival_of(audible_ts),
                        jb.stats.step_trims,
                        hit,
                        jb.is_playing(),
                    )
                };
                let water = jb_water.saturating_add(pipe_ms);
                if newest_ts != last_newest_ts {
                    last_newest_ts = newest_ts;
                    last_packet_at = Instant::now();
                }

                // 阶跃吸收的统计只写原子量，由 UI 线程在 `diag::log_periodic_stats`
                // 里落盘：解码线程每 10 s 同步写日志，在慢盘/轮转时会阻塞超过
                // PCM 环的 40 ms，造成周期性的可闻欠载。水位一律用**总水位**
                // （jb + pipe）口径，与判据一致；否则“跳了 120 ms 只掉 76 ms”
                // 会把人带坑里。限额分支每 60 s 窗口只放行一条。
                if let Some((before, want, skipped, limited)) = step_hit {
                    if limited {
                        if step.take_limited_log() {
                            STEP_BEFORE_MS.store(before, Ordering::Relaxed);
                            STEP_WINDOW_TRIMS.store(step.trims_in_window(), Ordering::Relaxed);
                            STEP_EVENT.store(STEP_EVENT_LIMITED, Ordering::Release);
                        }
                    } else if skipped > 0
                        // 限额事件每个窗口只出现一次，不能被随后的裁剪事件覆盖
                        && STEP_EVENT.load(Ordering::Relaxed) != STEP_EVENT_LIMITED
                    {
                        STEP_BEFORE_MS.store(before, Ordering::Relaxed);
                        STEP_WANT.store(want, Ordering::Relaxed);
                        STEP_SKIPPED.store(skipped, Ordering::Relaxed);
                        STEP_WATER_MS.store(water, Ordering::Relaxed);
                        STEP_TRIMS.store(trims, Ordering::Relaxed);
                        STEP_EVENT.store(STEP_EVENT_TRIMMED, Ordering::Release);
                    }
                }

                // ── 前馈：只管“声卡设备时钟 vs QPC” ──
                // 设备进度与时刻都取自音频回调里的 IAudioClock 采样：设备帧数
                // 与 `timestamp().playback` 对应的 QPC 时刻绑定，而不是解码线程
                // 被唤醒时的 `Instant::now()`。没有采样就不喂：设备重建后新流的
                // 首个采样时刻与后续样本同轴，混入 `Instant::now()` 会让 t0 落在
                // 真实时间轴而样本落在旧轴，`duration_since` 恒 0、桶数永不增长。
                let played = aw.played_frames();
                if let Some((clock_frames, played_at)) = aw.device_clock_sample()
                    && clock_frames > 0
                {
                    device_rate_est.observe(clock_frames, played_at);
                }
                let ff = device_rate_est.feedforward().unwrap_or(0.0);

                // ── 反馈：只收长期残差 ──
                // 计时同样用声卡已消费帧数（断流时媒体样本不前进，用它计时会冻结积分）。
                // 长时间没有新 RTP 时冻结反馈并复位积分：否则 `pull` 推进播放头后
                // water≈0，PI 会一路积到 −0.5%，恢复出声时带明显音高偏移。
                let rtp_idle = last_packet_at.elapsed() > Duration::from_millis(500);
                // 反馈只在**真的在出声**时才评估：预缓冲阶段水位必然低于目标，
                // 照常喂 PI 会把积分一路压到负饱和（`PI_INTEGRAL_MAX`），开播后
                // 只能按 ≈3 ppm/s 慢慢退，稳态水位因此被顶在目标之上 10–30 ms
                // 长达数分钟。现场日志指纹就在这里：开播瞬间 `反馈 -1100ppm`，
                // 之后 -457 → -294 → +21 → +65 缓慢爬升。
                let feedback_on = !rtp_idle && playing;
                let du = if !feedback_on {
                    if !idle_feedback {
                        idle_feedback = true;
                        drift.reset();
                    }
                    0.0
                } else {
                    if idle_feedback {
                        idle_feedback = false;
                        drift.reset();
                    }
                    drift.update(water, played)
                };
                adjust = ff + du;
                // 两支路相合后仍须守比率钳位：前馈和反馈各自钳过了，乘积可能越界。
                // 在这里钳一次，诊断 CSV 里看到的才是真正下发给重采样器的数。
                adjust = adjust.clamp(
                    -protocol::rtp::RESAMPLE_MAX_ADJUST,
                    protocol::rtp::RESAMPLE_MAX_ADJUST,
                );
                aw.set_ratio_adjust(adjust);
                DRIFT_ADJUST_BITS.store((adjust as f32).to_bits(), Ordering::Relaxed);

                // 周期日志所需的快照一律落原子量，由 UI 线程输出（见 diag.rs）。
                DECODE_JB_WATER_MS.store(jb_water, Ordering::Relaxed);
                DECODE_PIPE_MS.store(pipe_ms, Ordering::Relaxed);
                DECODE_WATER_TARGET_MS.store(water_target, Ordering::Relaxed);
                DECODE_DEVICE_MS.store(aw.device_latency_ms(), Ordering::Relaxed);
                DECODE_DU_PPM_BITS.store(du.to_bits(), Ordering::Relaxed);
                DECODE_FF_PPM_BITS.store(ff.to_bits(), Ordering::Relaxed);
                DECODE_SIGMA_PPM_BITS
                    .store(device_rate_est.sigma_ppm().to_bits(), Ordering::Relaxed);
                DECODE_BUCKETS.store(device_rate_est.buckets() as u32, Ordering::Relaxed);

                // ── 总延迟估算 ──
                // `audible_ts` 已经扣掉了“PCM 环 + 声卡排队”对应的媒体量，
                // 因此 now − arrival(audible_ts) **本身**就是听感延迟，不能再加
                // device_latency（旧公式把声卡等待算了两遍）。
                if let Some(at) = arrival {
                    let dev_ms = aw.device_latency_ms();
                    DEVICE_LATENCY_MS.store(dev_ms, Ordering::Relaxed);
                    let age_ms = at.elapsed().as_millis().min(u32::MAX as u128) as u32;
                    LATENCY_MS.store(age_ms, Ordering::Relaxed);
                }

                match pull {
                    protocol::rtp::Pull::Packet { len, .. } => {
                        // 解码失败不能只“等水位到期”：`pull` 已推进 10 ms 时间线，
                        // 必须像丢包一样补一帧 PLC，否则播放环出现 10 ms 空洞。
                        match dec.decode(&payload[..len]) {
                            Some(n) => {
                                let pcm = dec.pcm_mut(n);
                                rms_sum += pcm
                                    .iter()
                                    .map(|&x| (x as f64) * (x as f64))
                                    .sum::<f64>();
                                rms_count += n as u64;
                                last_pcm = pcm.last().copied();
                                aw.write(pcm, protocol::rtp::OPUS_SAMPLE_RATE);
                            }
                            None => match dec.conceal() {
                                Some(n) => {
                                    let pcm = dec.pcm_mut(n);
                                    rms_sum += pcm
                                        .iter()
                                        .map(|&x| (x as f64) * (x as f64))
                                        .sum::<f64>();
                                    rms_count += n as u64;
                                    last_pcm = pcm.last().copied();
                                    aw.write(pcm, protocol::rtp::OPUS_SAMPLE_RATE);
                                }
                                None => {
                                    // 解码与 PLC 都失败：补零不走淡出（没有可接续的电平）
                                    last_pcm = None;
                                    aw.write(&silence_frame, protocol::rtp::OPUS_SAMPLE_RATE);
                                }
                            },
                        }
                    }
                    protocol::rtp::Pull::Plc { .. } => {
                        // 先用 Opus 带内 FEC 重建这一帧：丢帧的 LBRR 冗余在**下一个
                        // 包里**，所以前瞻一个包（不消费，下一轮 `pull` 仍会正常取它）
                        // 来还原；只有下一个包也还没到时才退回 PLC。发送端没开
                        // in-band FEC 时 libopus 会自动退化成 PLC，故不会更差。
                        // 现实中这正好覆盖"包迟到/被 AP 攒批挤掉"的短洞：
                        // 过去每洞必 PLC（30 ms 听起来就是一下音量塌陷）。
                        let fec_n = {
                            let jb = spawn_session.jitter();
                            match jb.peek_current(&mut payload) {
                                Some(len) => dec.decode_fec(&payload[..len]),
                                None => None,
                            }
                        };
                        if fec_n.is_some() {
                            FEC_FRAMES.fetch_add(1, Ordering::Relaxed);
                        }
                        match fec_n.or_else(|| dec.conceal()) {
                            Some(n) => {
                                let pcm = dec.pcm_mut(n);
                                rms_sum +=
                                    pcm.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>();
                                rms_count += n as u64;
                                last_pcm = pcm.last().copied();
                                aw.write(pcm, protocol::rtp::OPUS_SAMPLE_RATE);
                            }
                            None => {
                                // 生成不出 PLC：补零，不参与淡出
                                last_pcm = None;
                                aw.write(&silence_frame, protocol::rtp::OPUS_SAMPLE_RATE);
                            }
                        }
                    }
                    protocol::rtp::Pull::Silence { .. } => {
                        // 中段洞 PLC 到上限 / 尾部洞 hold 超时：`pull` 已推进一帧
                        // 媒体时间，否则恢复时真包会被判迟到。PLC→静音的接缝做
                        // 一帧 10 ms 线性淡出（从上一块最后写出的样本到 0），
                        // 避免硬切出“咔”声；之后保持纯静音。
                        match last_pcm.take() {
                            Some(last) => {
                                let mut fade =
                                    [0.0f32; protocol::rtp::FRAME_SAMPLES as usize];
                                let n = fade.len();
                                for (i, s) in fade.iter_mut().enumerate() {
                                    *s = last * (n - 1 - i) as f32 / (n - 1) as f32;
                                }
                                aw.write(&fade, protocol::rtp::OPUS_SAMPLE_RATE);
                            }
                            None => {
                                aw.write(&silence_frame, protocol::rtp::OPUS_SAMPLE_RATE);
                            }
                        }
                    }
                    protocol::rtp::Pull::Wait => {
                        // 现在不该生产：生产节奏由声卡消费节奏约束
                        std::thread::park_timeout(Duration::from_millis(4));
                    }
                }

                if last_stats.elapsed() > Duration::from_millis(100) {
                    let level_db = if rms_count > 0 {
                        let rms = (rms_sum / rms_count as f64).sqrt() as f32;
                        20.0 * (rms.max(1e-10)).log10()
                    } else {
                        -60.0
                    };
                    AUDIO_LEVEL_DB.store(level_db.to_bits(), Ordering::Relaxed);
                    rms_sum = 0.0;
                    rms_count = 0;
                    last_stats = Instant::now();
                }
            }
        });

    // 线程创建失败（极端堆耗尽 / 线程数上限）必须报错：
    // 原先用 `let _ =` 吞掉，`AudioInitState` 永远停在 Pending，等待超时后
    // 引擎照常启动 → UI 显示“运行中”但永远没有声音，日志一片空白。
    if let Err(e) = spawn_result {
        logf!("[Audio] 解码线程创建失败: {e}");
        session.set_audio_state(AudioInitState::DeviceFailed);
    }
}
