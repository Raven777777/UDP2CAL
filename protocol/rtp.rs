//! RTP 媒体承载（RTP version 2 / PT=111 / 48 kHz / 固定 10 ms）
//!
//! 本模块是 v3 媒体面的唯一真源，包含三部分：
//!
//! 1. **线格式**：[`RtpHeader`] / [`parse`] / [`write_packet`]，标准 12 字节 RTP 固定头，
//!    无 CSRC、无扩展、无填充，负载就是 Opus 包本身；
//! 2. **扩展序号/时间戳**：[`extend_seq`] / [`extend_ts`]，用有符号差值把 16/32 位
//!    回绕扩展成 64 位单调计数；
//! 3. **接收侧时钟模型**：[`JitterBuffer`] 只负责"按水位存放"，[`DriftController`]
//!    只根据可播放水位给出丢/插建议；**播放决策完全由声卡播放头驱动**，
//!    本模块内不存在任何定时器、`sleep` 或墙钟。

// ═══════════════════════ 常量 ═══════════════════════

pub const RTP_VERSION: u8 = 2;
pub const RTP_HEADER_SIZE: usize = 12;
pub const OPUS_PAYLOAD_TYPE: u8 = 111;

pub const OPUS_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_FRAME_MS: u32 = 10;
pub const OPUS_FRAME_SAMPLES: u32 = 480;

/// Opus 单包负载上限（RFC 7587 + 1275 字节实用上限）
pub const OPUS_MAX_PAYLOAD: usize = 1275;
/// 单个 RTP 包上限：12 字节头 + 1275 字节负载
pub const RTP_MAX_PACKET: usize = RTP_HEADER_SIZE + OPUS_MAX_PAYLOAD;
/// 加上 UDP(8) + IPv4(20) 后的上限，必须小于典型以太网 MTU
pub const UDP_IP_MAX_PACKET: usize = RTP_MAX_PACKET + 8 + 20;

/// 重排槽位
pub const MAX_REORDER_PACKETS: usize = 64;

// ── 抖动缓冲参数（规范 7.2）──
pub const FRAME_MS: u32 = OPUS_FRAME_MS;
pub const FRAME_SAMPLES: u32 = OPUS_FRAME_SAMPLES;
/// 预缓冲启动阈值（4 帧连续）
pub const JITTER_START_MS: u32 = 40;
/// 目标水位（6 帧）
pub const JITTER_TARGET_MS: u32 = 60;
/// 高水位（12 帧）
pub const JITTER_HIGH_MS: u32 = 120;
/// 低水位（2 帧）
pub const JITTER_LOW_MS: u32 = 20;
/// 提前生成 PLC 的提前量
pub const PLC_LEAD_MS: u32 = 20;
/// 连续 PLC 上限（12 帧）
pub const PLC_MAX_MS: u32 = 120;
/// 触发重同步的时间戳跳变阈值
pub const RESYNC_GAP_MS: u32 = 200;

// ── 漂移校正参数（规范 7.9）──
/// 单次校正下限 1 ms
pub const DRIFT_MIN_SAMPLES: u32 = FRAME_SAMPLES / 10;
/// 单次校正上限 5 ms
pub const DRIFT_MAX_SAMPLES: u32 = FRAME_SAMPLES / 2;
/// 两次校正之间至少间隔 500 ms 的**音频样本时间**（不是墙钟）
pub const DRIFT_INTERVAL_SAMPLES: u64 = 500 * OPUS_SAMPLE_RATE as u64 / 1000;

#[inline]
pub const fn ms_to_samples(ms: u32) -> u32 {
    ms * FRAME_SAMPLES / FRAME_MS
}

#[inline]
pub const fn samples_to_ms(samples: u64) -> u64 {
    samples * 1000 / OPUS_SAMPLE_RATE as u64
}

