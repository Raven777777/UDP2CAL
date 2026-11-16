//! 接收抖动缓冲：按时间戳存放、拉取与 PLC 决策。

use std::time::Instant;

use super::header::{OPUS_MAX_PAYLOAD, RtpPacket, extend_seq, extend_ts};
use super::{FRAME_MS, FRAME_SAMPLES, samples_to_ms};

/// 重排槽位
pub const MAX_REORDER_PACKETS: usize = 64;

/// 起播的**硬下限**：至少 3 帧连续可解码音频。
///
/// 真正的起播水位是 [`JitterBuffer::set_start_prebuffer_ms`] 设置的目标
/// （默认与 [`WATER_TARGET_MS`](super::WATER_TARGET_MS) 相同）：先攒到目标再播，
/// 避免"30 ms 起播、再用 ±0.5% 的 ASRC 花 24 s 爬到 150 ms"这种隐性启动策略。
pub const JITTER_START_MS: u32 = 30;

/// 解码缓冲（PCM 环 + 声卡）的深度上限：达到它就不再预解码。
///
/// 这个阈值守的是**提交期限**：解码缓冲是"已经解码、还没提交给声卡"的媒体量。
/// 达到它就不再预解码，让生产节奏跟随声卡消费；它与 PLC 判据同处一个口径。
pub const DECODE_LIMIT_MS: u32 = 40;
/// 提交提前量：PCM 环只剩这么多时，下一帧必须立刻产出（缺口已经到期）。
///
/// **设备缓冲不算提前量**：声卡里排队的音频已经在设备时钟上，不会给应用
/// 额外的解码时间。旧判据 `ring + device <= 20 ms` 在 30 ms 设备延迟下永远
/// 不成立，回调只能输出静音。
pub const PLC_LEAD_MS: u32 = 20;
/// 连续 PLC 上限（12 帧）
pub const PLC_MAX_MS: u32 = 120;
/// 触发重同步的时间戳跳变阈值
pub const RESYNC_GAP_MS: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertResult {
    /// 首包建立基准，进入预缓冲
    Started,
    /// 正常入槽
    Inserted,
    /// 迟到 / 重复 / 时间戳非法
    Dropped,
    /// 触发重同步（时间戳合法而序号落后，或 M=1 且跳变 > 200 ms）
    Resync,
}

/// 播放线程拉取结果。`ts` 是该帧在 RTP 时间线上的扩展时间戳。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull {
    /// 取到真实包，负载已拷入调用方缓冲
    Packet { len: usize, ts: u64 },
    /// 缺口已到播放头：请求一帧 PLC
    Plc { ts: u64 },
    /// 连续 PLC 已到上限：输出静音
    Silence { ts: u64 },
    /// 水位不足或还没到缺口：调用方继续等（不得用定时器"补"）
    Wait,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JitterStats {
    pub received: u64,
    pub bytes: u64,
    pub inserted: u64,
    pub late: u64,
    pub duplicate: u64,
    pub overflow: u64,
    pub resyncs: u64,
    /// 其中因"时间戳合法而序号落后"触发的重新对齐次数（断流/DTX 自愈计数）
    pub seq_realign: u64,
    pub plc_frames: u64,
    pub max_plc_run: u32,
    /// 因阶跃吸收被消费的整帧总数（逐帧解码后丢弃 PCM，见 [`JitterBuffer::pull_trim`]）
    pub step_trims: u64,
}

#[derive(Clone, Copy)]
struct PacketSlot {
    valid: bool,
    seq: u64,
    ts: u64,
    len: u16,
}

const EMPTY_SLOT: PacketSlot = PacketSlot {
    valid: false,
    seq: 0,
    ts: 0,
    len: 0,
};

/// 到达时刻追踪深度：128 × 10 ms ≈ 1.28 s，远大于任何可能的总水位。
pub const ARRIVAL_TRACE: usize = 128;

/// 单条到达记录：RTP 时间戳 + 该包进入接收端时的本地时刻。
///
/// 用途只有一个：估算"正在播放的样本有多旧"。`now - arrival_of(playhead_ts)`
/// 就是这一段音频从到达网卡到被声卡播出的真实时长，再加上声卡自身的
/// 缓冲延迟即为端到端延迟估算。这是接收端**唯一**能在本地测到的延迟口径——
/// 发送端到接收端的单程网络时延无法在没有跨设备时钟的情况下测出。
#[derive(Clone, Copy)]
struct Arrival {
    ts: u64,
    at: Option<Instant>,
}

const EMPTY_ARRIVAL: Arrival = Arrival { ts: 0, at: None };

/// 单方向的接收抖动缓冲。
///
/// 线程模型：插入由网络线程完成，拉取由播放/解码线程完成，二者共享同一把锁；
/// **插入路径绝不调用 Opus 解码**，播放路径绝不做网络 IO。
pub struct JitterBuffer {
    slots: [PacketSlot; MAX_REORDER_PACKETS],
    /// 与 `slots` 一一对应的负载区（`MAX_REORDER_PACKETS * OPUS_MAX_PAYLOAD`）
    data: Box<[u8]>,
    /// 最近一次**收到**的数据包序号 + 1。
    ///
    /// 它不是“播放头对应的包号”：PLC/静音只推进媒体时间戳，不能凭空发明
    /// 发送端从未发出的序号（RFC 3550：序号每发一个 RTP 包才 +1）。
    next_seq: u64,
    next_ts: u64,
    highest_seq: u64,
    highest_ts: u64,
    /// 当前时间线的起始序号（首包或最近一次重同步），与 `base_inserted` 配套
    /// 用来算丢包估计。
    base_seq: u64,
    /// 建立 `base_seq` 那一刻的 `stats.inserted` 快照。
    base_inserted: u64,
    started: bool,
    playing: bool,
    plc_run: u32,
    /// 每次重同步/复位自增，播放线程据此重建解码器状态
    pub epoch: u64,
    /// 最近一次重同步的原因（诊断用）。`reset()` 清空、首包建立基准时清空，
    /// 其余重同步路径写入人手可读的原因——上层日志直接打印它。
    pub last_rebase_reason: String,
    pub stats: JitterStats,
    /// 到达时刻环形记录（见 [`Arrival`]）
    arrivals: Box<[Arrival]>,
    arrival_pos: usize,
    /// 起播需要攒到的媒体水位（通常等于本方向的目标水位）
    start_ms: u32,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            slots: [EMPTY_SLOT; MAX_REORDER_PACKETS],
            data: vec![0u8; MAX_REORDER_PACKETS * OPUS_MAX_PAYLOAD].into_boxed_slice(),
            next_seq: 0,
            next_ts: 0,
            highest_seq: 0,
            highest_ts: 0,
            base_seq: 0,
            base_inserted: 0,
            started: false,
            playing: false,
            plc_run: 0,
            epoch: 0,
            last_rebase_reason: String::new(),
            stats: JitterStats::default(),
            arrivals: vec![EMPTY_ARRIVAL; ARRIVAL_TRACE].into_boxed_slice(),
            arrival_pos: 0,
            // 硬下限只保证“有得播”；上层必须用 set_start_prebuffer_ms()
            // 注入本方向目标（Windows/Android 都在创建后立刻设置）。
            start_ms: JITTER_START_MS,
        }
    }

    /// 设置起播预缓冲目标（必须由上层按本方向的 `water_target_ms()` 注入）。
    ///
    /// 显式启动策略：连续帧达到 [`JITTER_START_MS`] 的硬下限后，还要攒到
    /// `start_ms` 才进入 PLAYING。这样不会出现“30 ms 起播，再用 0.5% 的
    /// 比率花 24 s 爬到 150 ms”的隐性过程。
    pub fn set_start_prebuffer_ms(&mut self, ms: u32) {
        self.start_ms = ms.max(JITTER_START_MS);
    }

    // ── 状态查询（供播放线程与统计使用）──

    pub fn is_started(&self) -> bool {
        self.started
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn next_ts(&self) -> u64 {
        self.next_ts
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// 期望包数（自上次重同步/复位起）：发送端序号跨度。
    ///
    /// 发送端的序号每发一个 RTP 包才 +1（RFC 3550；DTX / 发送失败不占号），
    /// 所以 `highest_seq − base_seq + 1` 就是这段时间里发送端真正发过的包数。
    pub fn expected_packets(&self) -> u64 {
        if !self.started {
            return 0;
        }
        self.highest_seq.saturating_sub(self.base_seq) + 1
    }

    /// 估计丢包数（自上次重同步/复位起）：`期望包数 − 成功入槽包数`。
    ///
    /// 差额包含**没到达**的包，也包含到达了但没能入槽的包（迟到 `stats.late`、
    /// 超窗 `stats.overflow`、重复 `stats.duplicate`）——要区分“网络真丢”与
    /// “本地丢弃”就看那几个计数。重同步会重置基准，所以它描述的是**当前
    /// 时间线**，不是进程生命周期。
    ///
    /// 分母用 [`Self::expected_packets`]；两者都为 0 时不要相除。
    pub fn lost_estimate(&self) -> u64 {
        self.expected_packets()
            .saturating_sub(self.stats.inserted.saturating_sub(self.base_inserted))
    }

    /// 已见过的最大媒体时间戳（发送端媒体时间线的最新位置）。
    /// 速率估计器拿它与 `arrival_of(highest_ts())` 组成观测对。
    pub fn highest_ts(&self) -> u64 {
        self.highest_ts
    }

    pub fn plc_run(&self) -> u32 {
        self.plc_run
    }

    /// 从 `next_ts` 起连续可解码的帧数（水位）
    pub fn contiguous_frames(&self) -> usize {
        let mut n = 0usize;
        let mut ts = self.next_ts;
        while n < MAX_REORDER_PACKETS && self.find_ts(ts).is_some() {
            n += 1;
            ts += FRAME_SAMPLES as u64;
        }
        n
    }

    /// 媒体跨度（毫秒）：`highest_ts − next_ts`，**包含空洞/丢包/DTX 的时间**。
    ///
    /// 它不是“实际压着多少可解码音频”：中间缺口也计入跨度，上限 64 帧。
    /// 需要真实占用看 [`Self::occupancy_frames`]，需要连续可解码看
    /// [`Self::contiguous_frames`]。控制器使用跨度是因为它在队首乱序时不塌陷；
    /// 但任何“是否真的积压”的判断都必须结合占用/连续量，不能只看跨度。
    pub fn water_ms(&self) -> u32 {
        if self.highest_ts < self.next_ts {
            return 0;
        }
        let frames = ((self.highest_ts - self.next_ts) / FRAME_SAMPLES as u64 + 1)
            .min(MAX_REORDER_PACKETS as u64);
        frames as u32 * FRAME_MS
    }

    /// 当前实际占用的槽位数（每槽 10 ms 媒体），不含空洞。
    pub fn occupancy_frames(&self) -> usize {
        self.slot_count()
    }

    /// 当前实际占用的媒体时长（毫秒），不含空洞。
    pub fn occupancy_ms(&self) -> u32 {
        self.occupancy_frames() as u32 * FRAME_MS
    }

    /// 从 `next_ts` 起连续可解码的媒体时长（毫秒）。
    pub fn contiguous_ms(&self) -> u32 {
        self.contiguous_frames() as u32 * FRAME_MS
    }

    // ── 插入（网络线程）──

    pub fn insert(&mut self, pkt: &RtpPacket<'_>) -> InsertResult {
        self.stats.received += 1;
        self.stats.bytes += pkt.payload.len() as u64;
        if pkt.payload.is_empty() || pkt.payload.len() > OPUS_MAX_PAYLOAD {
            return InsertResult::Dropped;
        }

        if !self.started {
            // 首包只建立基准：**不推进 epoch**。epoch 表示"需要重建解码器/播放头"，
            // 而重置时（reset）已经推过一次；首包再推一次会让上层白重建一次解码器。
            // 首包只建基准：原因清空（reset 已经推过 epoch，这里不再推）
            self.last_rebase_reason.clear();
            self.rebase_with(pkt.seq as u64, pkt.timestamp as u64, false);
            self.store(pkt, pkt.seq as u64, pkt.timestamp as u64);
            self.next_seq = pkt.seq as u64 + 1;
            self.stats.inserted += 1;
            return InsertResult::Started;
        }

        let ext_seq = extend_seq(self.highest_seq, pkt.seq);
        let ext_ts = extend_ts(self.highest_ts, pkt.timestamp);

        // 迟到：播放头已越过该时间戳（时间戳是播放顺序的唯一权威）
        if ext_ts < self.next_ts {
            self.stats.late += 1;
            return InsertResult::Dropped;
        }
        // 重复：同一时间戳已经在槽里
        if self.find_ts(ext_ts).is_some() {
            self.stats.duplicate += 1;
            return InsertResult::Dropped;
        }

        // ── 序号滞后不再是断点 ──
        //
        // 槽位按时间戳查找（`store` 只找空槽，`pull` 用 `find_ts`），序号只用于
        // 诊断与“同序号重复”的粗略过滤。因此 `ext_seq < next_seq` 但时间戳
        // 合法且未重复的包就是**真实的新音频**：直接接纳，绝不因序号滞后而
        // 清空整条缓冲。DTX 期间发送端只推进时间戳不推进序号，恢复包必然
        // 落在这条路径上。
        let gap_ms = samples_to_ms(ext_ts.saturating_sub(self.next_ts));
        if ext_seq < self.next_seq {
            self.stats.seq_realign += 1;
        }

        // M=1 且时间戳跳变超过阈值 → 重同步；跳变很小只更新标记状态
        if pkt.marker && gap_ms > RESYNC_GAP_MS as u64 {
            self.last_rebase_reason = format!("M=1 且时间戳跳变 {}ms", gap_ms);
            self.rebase(ext_seq, ext_ts);
            self.store(pkt, ext_seq, ext_ts);
            self.next_seq = ext_seq + 1;
            self.stats.resyncs += 1;
            self.stats.inserted += 1;
            return InsertResult::Resync;
        }

        // 超出重排窗口（`next_seq .. next_seq+63`）：**只丢弃这一包**。
        //
        // 这里绝不能重同步：重同步会连同 64 帧已缓冲的音频一起丢掉（约 640 ms
        // 空洞，听感就是"卡塔"）；而丢掉一包，等播放头走到该时间戳时只会产生
        // 一次 10 ms PLC，几乎不可闻。
        //
        // 也正因为窗口是满射的（窗口内最多 64 个不同序号，重复包已在上方被拒），
        // 稳态下槽位不会被用尽，`slot_count` 分支只是兜底。
        if ext_seq >= self.next_seq + MAX_REORDER_PACKETS as u64 {
            self.stats.overflow += 1;
            return InsertResult::Dropped;
        }

        if self.slot_count() >= MAX_REORDER_PACKETS {
            self.drop_oldest_non_next();
            self.stats.overflow += 1;
        }

        self.store(pkt, ext_seq, ext_ts);
        if ext_ts > self.highest_ts {
            self.highest_ts = ext_ts;
        }
        if ext_seq > self.highest_seq {
            self.highest_seq = ext_seq;
        }
        // `next_seq` 只跟随**实际收到的包**，PLC/静音不推进它。
        self.next_seq = self.next_seq.max(ext_seq + 1);
        self.stats.inserted += 1;
        InsertResult::Inserted
    }

    // ── 拉取（播放线程）──

    /// 拉取下一帧（播放线程）。
    ///
    /// * `ring_ms`：已解码、已写进 PCM 环、但还没被声卡接手的媒体量。
    ///   这是**应用还能提前多久提交**的唯一可支配量；达到
    ///   [`DECODE_LIMIT_MS`] 时暂停生产，见底时立即补 PLC。
    /// * `pipe_ms`：`ring_ms + device_latency`。仅为兼容 JNI 保留，**不再参与
    ///   播放/PLC 判定**；设备里排队的音频不会给解码器额外的时间。
    ///
    /// 返回 `Wait` 表示“现在不该生产”，调用方必须继续等待，而不得用定时器补帧。
    pub fn pull(&mut self, ring_ms: u32, _pipe_ms: u32, out: &mut [u8]) -> Pull {
        if !self.started {
            return Pull::Wait;
        }
        if !self.playing {
            let enough_frames = self.contiguous_frames() >= (JITTER_START_MS / FRAME_MS) as usize;
            let enough_water = self.water_ms() >= self.start_ms;
            if enough_frames && enough_water {
                self.playing = true;
            } else {
                return Pull::Wait;
            }
        }

        if let Some(idx) = self.find_ts(self.next_ts) {
            // 解码缓冲已够深：生产节奏必须由声卡消费节奏约束，
            // 否则断流时解码线程会以 CPU 速度填满整个环。
            if ring_ms >= DECODE_LIMIT_MS {
                return Pull::Wait;
            }
            let len = self.slots[idx].len as usize;
            if out.len() < len {
                return Pull::Wait;
            }
            let base = idx * OPUS_MAX_PAYLOAD;
            out[..len].copy_from_slice(&self.data[base..base + len]);
            let played_seq = self.slots[idx].seq;
            self.slots[idx].valid = false;
            let ts = self.next_ts;
            self.advance();
            self.next_seq = self.next_seq.max(played_seq + 1);
            self.plc_run = 0;
            return Pull::Packet { len, ts };
        }

        // PCM 提交缓冲已经见底：下一帧现在就要，否则声卡回调只能输出静音。
        // 设备延迟（pipe_ms 里的 device 部分）不能作为“再等等”的理由。
        if ring_ms <= PLC_LEAD_MS {
            let ts = self.next_ts;
            let max_run = PLC_MAX_MS / FRAME_MS;
            self.advance();
            if self.plc_run < max_run {
                self.plc_run += 1;
                self.stats.plc_frames += 1;
                if self.plc_run > self.stats.max_plc_run {
                    self.stats.max_plc_run = self.plc_run;
                }
                Pull::Plc { ts }
            } else {
                Pull::Silence { ts }
            }
        } else {
            Pull::Wait
        }
    }

    /// 前瞻当前播放位（`next_ts`）的包，**不消费**它。
    ///
    /// 用途：Opus 的带内 FEC（RFC 6716 §4.4 的 LBRR 冗余）。某一帧丢失时，
    /// 重建它所需的冗余数据藏在**下一个包**里，所以接收端必须先看到下一个包
    /// 才能决定怎么填这个洞。这里只读不写：不改 `next_ts` / `next_seq`，
    /// 也不让槽位失效——随后 [`pull`](Self::pull) 仍会正常取出同一个包。
    ///
    /// 返回 `None` 表示该帧的包还没到（或 `out` 放不下）。
    pub fn peek_current(&self, out: &mut [u8]) -> Option<usize> {
        let idx = self.find_ts(self.next_ts)?;
        let len = self.slots[idx].len as usize;
        if out.len() < len {
            return None;
        }
        let base = idx * OPUS_MAX_PAYLOAD;
        out[..len].copy_from_slice(&self.data[base..base + len]);
        Some(len)
    }

    /// 阶跃裁剪用：按播放顺序取出下一帧，但**由调用方解码后丢弃 PCM**。
    ///
    /// 这是 [`JitterBuffer::pull`] 的无节流版本：不看 `ring_ms`，也不要求
    /// 已经进入 PLAYING（调用方已判断可裁剪）。返回 `Packet` 时负载已复制到
    /// `out`；返回 `Plc`/`Silence` 时调用方必须让解码器走一次 conceal/silence，
    /// 使解码器状态与后续包连续。绝不能像旧 `skip_frames()` 那样只清槽位——
    /// Opus 是有状态编码，跳过的包不解码会让后续解码器状态与发送端错位，
    /// 接缝处出现爆音/失真。
    ///
    /// 计数记在 `stats.step_trims`，便于按每分钟限额观察。
    pub fn pull_trim(&mut self, out: &mut [u8]) -> Pull {
        if !self.started || !self.playing {
            return Pull::Wait;
        }
        // 裁剪绝不能吃掉已到达的最后一帧：留在缓冲里给正常播放。
        if self.next_ts >= self.highest_ts {
            return Pull::Wait;
        }
        if let Some(idx) = self.find_ts(self.next_ts) {
            let len = self.slots[idx].len as usize;
            if out.len() < len {
                return Pull::Wait;
            }
            let base = idx * OPUS_MAX_PAYLOAD;
            out[..len].copy_from_slice(&self.data[base..base + len]);
            let played_seq = self.slots[idx].seq;
            self.slots[idx].valid = false;
            let ts = self.next_ts;
            self.advance();
            self.next_seq = self.next_seq.max(played_seq + 1);
            self.plc_run = 0;
            self.stats.step_trims += 1;
            return Pull::Packet { len, ts };
        }
        let ts = self.next_ts;
        let max_run = PLC_MAX_MS / FRAME_MS;
        self.advance();
        self.stats.step_trims += 1;
        if self.plc_run < max_run {
            self.plc_run += 1;
            self.stats.plc_frames += 1;
            if self.plc_run > self.stats.max_plc_run {
                self.stats.max_plc_run = self.plc_run;
            }
            Pull::Plc { ts }
        } else {
            Pull::Silence { ts }
        }
    }

    // 漂移校正不再需要任何媒体时间偏移：播放决策只看缓冲水位（`pipe_ms`），
    // 比率只改"一帧媒体换多少设备帧"。两者都不需要记住历史，
    // 因此不存在"播放头与音频背道而驰"这种失败模式。

    /// 复位（重连 / SSRC 变化 / 手动停止）
    pub fn reset(&mut self) {
        self.clear_slots();
        self.next_seq = 0;
        self.next_ts = 0;
        self.highest_seq = 0;
        self.highest_ts = 0;
        self.base_seq = 0;
        self.base_inserted = 0;
        self.started = false;
        self.playing = false;
        self.plc_run = 0;
        // 注意：**不重置** `start_ms`。它是上层按本方向目标注入的配置，
        // 重连后仍应生效。
        // 必须清空：否则新会话会打印上一次会话的重同步原因（诊断误导）
        self.last_rebase_reason.clear();
        self.epoch += 1;
    }

    // ── 内部工具 ──

    /// 以给定 seq/ts 作为新基准，清空缓冲并回到预缓冲
    fn rebase(&mut self, seq: u64, ts: u64) {
        self.rebase_with(seq, ts, true);
    }

    fn rebase_with(&mut self, seq: u64, ts: u64, bump_epoch: bool) {
        self.clear_slots();
        self.next_seq = seq;
        self.next_ts = ts;
        self.highest_seq = seq;
        self.highest_ts = ts;
        self.base_seq = seq;
        self.base_inserted = self.stats.inserted;
        self.started = true;
        self.playing = false;
        self.plc_run = 0;
        if bump_epoch {
            self.epoch += 1;
        }
    }

    /// 推进媒体时间线。**不推进 `next_seq`**：PLC/静音不是发送端发出的包，
    /// 不能凭空发明序号。
    #[inline]
    fn advance(&mut self) {
        self.next_ts += FRAME_SAMPLES as u64;
    }

    fn find_ts(&self, ts: u64) -> Option<usize> {
        self.slots.iter().position(|s| s.valid && s.ts == ts)
    }

    fn slot_count(&self) -> usize {
        self.slots.iter().filter(|s| s.valid).count()
    }

    fn store(&mut self, pkt: &RtpPacket<'_>, seq: u64, ts: u64) {
        let len = pkt.payload.len().min(OPUS_MAX_PAYLOAD);
        // 优先找空槽；槽满时覆盖序号最小者（调用方已保证只有一个溢出）
        let idx = self.slots.iter().position(|s| !s.valid).unwrap_or_else(|| {
            self.slots
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.seq)
                .map(|(i, _)| i)
                .unwrap_or(0)
        });
        let base = idx * OPUS_MAX_PAYLOAD;
        self.data[base..base + len].copy_from_slice(&pkt.payload[..len]);
        self.slots[idx] = PacketSlot {
            valid: true,
            seq,
            ts,
            len: len as u16,
        };
        // 所有"接纳成功"的路径都汇聚到这里：到达时刻一并落账
        self.note_arrival(ts);
    }

    /// 记录一个包的到达时刻（仅诊断用：总延迟估算）
    fn note_arrival(&mut self, ts: u64) {
        self.arrivals[self.arrival_pos] = Arrival {
            ts,
            at: Some(Instant::now()),
        };
        self.arrival_pos = (self.arrival_pos + 1) % ARRIVAL_TRACE;
    }

    /// 查"时间戳 `ts` 那一帧是什么时候到达的"。
    ///
    /// 取的是最新一条 `entry.ts <= ts` 的记录：帧是按时间戳单调到达的，
    /// 该记录就是这一帧（或它最近的同刻邻居）。查不到（例如刚重同步、
    /// 记录已被覆盖）返回 `None`，调用方应跳过本次延迟估算。
    pub fn arrival_of(&self, ts: u64) -> Option<Instant> {
        for i in 0..ARRIVAL_TRACE {
            let idx = (self.arrival_pos + ARRIVAL_TRACE - 1 - i) % ARRIVAL_TRACE;
            let a = self.arrivals[idx];
            if let Some(at) = a.at {
                if a.ts <= ts {
                    return Some(at);
                }
            }
        }
        None
    }

    /// 丢弃扩展序号最小的非 `next` 包（缓冲满时的准入策略）
    fn drop_oldest_non_next(&mut self) {
        let mut victim: Option<(usize, u64)> = None;
        for (i, s) in self.slots.iter().enumerate() {
            if !s.valid || s.ts == self.next_ts {
                continue;
            }
            if victim.map(|(_, seq)| s.seq < seq).unwrap_or(true) {
                victim = Some((i, s.seq));
            }
        }
        if let Some((i, _)) = victim {
            self.slots[i].valid = false;
        }
    }

    fn clear_slots(&mut self) {
        for s in self.slots.iter_mut() {
            s.valid = false;
        }
        // 到达记录的时间线随重同步一起作废：留着会让延迟估算瞬间跳成"几百秒"
        self.arrivals.fill(EMPTY_ARRIVAL);
        self.arrival_pos = 0;
    }
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{RTP_HEADER_SIZE, parse, write_packet};

    fn pkt(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; RTP_HEADER_SIZE + payload.len()];
        let n = write_packet(&mut buf, marker, seq, ts, 0xDEAD_BEEF, payload).unwrap();
        buf.truncate(n);
        buf
    }

    fn insert(jb: &mut JitterBuffer, seq: u16, ts: u32, marker: bool, tag: u8) -> InsertResult {
        let buf = pkt(seq, ts, marker, &[tag]);
        let p = parse(&buf).unwrap();
        jb.insert(&p)
    }

    fn pull_tags(jb: &mut JitterBuffer, decoded_ms: u32, count: usize) -> Vec<u8> {
        let mut out = [0u8; OPUS_MAX_PAYLOAD];
        let mut tags = Vec::new();
        for _ in 0..count {
            match jb.pull(decoded_ms, decoded_ms, &mut out) {
                Pull::Packet { .. } => tags.push(out[0]),
                Pull::Plc { .. } => tags.push(0xFE),
                Pull::Silence { .. } => tags.push(0xFF),
                Pull::Wait => break,
            }
        }
        tags
    }

    #[test]
    fn jb_in_order_plays_after_prebuffer() {
        let mut jb = JitterBuffer::new();
        // 首包建立基准但不足 4 帧 → Wait
        assert_eq!(insert(&mut jb, 100, 4800, true, 1), InsertResult::Started);
        assert_eq!(jb.pull(0, 0, &mut [0u8; 1275]), Pull::Wait);
        for i in 1..4u16 {
            insert(&mut jb, 100 + i, 4800 + 480 * i as u32, false, i as u8 + 1);
        }
        assert_eq!(jb.water_ms(), 40);
        assert_eq!(pull_tags(&mut jb, 0, 4), vec![1, 2, 3, 4]);
        assert_eq!(jb.water_ms(), 0);
    }

    #[test]
    fn jb_out_of_order_reassembles() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 10);
        insert(&mut jb, 2, 960, false, 12); // 乱序 2 帧
        insert(&mut jb, 3, 1440, false, 13);
        insert(&mut jb, 1, 480, false, 11);
        assert_eq!(pull_tags(&mut jb, 0, 4), vec![10, 11, 12, 13]);
    }

    #[test]
    fn jb_lost_estimate_counts_sequence_holes() {
        let mut jb = JitterBuffer::new();
        // 首包：期望 1、入槽 1 → 不丢
        assert_eq!(insert(&mut jb, 100, 0, true, 1), InsertResult::Started);
        assert_eq!(jb.expected_packets(), 1);
        assert_eq!(jb.lost_estimate(), 0);

        insert(&mut jb, 101, 480, false, 2);
        // 跳过 102：序号跨度 3 但只入槽 2 个 → 丢 1
        insert(&mut jb, 103, 1440, false, 3);
        assert_eq!(jb.expected_packets(), 4);
        assert_eq!(jb.lost_estimate(), 1);
        assert_eq!(jb.stats.inserted, 3);

        // 迟到/重复包不影响期望，也不让估计变小（它们本来就没入槽）
        let _ = jb.pull(0, 0, &mut [0u8; OPUS_MAX_PAYLOAD]);
        assert_eq!(insert(&mut jb, 101, 480, false, 9), InsertResult::Dropped);
        assert_eq!(jb.lost_estimate(), 1);

        // 重同步重置基准：新时间线从重同步包开始算
        insert(&mut jb, 900, 96_000, true, 4);
        assert_eq!(jb.expected_packets(), 1);
        assert_eq!(jb.lost_estimate(), 0);

        // reset 后未起播：两个量都为 0（分母不能拿来相除）
        jb.reset();
        assert_eq!(jb.expected_packets(), 0);
        assert_eq!(jb.lost_estimate(), 0);
    }

    #[test]
    fn jb_duplicate_and_late_are_dropped() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 20);
        insert(&mut jb, 1, 480, false, 21);
        insert(&mut jb, 2, 960, false, 22);
        insert(&mut jb, 3, 1440, false, 23);
        assert_eq!(pull_tags(&mut jb, 0, 2), vec![20, 21]); // next_ts = 960
        // 重复（已在槽里）与迟到（已经播过）
        assert_eq!(insert(&mut jb, 2, 960, false, 99), InsertResult::Dropped);
        assert_eq!(insert(&mut jb, 1, 480, false, 98), InsertResult::Dropped);
        assert_eq!(pull_tags(&mut jb, 0, 2), vec![22, 23]);
    }

    #[test]
    fn jb_gap_triggers_plc_only_at_playhead() {
        let mut jb = JitterBuffer::new();
        // 0,480,960,1440 连续，1920 丢失，2400/2880 正常到达
        insert(&mut jb, 0, 0, true, 1);
        insert(&mut jb, 1, 480, false, 2);
        insert(&mut jb, 2, 960, false, 3);
        insert(&mut jb, 3, 1440, false, 4);
        insert(&mut jb, 5, 2400, false, 6);
        insert(&mut jb, 6, 2880, false, 7);
        assert_eq!(pull_tags(&mut jb, 0, 4), vec![1, 2, 3, 4]); // next_ts = 1920
        // 播放头还停在 0：缺口远未到期 → 等待，不生成 PLC
        assert_eq!(
            jb.pull(PLC_LEAD_MS + 10, PLC_LEAD_MS + 10, &mut [0u8; 1275]),
            Pull::Wait
        );
        // 解码缓冲只剩 20 ms（1920 - 20 ms）→ 缺口到期，PLC
        assert_eq!(
            jb.pull(PLC_LEAD_MS, PLC_LEAD_MS, &mut [0u8; 1275]),
            Pull::Plc { ts: 1920 }
        );
        // 下一帧是真实包
        assert_eq!(
            jb.pull(PLC_LEAD_MS, PLC_LEAD_MS, &mut [0u8; 1275]),
            Pull::Packet { len: 1, ts: 2400 }
        );
    }

    #[test]
    fn peek_current_returns_the_playhead_packet_without_consuming_it() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 4800, true, 1);
        insert(&mut jb, 1, 5280, false, 2);
        insert(&mut jb, 2, 5760, false, 3);
        insert(&mut jb, 3, 6240, false, 4);
        // 播放位先推到第 3 个包（tag 3）
        assert_eq!(pull_tags(&mut jb, 0, 2), vec![1, 2]);

        let water_before = jb.water_ms();
        let mut out = [0u8; OPUS_MAX_PAYLOAD];
        assert_eq!(jb.peek_current(&mut out), Some(1));
        assert_eq!(out[0], 3);
        // 前瞻是纯读：水位不变，重复调用返回同一个包
        assert_eq!(jb.water_ms(), water_before);
        assert_eq!(jb.peek_current(&mut out), Some(1));
        // 之后仍按序播放同一个包 —— 这正是 FEC 需要的行为
        assert_eq!(pull_tags(&mut jb, 0, 2), vec![3, 4]);
        // 播放位越过所有包之后就没得前瞻了
        assert_eq!(jb.peek_current(&mut out), None);
    }

    #[test]
    fn peek_current_is_none_when_the_packet_is_missing_or_output_is_small() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        insert(&mut jb, 1, 480, false, 2);
        insert(&mut jb, 2, 960, false, 3);
        insert(&mut jb, 3, 1440, false, 4);
        assert_eq!(pull_tags(&mut jb, 0, 3), vec![1, 2, 3]); // next_ts = 1440
        // 输出缓冲放不下 → None（不得截断成半包给解码器）
        assert_eq!(jb.peek_current(&mut [0u8; 0]), None);
        let mut out = [0u8; OPUS_MAX_PAYLOAD];
        assert_eq!(jb.peek_current(&mut out), Some(1));
        // 缺口处还没到的包同样是 None：调用方据此退回 PLC
        assert_eq!(pull_tags(&mut jb, 0, 1), vec![4]); // next_ts = 1920（永远不到）
        assert_eq!(jb.peek_current(&mut out), None);
    }

    #[test]
    fn jb_plc_ignores_device_buffer_depth() {
        // 设备缓冲 10/20/30/50 ms 都必须在 PCM 环见底时补 PLC；
        // 旧判据 pipe_ms <= 20 在 30/50 ms 设备缓冲下会先欠载。
        for device_ms in [10u32, 20, 30, 50] {
            let mut jb = JitterBuffer::new();
            insert(&mut jb, 0, 0, true, 1);
            insert(&mut jb, 1, 480, false, 2);
            insert(&mut jb, 2, 960, false, 3);
            insert(&mut jb, 4, 1920, false, 5); // 1440 丢失
            assert_eq!(pull_tags(&mut jb, 0, 3), vec![1, 2, 3]);
            let pipe = PLC_LEAD_MS + device_ms;
            assert_eq!(
                jb.pull(PLC_LEAD_MS, pipe, &mut [0u8; 1275]),
                Pull::Plc { ts: 1440 },
                "device {device_ms}ms must not delay PLC"
            );
            assert_eq!(
                jb.pull(PLC_LEAD_MS, pipe, &mut [0u8; 1275]),
                Pull::Packet { len: 1, ts: 1920 }
            );
        }
    }

    #[test]
    fn jb_plc_run_is_capped() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        for i in 1..4u16 {
            insert(&mut jb, i, 480 * i as u32, false, i as u8);
        }
        // 播完后长时间没有新包，播放头一路推进
        let mut plc = 0;
        let mut silence = 0;
        for _ in 0..30u64 {
            match jb.pull(0, 0, &mut [0u8; 1275]) {
                Pull::Plc { .. } => plc += 1,
                Pull::Silence { .. } => silence += 1,
                _ => {}
            }
        }
        assert_eq!(plc, (PLC_MAX_MS / FRAME_MS) as usize);
        assert!(silence > 0);
        assert!(jb.is_playing());
        assert_eq!(jb.plc_run(), PLC_MAX_MS / FRAME_MS);
    }

    #[test]
    fn jb_marker_with_big_jump_resyncs() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        let epoch = jb.epoch;
        // M=1 且跳变 500 ms → 重同步
        let r = insert(&mut jb, 50, 480 * 50, true, 99);
        assert_eq!(r, InsertResult::Resync);
        assert!(jb.epoch > epoch);
        assert!(!jb.is_playing(), "重同步后必须回到预缓冲");
        assert_eq!(jb.next_ts(), 480 * 50);
        assert_eq!(jb.stats.resyncs, 1);
    }

    #[test]
    fn jb_marker_with_small_jump_keeps_buffer() {
        let mut jb = JitterBuffer::new();
        for i in 0..6u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        pull_tags(&mut jb, 0, 4);
        let epoch = jb.epoch;
        // M=1 但时间戳连续（DTX 结束、标记语义）→ 不重置
        let r = insert(&mut jb, 6, 480 * 6, true, 7);
        assert_eq!(r, InsertResult::Inserted);
        assert_eq!(jb.epoch, epoch);
        assert!(jb.is_playing());
    }

    #[test]
    fn jb_handles_seq_and_ts_wrap() {
        let mut jb = JitterBuffer::new();
        let base_ts = 0xFFFF_FF00u32;
        let base_seq = 0xFFFEu16;
        for i in 0..8u32 {
            let ts = base_ts.wrapping_add(480 * i);
            let seq = base_seq.wrapping_add(i as u16);
            insert(&mut jb, seq, ts, i == 0, i as u8 + 1);
        }
        assert_eq!(jb.next_ts(), base_ts as u64);
        assert_eq!(pull_tags(&mut jb, 0, 8), (1..=8).collect::<Vec<u8>>());
        // 回绕后 next_ts 仍在 64 位时间线上单调递增
        assert_eq!(jb.next_ts(), base_ts as u64 + 480 * 8);
    }

    #[test]
    fn jb_full_window_drops_newest_without_resync() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8);
        }
        // 先播掉两帧；随后把窗口填到 seq 65，next_seq 跟到 66
        pull_tags(&mut jb, 0, 2);
        for i in 4..=(MAX_REORDER_PACKETS as u16 + 1) {
            insert(&mut jb, i, 480 * i as u32, false, i as u8);
        }
        assert_eq!(jb.stats.overflow, 0);
        let epoch = jb.epoch;

        // 远未来 seq 130 已在窗口之外：丢这一包，**不得**重同步
        assert_eq!(
            insert(&mut jb, 130, 480 * 130, false, 130),
            InsertResult::Dropped
        );
        assert_eq!(jb.stats.overflow, 1);
        assert_eq!(jb.epoch, epoch, "缓冲不得被整块清空");
        assert_eq!(jb.stats.resyncs, 0);

        // 已缓冲的连续帧照常播放（没有 640 ms 空洞）
        assert_eq!(pull_tags(&mut jb, 0, 2), vec![2, 3]);
    }

    #[test]
    fn jb_overflow_counter_only_grows_outside_window() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 0);
        // 窗口内乱序（3 先到、1 后到）不应计入溢出
        insert(&mut jb, 3, 1440, false, 3);
        insert(&mut jb, 2, 960, false, 2);
        insert(&mut jb, 1, 480, false, 1);
        assert_eq!(jb.stats.overflow, 0);
        assert_eq!(pull_tags(&mut jb, 0, 4), vec![0, 1, 2, 3]);
    }

    #[test]
    fn jb_reset_clears_state() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        jb.reset();
        assert!(!jb.is_started());
        assert_eq!(jb.pull(0, 0, &mut [0u8; 1275]), Pull::Wait);
        assert!(jb.epoch > 0);
    }

    #[test]
    fn jb_prebuffer_threshold_is_three_frames() {
        // JITTER_START_MS = 30 → 3 帧即可起播（配合受控重采样的低水位稳态）
        let mut jb = JitterBuffer::new();
        for i in 0..2u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        assert_eq!(jb.pull(0, 0, &mut [0u8; 1275]), Pull::Wait, "2 帧不得起播");
        insert(&mut jb, 2, 960, false, 3);
        assert_eq!(pull_tags(&mut jb, 0, 3), vec![1, 2, 3]);
    }

    #[test]
    fn jb_accepts_lagging_seq_without_rebuild() {
        // DTX 期间发送端只推进时间戳、不推进序号；接收端 PLC/静音也**不**推进
        // next_seq。恢复包序号可能落后，但时间戳合法：按时间戳接纳，绝不重建。
        let mut jb = JitterBuffer::new();
        for i in 0..8u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        // 全部播掉（next_seq 停在 8，next_ts = 3840）
        pull_tags(&mut jb, 0, 8);
        // 断流 500 ms：播放头推进 50 帧，但 next_seq 不动
        for _ in 0..50u64 {
            let _ = jb.pull(0, 0, &mut [0u8; 1275]);
        }
        assert_eq!(jb.next_seq(), 8, "PLC/静音不得发明发送端序号");
        assert_eq!(jb.next_ts(), 480 * 58);
        let epoch = jb.epoch;

        // 恢复包序号看起来“倒退”（2 < 8），时间戳未来且连续
        let r = insert(&mut jb, 2, 480 * 58, false, 9);
        assert_eq!(r, InsertResult::Inserted, "序号滞后不得丢弃真实音频");
        assert_eq!(jb.stats.seq_realign, 1);
        assert_eq!(jb.stats.resyncs, 0, "不得整块清空缓冲");
        assert_eq!(jb.epoch, epoch);
        assert_eq!(jb.next_ts(), 480 * 58);
        assert_eq!(pull_tags(&mut jb, 0, 1), vec![9]);
    }

    #[test]
    fn jb_long_gap_across_seq_wrap_still_recovers() {
        // BUG-08 回归：断流跨度超过 32767 个包时，16 位有符号差值会把恢复包
        // 的序号扩成负数；只要时间戳合法就必须能重新对齐。
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        // 播掉一帧，进入 PLAYING
        let _ = jb.pull(0, 0, &mut [0u8; 1275]);
        // 断流 40000 帧（约 6.7 分钟）：接收端靠静音推进
        let gap = 40_000u32;
        for _ in 0..gap as u64 / 8 {
            let _ = jb.pull(0, 0, &mut [0u8; 1275]);
        }
        // 恢复包时间戳正确接续，序号因回绕看起来"倒退"
        let resume_ts = 480 * gap;
        let r = insert(&mut jb, (gap as u16).wrapping_add(1), resume_ts, true, 2);
        assert_eq!(r, InsertResult::Resync);
        assert_eq!(jb.next_ts(), resume_ts as u64);
        assert!(
            jb.stats.late <= 1,
            "不得把恢复包判成迟到: {}",
            jb.stats.late
        );
    }

    #[test]
    fn pull_paces_on_decoded_buffer() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        // 解码缓冲已达上限：帧就在手上也不生产（这就是"预解码节流"）
        assert_eq!(
            jb.pull(DECODE_LIMIT_MS, DECODE_LIMIT_MS, &mut [0u8; 1275]),
            Pull::Wait
        );
        assert_eq!(
            jb.pull(DECODE_LIMIT_MS + 40, DECODE_LIMIT_MS + 40, &mut [0u8; 1275]),
            Pull::Wait
        );
        // 掉到上限以下才生产
        assert_eq!(
            jb.pull(DECODE_LIMIT_MS - 10, DECODE_LIMIT_MS - 10, &mut [0u8; 1275]),
            Pull::Packet { len: 1, ts: 0 }
        );
    }

    #[test]
    fn jb_pull_trim_decodes_every_frame_and_advances() {
        let mut jb = JitterBuffer::new();
        for i in 0..12u16 {
            insert(&mut jb, 100 + i, 4800 + 480 * i as u32, i == 0, i as u8 + 1);
        }
        assert_eq!(jb.water_ms(), 120);
        assert_eq!(pull_tags(&mut jb, 0, 1), vec![1]);
        // 裁剪 4 帧：时间线前移 40 ms，但每帧都按顺序取出交给解码器
        let epoch = jb.epoch;
        let mut buf = [0u8; OPUS_MAX_PAYLOAD];
        let mut tags = Vec::new();
        for _ in 0..4 {
            match jb.pull_trim(&mut buf) {
                Pull::Packet { .. } => tags.push(buf[0]),
                other => panic!("裁剪必须是 Packet，得到 {other:?}"),
            }
        }
        assert_eq!(tags, vec![2, 3, 4, 5]);
        assert_eq!(jb.water_ms(), 120 - 10 - 40);
        assert_eq!(jb.stats.step_trims, 4);
        assert_eq!(jb.stats.plc_frames, 0);
        assert_eq!(jb.epoch, epoch);
        assert_eq!(pull_tags(&mut jb, 0, 1), vec![6]);
    }

    #[test]
    fn jb_pull_trim_never_runs_past_arrived_media() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        let mut buf = [0u8; OPUS_MAX_PAYLOAD];
        assert_eq!(jb.pull(DECODE_LIMIT_MS, 0, &mut buf), Pull::Wait);
        assert!(jb.is_playing());
        assert_eq!(jb.highest_ts(), 480 * 3);
        let mut pulled = 0;
        loop {
            match jb.pull_trim(&mut buf) {
                Pull::Packet { .. } => pulled += 1,
                Pull::Wait => break,
                other => panic!("缺口尚未到达却返回 {other:?}"),
            }
        }
        assert_eq!(pulled, 3, "最多裁到最高时间戳，不能推进虚空");
        assert_eq!(jb.next_ts(), jb.highest_ts());
        assert_eq!(jb.water_ms(), FRAME_MS, "最后那一帧必须留着");
        assert_eq!(jb.stats.step_trims, 3);
        assert_eq!(jb.stats.plc_frames, 0);
    }

    #[test]
    fn jb_pull_trim_uses_plc_for_a_real_gap() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        insert(&mut jb, 1, 480, false, 2);
        insert(&mut jb, 2, 960, false, 3);
        insert(&mut jb, 4, 1920, false, 5);
        insert(&mut jb, 5, 2400, false, 6);
        let mut buf = [0u8; OPUS_MAX_PAYLOAD];
        assert_eq!(jb.pull(DECODE_LIMIT_MS, 0, &mut buf), Pull::Wait);
        assert!(jb.is_playing());
        assert_eq!(jb.pull_trim(&mut buf), Pull::Packet { len: 1, ts: 0 });
        assert_eq!(jb.pull_trim(&mut buf), Pull::Packet { len: 1, ts: 480 });
        assert_eq!(jb.pull_trim(&mut buf), Pull::Packet { len: 1, ts: 960 });
        assert_eq!(jb.pull_trim(&mut buf), Pull::Plc { ts: 1440 });
        assert_eq!(jb.stats.step_trims, 4);
        assert_eq!(jb.stats.plc_frames, 1);
    }

    #[test]
    fn jb_pull_trim_needs_a_playing_buffer() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        insert(&mut jb, 1, 480, false, 2);
        assert!(!jb.is_playing());
        assert_eq!(jb.pull_trim(&mut [0u8; 1275]), Pull::Wait);
        assert_eq!(jb.stats.step_trims, 0);
    }

    /// 重同步原因必须能说清"为什么"：上层日志直接印这一行。
    #[test]
    fn jb_records_rebase_reason_and_clears_it_on_reset() {
        let mut jb = JitterBuffer::new();
        for i in 0..6u16 {
            insert(&mut jb, 100 + i, 4800 + 480 * i as u32, i == 0, i as u8 + 1);
        }
        assert!(jb.last_rebase_reason.is_empty());
        // 六个包都真实收到，next_seq 已跟到 106；播放头推到第 5 帧
        assert_eq!(pull_tags(&mut jb, 0, 4).len(), 4);
        assert_eq!(jb.next_seq(), 106);
        assert_eq!(jb.next_ts(), 6720);
        // M=1 且时间戳跳 500 ms：只有真正的断流才重同步，原因必须可读
        assert_eq!(
            insert(&mut jb, 102, 6720 + 24_000, true, 9),
            InsertResult::Resync
        );
        assert!(
            jb.last_rebase_reason.contains("M=1"),
            "{}",
            jb.last_rebase_reason
        );
        // 序号 102 落后于已收的 105，但时间戳是未来：计一次“按时间戳对齐”，
        // 并且它已经进入重同步分支——两者不矛盾。
        assert_eq!(jb.stats.seq_realign, 1);
        // 新会话不能带着上一次的旧原因
        jb.reset();
        assert!(jb.last_rebase_reason.is_empty());
    }

    // ── 到达时刻追踪（总延迟估算）──

    #[test]
    fn arrival_trace_reports_when_frame_landed() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        insert(&mut jb, 1, 480, false, 2);
        let a0 = jb.arrival_of(0).expect("首帧必须有到达记录");
        let a1 = jb.arrival_of(480).expect("第二帧必须有到达记录");
        assert!(a1 >= a0, "到达时刻必须单调");
        // 查询一个尚未到达的时间戳：回落到最近的更早记录，而不是 None
        assert!(jb.arrival_of(960).is_some());
        // 重同步会清空记录，避免延迟估算跨会话跳变
        jb.reset();
        assert!(jb.arrival_of(480).is_none());
    }
}