// ═══════════════════════ 线格式 ═══════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtpError {
    /// 包长 < 13
    TooShort,
    /// 包长 > 1287
    TooLong,
    /// 版本不是 2
    BadVersion,
    /// P/X/CC 非零（v3 不支持）
    BadHeaderFlags,
    /// PT 不是 111
    BadPayloadType,
    /// SSRC 为 0
    ZeroSsrc,
    /// 负载为空或超过 1275
    BadPayloadSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    /// 标记位：会话首包 / DTX 结束 / 时间戳不连续后的首包
    pub marker: bool,
    pub payload_type: u8,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpHeader {
    pub const fn new(marker: bool, seq: u16, timestamp: u32, ssrc: u32) -> Self {
        Self { marker, payload_type: OPUS_PAYLOAD_TYPE, seq, timestamp, ssrc }
    }

    /// 写入 12 字节固定头
    pub fn encode(&self, buf: &mut [u8; RTP_HEADER_SIZE]) {
        buf[0] = 0x80; // V=2, P=0, X=0, CC=0
        buf[1] = (if self.marker { 0x80 } else { 0 }) | (self.payload_type & 0x7F);
        buf[2..4].copy_from_slice(&self.seq.to_be_bytes());
        buf[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        buf[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpPacket<'a> {
    pub marker: bool,
    pub payload_type: u8,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: &'a [u8],
}

/// 严格解析并校验一个 RTP 包（规范 4.1 的全部拒绝条件）
pub fn parse(buf: &[u8]) -> Result<RtpPacket<'_>, RtpError> {
    if buf.len() < RTP_HEADER_SIZE + 1 {
        return Err(RtpError::TooShort);
    }
    if buf.len() > RTP_MAX_PACKET {
        return Err(RtpError::TooLong);
    }
    if buf[0] != 0x80 {
        if buf[0] >> 6 != RTP_VERSION {
            return Err(RtpError::BadVersion);
        }
        return Err(RtpError::BadHeaderFlags);
    }
    let b1 = buf[1];
    let payload_type = b1 & 0x7F;
    if payload_type != OPUS_PAYLOAD_TYPE {
        return Err(RtpError::BadPayloadType);
    }
    let seq = u16::from_be_bytes([buf[2], buf[3]]);
    let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
    if ssrc == 0 {
        return Err(RtpError::ZeroSsrc);
    }
    let payload = &buf[RTP_HEADER_SIZE..];
    if payload.is_empty() || payload.len() > OPUS_MAX_PAYLOAD {
        return Err(RtpError::BadPayloadSize);
    }
    Ok(RtpPacket {
        marker: b1 & 0x80 != 0,
        payload_type,
        seq,
        timestamp,
        ssrc,
        payload,
    })
}

/// 写入完整 RTP 包（头部 + 负载），返回总字节数。
///
/// 负载为空或超过 1275 字节时返回 `None`：**不得截断**，截断会产生非法 Opus 包。
pub fn write_packet(
    dest: &mut [u8],
    marker: bool,
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    payload: &[u8],
) -> Option<usize> {
    if payload.is_empty() || payload.len() > OPUS_MAX_PAYLOAD {
        return None;
    }
    let total = RTP_HEADER_SIZE + payload.len();
    if dest.len() < total {
        return None;
    }
    RtpHeader::new(marker, seq, timestamp, ssrc).encode((&mut dest[..RTP_HEADER_SIZE]).try_into().ok()?);
    dest[RTP_HEADER_SIZE..total].copy_from_slice(payload);
    Some(total)
}

// ═══════════════════════ 扩展序号 / 时间戳 ═══════════════════════

/// 用 16 位有符号差值把 `seq` 扩展到 `highest` 所在的 64 位时间线。
///
/// 乱序窗口远小于 32768，因此有符号差值不会误判方向：
/// `highest = 65535, seq = 0` → `65536`。
#[inline]
pub fn extend_seq(highest: u64, seq: u16) -> u64 {
    let diff = seq.wrapping_sub(highest as u16) as i16 as i64;
    (highest as i64 + diff).max(0) as u64
}

/// 用 32 位有符号差值把 `ts` 扩展到 `highest` 所在的 64 位时间线。
///
/// `highest = 0xFFFF_FF00, ts = 0x0000_00E0` → `0x1_0000_00E0`。
#[inline]
pub fn extend_ts(highest: u64, ts: u32) -> u64 {
    let diff = ts.wrapping_sub(highest as u32) as i32 as i64;
    (highest as i64 + diff).max(0) as u64
}

// ═══════════════════════ 抖动缓冲 ═══════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertResult {
    /// 首包建立基准，进入预缓冲
    Started,
    /// 正常入槽
    Inserted,
    /// 迟到 / 重复 / 时间戳与序号矛盾
    Dropped,
    /// 触发重同步（M=1 且跳变 > 200 ms，或超出重排窗口）
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
    pub plc_frames: u64,
    pub max_plc_run: u32,
}

#[derive(Clone, Copy)]
struct PacketSlot {
    valid: bool,
    seq: u64,
    ts: u64,
    len: u16,
}

const EMPTY_SLOT: PacketSlot = PacketSlot { valid: false, seq: 0, ts: 0, len: 0 };

/// 单方向的接收抖动缓冲。
///
/// 线程模型：插入由网络线程完成，拉取由播放/解码线程完成，二者共享同一把锁；
/// **插入路径绝不调用 Opus 解码**，播放路径绝不做网络 IO。
pub struct JitterBuffer {
    slots: [PacketSlot; MAX_REORDER_PACKETS],
    /// 与 `slots` 一一对应的负载区（`MAX_REORDER_PACKETS * OPUS_MAX_PAYLOAD`）
    data: Box<[u8]>,
    next_seq: u64,
    next_ts: u64,
    highest_seq: u64,
    highest_ts: u64,
    started: bool,
    playing: bool,
    plc_run: u32,
    /// 漂移校正造成的媒体时间偏移（样本），由播放线程维护
    timestamp_offset: i64,
    /// 每次重同步/复位自增，播放线程据此重建解码器状态
    pub epoch: u64,
    pub stats: JitterStats,
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
            started: false,
            playing: false,
            plc_run: 0,
            timestamp_offset: 0,
            epoch: 0,
            stats: JitterStats::default(),
        }
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

    pub fn plc_run(&self) -> u32 {
        self.plc_run
    }

    pub fn timestamp_offset(&self) -> i64 {
        self.timestamp_offset
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

    /// 当前水位（毫秒）
    pub fn water_ms(&self) -> u32 {
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
            self.rebase_with(pkt.seq as u64, pkt.timestamp as u64, false);
            self.store(pkt, self.next_seq, self.next_ts);
            self.stats.inserted += 1;
            return InsertResult::Started;
        }

        let ext_seq = extend_seq(self.highest_seq, pkt.seq);
        let ext_ts = extend_ts(self.highest_ts, pkt.timestamp);

        // 迟到：播放头已越过该时间戳
        if ext_ts < self.next_ts {
            self.stats.late += 1;
            return InsertResult::Dropped;
        }
        // 重复：同一时间戳已经在槽里
        if self.find_ts(ext_ts).is_some() {
            self.stats.duplicate += 1;
            return InsertResult::Dropped;
        }
        // 序号与时间戳矛盾：时间戳是播放顺序的权威，丢弃异常包
        if ext_seq < self.next_seq {
            self.stats.late += 1;
            return InsertResult::Dropped;
        }

        // M=1 且时间戳跳变超过阈值 → 重同步；跳变很小只更新标记状态
        let gap_ms = samples_to_ms(ext_ts.saturating_sub(self.next_ts));
        if pkt.marker && gap_ms > RESYNC_GAP_MS as u64 {
            self.rebase(ext_seq, ext_ts);
            self.store(pkt, ext_seq, ext_ts);
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
        self.stats.inserted += 1;
        InsertResult::Inserted
    }

    // ── 拉取（播放线程）──

    /// 按播放头拉取下一帧。
    ///
    /// `playhead_ts` 是声卡当前播放位置对应的 48 kHz 扩展时间戳；返回 `Wait`
    /// 表示"还不到缺口或水位不足"，调用方必须继续等待而**不得**用定时器补帧。
    pub fn pull(&mut self, playhead_ts: u64, out: &mut [u8]) -> Pull {
        if !self.started {
            return Pull::Wait;
        }
        if !self.playing {
            if self.contiguous_frames() >= (JITTER_START_MS / FRAME_MS) as usize {
                self.playing = true;
            } else {
                return Pull::Wait;
            }
        }

        if let Some(idx) = self.find_ts(self.next_ts) {
            let len = self.slots[idx].len as usize;
            if out.len() < len {
                return Pull::Wait;
            }
            let base = idx * OPUS_MAX_PAYLOAD;
            out[..len].copy_from_slice(&self.data[base..base + len]);
            self.slots[idx].valid = false;
            let ts = self.next_ts;
            self.advance();
            self.plc_run = 0;
            return Pull::Packet { len, ts };
        }

        // 播放头已经走到缺口（含 PLC 提前量）才生成补偿
        let lead = ms_to_samples(PLC_LEAD_MS) as u64;
        if self.next_ts <= playhead_ts.saturating_add(lead) {
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

    /// 播放线程校正 PCM 长度后回写媒体时间偏移。
    ///
    /// 丢弃 `n` 个样本 → 媒体时间线提前 `n`；插入 `n` 个样本 → 延后 `n`。
    pub fn note_pcm_correction(&mut self, delta_samples: i64) {
        self.timestamp_offset += delta_samples;
    }

    /// 复位（重连 / SSRC 变化 / 手动停止）
    pub fn reset(&mut self) {
        self.clear_slots();
        self.next_seq = 0;
        self.next_ts = 0;
        self.highest_seq = 0;
        self.highest_ts = 0;
        self.started = false;
        self.playing = false;
        self.plc_run = 0;
        self.timestamp_offset = 0;
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
        self.started = true;
        self.playing = false;
        self.plc_run = 0;
        if bump_epoch {
            self.epoch += 1;
        }
    }

    #[inline]
    fn advance(&mut self) {
        self.next_seq += 1;
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
        let idx = self
            .slots
            .iter()
            .position(|s| !s.valid)
            .unwrap_or_else(|| {
                self.slots
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, s)| s.seq)
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            });
        let base = idx * OPUS_MAX_PAYLOAD;
        self.data[base..base + len].copy_from_slice(&pkt.payload[..len]);
        self.slots[idx] = PacketSlot { valid: true, seq, ts, len: len as u16 };
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
    }
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════ 漂移校正 ═══════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftAction {
    None,
    /// 丢弃 `n` 个样本（每声道）
    Drop(u32),
    /// 插入 `n` 个样本（每声道）
    Insert(u32),
}

/// 漂移 / 延迟校正器。
///
/// 两条互相独立的策略，故意用不同判据：
///
/// * **高水位 → 排空延迟**（按绝对值）：启动突发或网络成簇会把缓冲顶高，而
///   "生产速率 == 消费速率"不会让它自己回落，必须主动削减。为不产生可闻接缝，
///   限速执行：>300 ms 每 500 ms 削 5 ms（1%），200–300 ms 每 2 s 削 5 ms（0.25%），
///   **低于 200 ms 一律不动**——避免在 120 ms 边界上反复微剪。
/// * **低水位 → 补时钟漂移**（按趋势）：PCM 提前写进声卡时抖动缓冲水位本就是 0，
///   按绝对值判定会每 500 ms 插 5 ms（永久 1%）。所以只在"水位连续 3 次评估都在
///   下降"时才补，稳定低位视为正常。
pub struct DriftController {
    prev_water: i64,
    rising: u32,
    falling: u32,
    last_eval: u64,
    last_action: u64,
    primed: bool,
    pub corrections: u64,
    pub corrected_samples: u64,
}

/// 连续多少次评估同向才判定为趋势（每次间隔 500 ms 音频时间）
const DRIFT_TREND_ROUNDS: u32 = 3;
/// 单次评估的水位变化小于该值视为持平（抑制抖动噪声）
const DRIFT_TREND_DEADBAND_MS: i64 = 10;
/// 高水位排空的下限：低于该值不再主动削减
const DRIFT_TRIM_FLOOR_MS: u32 = 200;
/// 深度积压时的排空节奏（每 500 ms 削一次）
const DRIFT_TRIM_FAST_INTERVAL: u64 = DRIFT_INTERVAL_SAMPLES;
/// 接近下限时的排空节奏（每 2 s 削一次）
const DRIFT_TRIM_SLOW_INTERVAL: u64 = 4 * DRIFT_INTERVAL_SAMPLES;

impl DriftController {
    pub fn new() -> Self {
        Self {
            prev_water: 0,
            rising: 0,
            falling: 0,
            last_eval: 0,
            last_action: 0,
            primed: false,
            corrections: 0,
            corrected_samples: 0,
        }
    }

    pub fn reset(&mut self) {
        self.prev_water = 0;
        self.rising = 0;
        self.falling = 0;
        self.last_eval = 0;
        self.last_action = 0;
        self.primed = false;
    }

    /// `water_ms` 为**总缓冲深度**（抖动缓冲 + 输出队列），
    /// `media_samples` 为累计输出的音频样本数。
    pub fn update(&mut self, water_ms: u32, media_samples: u64) -> DriftAction {
        if !self.primed {
            self.primed = true;
            self.prev_water = water_ms as i64;
            self.last_eval = media_samples;
            self.last_action = media_samples;
            return DriftAction::None;
        }
        // 每 500 ms 音频时间评估一次
        if media_samples.saturating_sub(self.last_eval) < DRIFT_INTERVAL_SAMPLES {
            return DriftAction::None;
        }
        self.last_eval = media_samples;

        // ── 更新趋势 ──
        let water = water_ms as i64;
        let delta = water - self.prev_water;
        self.prev_water = water;
        if delta > DRIFT_TREND_DEADBAND_MS {
            self.rising += 1;
            self.falling = 0;
        } else if delta < -DRIFT_TREND_DEADBAND_MS {
            self.falling += 1;
            self.rising = 0;
        } else {
            self.rising = 0;
            self.falling = 0;
        }

        // ── 高水位：限速排空（延迟控制，不是漂移检测）──
        if water_ms > DRIFT_TRIM_FLOOR_MS {
            let interval = if water_ms > DRIFT_TRIM_FLOOR_MS * 3 / 2 {
                DRIFT_TRIM_FAST_INTERVAL
            } else {
                DRIFT_TRIM_SLOW_INTERVAL
            };
            if media_samples.saturating_sub(self.last_action) >= interval {
                self.last_action = media_samples;
                self.corrections += 1;
                self.corrected_samples += DRIFT_MAX_SAMPLES as u64;
                return DriftAction::Drop(DRIFT_MAX_SAMPLES);
            }
            return DriftAction::None;
        }

        // ── 低水位：只在持续变薄时补时钟漂移 ──
        if self.falling >= DRIFT_TREND_ROUNDS && water_ms < JITTER_LOW_MS {
            self.falling = 0;
            self.last_action = media_samples;
            self.corrections += 1;
            self.corrected_samples += DRIFT_MAX_SAMPLES as u64;
            return DriftAction::Insert(DRIFT_MAX_SAMPLES);
        }
        DriftAction::None
    }
}

impl Default for DriftController {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════ 播放头时钟 ═══════════════════════

/// 把设备帧数映射到 48 kHz RTP 时间线（规范 7.10）。
///
/// - `base_ts`：第一帧真实 PCM 被写入时的 RTP 时间戳；
/// - `base_device_frames`：该时刻声卡已消费的设备帧数；
/// - `offset`：漂移校正累计偏移（样本）。
///
/// 非 48 kHz 设备用余数累加器换算，避免整数除法造成长期漂移。
pub struct PlayheadClock {
    device_rate: u32,
    base_ts: u64,
    base_device_frames: u64,
    remainder: u64,
    last_device_frames: u64,
    last_value: u64,
    offset: i64,
    started: bool,
}

impl PlayheadClock {
    pub fn new(device_rate: u32) -> Self {
        Self {
            device_rate: device_rate.max(1),
            base_ts: 0,
            base_device_frames: 0,
            remainder: 0,
            last_device_frames: 0,
            last_value: 0,
            offset: 0,
            started: false,
        }
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    /// 以第一帧真实 PCM 的时间戳建立基准
    pub fn start(&mut self, base_ts: u64, played_device_frames: u64) {
        self.base_ts = base_ts;
        self.base_device_frames = played_device_frames;
        self.last_device_frames = played_device_frames;
        self.remainder = 0;
        self.last_value = base_ts;
        self.offset = 0;
        self.started = true;
    }

    /// 用音效卡已消费的设备帧数推进播放头
    pub fn update(&mut self, played_device_frames: u64) -> Option<u64> {
        if !self.started {
            return None;
        }
        if played_device_frames < self.last_device_frames {
            // 设备重置/回绕：保持上次结果，等待调用方重新 start()
            return Some(self.last_value);
        }
        let delta = played_device_frames - self.last_device_frames;
        self.last_device_frames = played_device_frames;
        if delta > 0 {
            let num = delta * OPUS_SAMPLE_RATE as u64 + self.remainder;
            self.last_value = self
                .last_value
                .saturating_add(num / self.device_rate as u64);
            self.remainder = num % self.device_rate as u64;
        }
        Some(self.apply_offset())
    }

    fn apply_offset(&self) -> u64 {
        (self.last_value as i64 + self.offset).max(0) as u64
    }

    /// 播放头当前值（含漂移偏移）
    pub fn value(&self) -> Option<u64> {
        if self.started {
            Some(self.apply_offset())
        } else {
            None
        }
    }

    /// 漂移校正：丢弃 PCM → `delta > 0` 让播放头前移
    pub fn note_pcm_correction(&mut self, delta_samples: i64) {
        self.offset += delta_samples;
    }

    pub fn reset(&mut self) {
        self.started = false;
        self.base_ts = 0;
        self.base_device_frames = 0;
        self.remainder = 0;
        self.last_device_frames = 0;
        self.last_value = 0;
        self.offset = 0;
    }
}

// ═══════════════════════ 测试 ═══════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; RTP_HEADER_SIZE + payload.len()];
        let n = write_packet(&mut buf, marker, seq, ts, 0xDEAD_BEEF, payload).unwrap();
        buf.truncate(n);
        buf
    }

    fn parse_owned(buf: &[u8]) -> RtpPacket<'_> {
        parse(buf).unwrap()
    }

    // ── 线格式 ──

    #[test]
    fn rtp_bytes_match_spec_vector_b1() {
        let buf = pkt(0x1234, 0x4B0, true, &[0xF8, 0xFF, 0xFE]);
        assert_eq!(
            buf,
            [0x80, 0xEF, 0x12, 0x34, 0x00, 0x00, 0x04, 0xB0, 0xDE, 0xAD, 0xBE, 0xEF, 0xF8, 0xFF, 0xFE]
        );
        let p = parse_owned(&buf);
        assert!(p.marker);
        assert_eq!(p.payload_type, OPUS_PAYLOAD_TYPE);
        assert_eq!(p.seq, 0x1234);
        assert_eq!(p.timestamp, 0x4B0);
        assert_eq!(p.ssrc, 0xDEAD_BEEF);
        assert_eq!(p.payload, &[0xF8, 0xFF, 0xFE]);

        // M=0 时首字节仍是 0x80，第二字节去掉 0x80
        let buf = pkt(0x1234, 0x4B0, false, &[0xF8]);
        assert_eq!(buf[1], 0x6F);
    }

    #[test]
    fn rtp_rejects_invalid_packets() {
        // 版本错误
        let mut buf = pkt(1, 0, false, &[1]);
        buf[0] = 0x40;
        assert_eq!(parse(&buf), Err(RtpError::BadVersion));
        // P=1
        let mut buf = pkt(1, 0, false, &[1]);
        buf[0] = 0xA0;
        assert!(matches!(parse(&buf), Err(RtpError::BadHeaderFlags)));
        // X=1
        let mut buf = pkt(1, 0, false, &[1]);
        buf[0] = 0x90;
        assert!(matches!(parse(&buf), Err(RtpError::BadHeaderFlags)));
        // CC=1
        let mut buf = pkt(1, 0, false, &[1]);
        buf[0] = 0x81;
        assert!(matches!(parse(&buf), Err(RtpError::BadHeaderFlags)));
        // PT 错误
        let mut buf = pkt(1, 0, false, &[1]);
        buf[1] = 0x60;
        assert_eq!(parse(&buf), Err(RtpError::BadPayloadType));
        // SSRC = 0
        let mut buf = pkt(1, 0, false, &[1]);
        buf[8..12].fill(0);
        assert_eq!(parse(&buf), Err(RtpError::ZeroSsrc));
        // 空负载
        let buf = [0x80u8, 0x6F, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        assert_eq!(parse(&buf), Err(RtpError::TooShort));
        // 过短
        assert_eq!(parse(&[0x80; 5]), Err(RtpError::TooShort));
        // 超长
        let mut big = vec![0u8; RTP_MAX_PACKET + 1];
        big[0] = 0x80;
        big[1] = 0x6F;
        big[11] = 1;
        assert_eq!(parse(&big), Err(RtpError::TooLong));
    }

    #[test]
    fn rtp_packet_size_limits() {
        // 1275 字节负载 → 1287 字节包
        let payload = vec![0xA5u8; OPUS_MAX_PAYLOAD];
        let mut buf = vec![0u8; RTP_MAX_PACKET];
        assert_eq!(write_packet(&mut buf, false, 1, 0, 7, &payload), Some(RTP_MAX_PACKET));
        assert_eq!(UDP_IP_MAX_PACKET, 1315);
        assert!(write_packet(&mut buf, false, 1, 0, 7, &vec![0u8; 1276]).is_none());
        assert!(write_packet(&mut buf, false, 1, 0, 7, &[]).is_none());
        // 缓冲区不足
        let mut small = vec![0u8; 20];
        assert!(write_packet(&mut small, false, 1, 0, 7, &vec![0u8; 9]).is_none());
    }

    // ── 扩展计数 ──

    #[test]
    fn seq_extends_across_wrap() {
        assert_eq!(extend_seq(65535, 0), 65536);
        assert_eq!(extend_seq(65535, 1), 65537);
        assert_eq!(extend_seq(65535, 65534), 65534);
        assert_eq!(extend_seq(10, 9), 9);
        assert_eq!(extend_seq(0, 5), 5);
        // 迟到包（回退 3）
        assert_eq!(extend_seq(100, 97), 97);
        // 大跨度前跳
        assert_eq!(extend_seq(100, 30000), 30000);
    }

    #[test]
    fn timestamp_extends_across_wrap() {
        assert_eq!(extend_ts(0xFFFF_FF00, 0x0000_00E0), 0x1_0000_00E0);
        assert_eq!(extend_ts(0xFFFF_FFFF, 0x1DF), 0x1_0000_01DF);
        assert_eq!(extend_ts(480, 960), 960);
        assert_eq!(extend_ts(960, 480), 480);
    }

    #[test]
    fn clock_samples_helpers() {
        assert_eq!(ms_to_samples(10), 480);
        assert_eq!(ms_to_samples(20), 960);
        assert_eq!(ms_to_samples(200), 9600);
        assert_eq!(samples_to_ms(9600), 200);
        assert_eq!(samples_to_ms(480), 10);
    }

    // ── 抖动缓冲 ──

    fn insert(jb: &mut JitterBuffer, seq: u16, ts: u32, marker: bool, tag: u8) -> InsertResult {
        let buf = pkt(seq, ts, marker, &[tag]);
        let p = parse(&buf).unwrap();
        jb.insert(&p)
    }

    fn pull_tags(jb: &mut JitterBuffer, playhead: u64, count: usize) -> Vec<u8> {
        let mut out = [0u8; OPUS_MAX_PAYLOAD];
        let mut tags = Vec::new();
        for _ in 0..count {
            match jb.pull(playhead, &mut out) {
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
        assert_eq!(jb.pull(4800, &mut [0u8; 1275]), Pull::Wait);
        for i in 1..4u16 {
            insert(&mut jb, 100 + i, 4800 + 480 * i as u32, false, i as u8 + 1);
        }
        assert_eq!(jb.water_ms(), 40);
        assert_eq!(pull_tags(&mut jb, 4800, 4), vec![1, 2, 3, 4]);
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
        assert_eq!(pull_tags(&mut jb, 960, 2), vec![22, 23]);
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
        assert_eq!(jb.pull(0, &mut [0u8; 1275]), Pull::Wait);
        // 播放头走到 20 ms（1920 - 20 ms 提前量）→ 缺口到期，PLC
        assert_eq!(jb.pull(960, &mut [0u8; 1275]), Pull::Plc { ts: 1920 });
        // 下一帧是真实包
        assert_eq!(jb.pull(960, &mut [0u8; 1275]), Pull::Packet { len: 1, ts: 2400 });
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
        for step in 0..30u64 {
            match jb.pull(ms_to_samples(step as u32 * 10) as u64, &mut [0u8; 1275]) {
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
        assert_eq!(pull_tags(&mut jb, base_ts as u64, 8), (1..=8).collect::<Vec<u8>>());
        // 回绕后 next_ts 仍在 64 位时间线上单调递增
        assert_eq!(jb.next_ts(), base_ts as u64 + 480 * 8);
    }

    #[test]
    fn jb_full_window_drops_newest_without_resync() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8);
        }
        // 先播掉两帧，让 next 落在 seq 2；窗口是 2..=65
        pull_tags(&mut jb, 0, 2);
        for i in 4..=(MAX_REORDER_PACKETS as u16 + 1) {
            insert(&mut jb, i, 480 * i as u32, false, i as u8);
        }
        assert_eq!(jb.stats.overflow, 0);
        let epoch = jb.epoch;

        // seq 66 已在窗口之外：丢这一包，**不得**重同步
        assert_eq!(insert(&mut jb, 66, 480 * 66, false, 66), InsertResult::Dropped);
        assert_eq!(jb.stats.overflow, 1);
        assert_eq!(jb.epoch, epoch, "缓冲不得被整块清空");
        assert_eq!(jb.stats.resyncs, 0);

        // 已缓冲的连续帧照常播放（没有 640 ms 空洞）
        assert_eq!(pull_tags(&mut jb, 480, 2), vec![2, 3]);

        // 播放推进、窗口前移后，同一序号即可正常接纳
        assert_eq!(insert(&mut jb, 66, 480 * 66, false, 66), InsertResult::Inserted);
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
        assert_eq!(jb.pull(0, &mut [0u8; 1275]), Pull::Wait);
        assert!(jb.epoch > 0);
    }

    // ── 漂移 ──

    #[test]
    fn drift_keeps_normal_water_untouched() {
        // 20–120 ms 是正常工作区间：稳定在此不得产生任何编辑
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(60, media);
        for _ in 0..600 {
            media += DRIFT_INTERVAL_SAMPLES;
            assert_eq!(d.update(60, media), DriftAction::None);
        }
        assert_eq!(d.corrections, 0);
    }

    #[test]
    fn drift_does_not_trim_below_floor() {
        // 150 ms 高于规范高水位但低于排空下限：不主动削减，
        // 宁可多 30 ms 延迟，也不在边界上反复微剪
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(150, media);
        for _ in 0..600 {
            media += DRIFT_INTERVAL_SAMPLES;
            assert_eq!(d.update(150, media), DriftAction::None);
        }
        assert_eq!(d.corrections, 0);
    }

    #[test]
    fn drift_drains_excess_water_at_bounded_rate() {
        // 400 ms 积压：每 500 ms 削 5 ms（1%），10 秒正好 20 次
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(400, media);
        let mut drops = 0;
        for _ in 0..20 {
            media += DRIFT_INTERVAL_SAMPLES;
            if d.update(400, media) == DriftAction::Drop(DRIFT_MAX_SAMPLES) {
                drops += 1;
            }
        }
        assert_eq!(drops, 20);
        assert_eq!(d.corrections, 20);
    }

    #[test]
    fn drift_drains_slowly_near_floor() {
        // 220 ms：降到 0.25% 的节奏（每 2 s 一次）
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(220, media);
        let mut drops = 0;
        for _ in 0..20 {
            media += DRIFT_INTERVAL_SAMPLES;
            if d.update(220, media) == DriftAction::Drop(DRIFT_MAX_SAMPLES) {
                drops += 1;
            }
        }
        assert_eq!(drops, 5);
    }

    #[test]
    fn drift_inserts_only_when_falling() {
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(120, media);
        let mut action = DriftAction::None;
        for k in 1..30u64 {
            media += DRIFT_INTERVAL_SAMPLES;
            let water = 120u32.saturating_sub(k as u32 * 20);
            let a = d.update(water, media);
            if a != DriftAction::None {
                action = a;
                break;
            }
        }
        assert_eq!(action, DriftAction::Insert(DRIFT_MAX_SAMPLES));
    }

    #[test]
    fn drift_ignores_stable_low_water() {
        // PCM 已提前写进声卡时水位恒为低位：不得按绝对值补帧
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(10, media);
        for _ in 0..600 {
            media += DRIFT_INTERVAL_SAMPLES;
            assert_eq!(d.update(10, media), DriftAction::None);
        }
        assert_eq!(d.corrections, 0);
    }

    #[test]
    fn timestamp_offset_tracks_correction() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        assert_eq!(jb.timestamp_offset(), 0);
        jb.note_pcm_correction(240); // 丢弃 5 ms
        assert_eq!(jb.timestamp_offset(), 240);
        jb.note_pcm_correction(-480); // 插入 10 ms
        assert_eq!(jb.timestamp_offset(), -240);
    }

    // ── 播放头映射 ──

    #[test]
    fn playhead_48k_maps_one_to_one() {
        let mut clock = PlayheadClock::new(48_000);
        assert_eq!(clock.value(), None);
        clock.start(9600, 1000);
        assert_eq!(clock.value(), Some(9600));
        assert_eq!(clock.update(1480), Some(9600 + 480));
        assert_eq!(clock.update(1960), Some(9600 + 960));
        clock.note_pcm_correction(240);
        assert_eq!(clock.value(), Some(9600 + 960 + 240));
    }

    #[test]
    fn playhead_44k1_uses_remainder_accumulator() {
        let mut clock = PlayheadClock::new(44_100);
        clock.start(0, 0);
        // 44100 设备帧 = 1 秒 = 48000 样本，余数累加不得丢样
        let mut total = 0u64;
        for _ in 0..441 {
            total += 100;
            clock.update(total);
        }
        let ph = clock.value().unwrap();
        assert!((ph as i64 - 48_000).abs() <= 1, "ph={ph}");
        // 再跑 10 秒，累计误差仍然 ≤1 样本
        for _ in 0..4410 {
            total += 100;
            clock.update(total);
        }
        let ph = clock.value().unwrap();
        assert!((ph as i64 - 48_000 * 11).abs() <= 2, "ph={ph}");
    }
}
