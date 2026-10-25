//! RTP 媒体承载（RTP version 2 / PT=111 / 48 kHz / 固定 10 ms）
//!
//! 本模块是 v3 媒体面的唯一真源，包含三部分：
//!
//! 1. **线格式**：[`RtpHeader`] / [`parse`] / [`write_packet`]，标准 12 字节 RTP 固定头，
//!    无 CSRC、无扩展、无填充，负载就是 Opus 包本身；
//! 2. **扩展序号/时间戳**：[`extend_seq`] / [`extend_ts`]，用有符号差值把 16/32 位
//!    回绕扩展成 64 位单调计数；
//! 3. **接收侧时钟模型**：[`JitterBuffer`] 只负责"按水位存放"，[`DriftController`]
//!    用受控重采样比率把可播放水位钉在目标上、并在比率顶满时做有界单次回落；
//!    **播放决策完全由声卡播放头驱动**，本模块内不存在任何定时器、`sleep` 或墙钟。

// ═══════════════════════ 常量 ═══════════════════════

use std::time::Instant;

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
/// 预缓冲启动阈值（3 帧连续）。
///
/// 40 ms 是为"离散丢/插校正"留的安全垫；改用受控重采样后水位由比率闭环
/// 维持在目标附近，启动门槛可以降到 30 ms 而不增加 PLC 概率。
pub const JITTER_START_MS: u32 = 30;
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
//
// 两级手段，按优先级：
//   1. **受控重采样**（主力）：把重采样比在 ±0.5% 内按水位偏差缓慢调整，
//      每 10 ms 更新一次。比率是连续量，永远不会产生离散接缝，
//      0.5% 相当于每秒最多吞吐 5 ms 的缓冲差，足够覆盖任何真实时钟漂移
//      （晶振级漂移典型 10–100 ppm）。
//   2. **有界回落**（安全网）：只有当水位仍持续跑出 ±(30/10) ms 的回落带
//      ——即比率已经顶到上限、单靠重采样收不住——才做一次 1 ms 的丢/插。
//      每秒最多 1 ms，且必须在 ≥1 ms 的交叉淡化区内完成，绝不硬切。

/// 有界回落的单次量：1 ms（48 样本）
pub const SLIP_SAMPLES: u32 = FRAME_SAMPLES / 10;
/// 交叉淡化长度：1 ms。丢/插都在这个窗口内完成，不得硬切。
pub const SLIP_FADE_SAMPLES: usize = SLIP_SAMPLES as usize;
/// 回落高带：水位高于目标 30 ms 以上
pub const SLIP_HIGH_MARGIN_MS: u32 = 30;
/// 回落低带：水位低于目标 10 ms
pub const SLIP_LOW_MARGIN_MS: u32 = 10;
/// 回落最小间隔：1 秒**音频样本时间**（不是墙钟）—— 每秒最多 1 ms
pub const SLIP_MIN_INTERVAL_SAMPLES: u64 = OPUS_SAMPLE_RATE as u64;
/// 回落评估周期：100 ms 音频样本时间
pub const SLIP_EVAL_SAMPLES: u64 = OPUS_SAMPLE_RATE as u64 / 10;
/// 高带"稳定"判据：连续 5 次评估（500 ms）都在高带内才动手
pub const SLIP_STABLE_ROUNDS: u32 = 5;
/// 启动宽限：前 2 秒只做比率微调。此时水位必然从 0 爬向目标，
/// 若允许回落会在启动阶段连发多次插入。
pub const SLIP_WARMUP_SAMPLES: u64 = 2 * OPUS_SAMPLE_RATE as u64;

/// 重采样比率的微调上限（±0.5%）
pub const RESAMPLE_MAX_ADJUST: f64 = 0.005;
/// 比率更新周期：每 10 ms（1 帧）更新一次
pub const RATIO_UPDATE_SAMPLES: u64 = FRAME_SAMPLES as u64;
/// 单次比率更新的最大步进。0.0001/10 ms = 1%/s，
/// 到达 ±0.5% 满幅需要 0.5 s——既是变调不可闻的斜坡，也足够跟上真实漂移。
pub const RATIO_SLEW_STEP: f64 = 0.0001;
/// 比率死区：偏差 ≤5 ms 认为已在目标上，不动作（避免来回抖动）
pub const RATIO_DEADBAND_MS: f64 = 5.0;
/// 比率满幅点：偏差 ≥20 ms 时比率达到 ±0.5%
pub const RATIO_FULL_MS: f64 = 20.0;
/// 水位一阶低通系数（每次更新 10 ms，等效时间常数 ≈ 150 ms）
pub const WATER_SMOOTH_ALPHA: f64 = 0.065;

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
}

#[derive(Clone, Copy)]
struct PacketSlot {
    valid: bool,
    seq: u64,
    ts: u64,
    len: u16,
}

const EMPTY_SLOT: PacketSlot = PacketSlot { valid: false, seq: 0, ts: 0, len: 0 };

/// 到达时刻追踪深度：128 × 10 ms ≈ 1.28 s，远大于任何可能的总水位。
const ARRIVAL_TRACE: usize = 128;

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
    next_seq: u64,
    next_ts: u64,
    highest_seq: u64,
    highest_ts: u64,
    started: bool,
    playing: bool,
    plc_run: u32,
    /// 每次重同步/复位自增，播放线程据此重建解码器状态
    pub epoch: u64,
    pub stats: JitterStats,
    /// 到达时刻环形记录（见 [`Arrival`]）
    arrivals: Box<[Arrival]>,
    arrival_pos: usize,
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
            epoch: 0,
            stats: JitterStats::default(),
            arrivals: vec![EMPTY_ARRIVAL; ARRIVAL_TRACE].into_boxed_slice(),
            arrival_pos: 0,
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

        // ── 断点判定（必须先于"序号迟到"判定）──
        //
        // 历史实现把 `ext_seq < next_seq` 的丢弃判定放在重同步之前，导致两类
        // 断流永远无法自愈（时间戳合法且在未来、序号却已落后）：
        //   1. 发送端在 ACK 中断 / DTX 期间只推进时间戳、不推进序号；
        //   2. 断流超过 32767 个包（约 5.4 分钟）后 16 位有符号差值回绕。
        // 由于接收端断流期间会用 PLC/静音推进 next_seq，滞后量永不闭合。
        // 现在：时间戳合法且未重复 → 说明是真实的新音频，按时间戳重新对齐；
        // 真正的迟到包在上面的 `ext_ts < next_ts` 已被拦下。
        let gap_ms = samples_to_ms(ext_ts.saturating_sub(self.next_ts));
        if ext_seq < self.next_seq {
            self.rebase(ext_seq, ext_ts);
            self.store(pkt, ext_seq, ext_ts);
            self.stats.resyncs += 1;
            self.stats.seq_realign += 1;
            self.stats.inserted += 1;
            return InsertResult::Resync;
        }

        // M=1 且时间戳跳变超过阈值 → 重同步；跳变很小只更新标记状态
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

    // 漂移校正造成的媒体时间偏移**只由 `PlayheadClock` 承担**：`pull` 的缺口
    // 判定用的是播放头（`next_ts <= playhead_ts + lead`），把偏移加在播放头上
    // 即可让整条媒体时间线随之平移。抖动缓冲不再保存偏移副本——历史上那份
    // 副本只写不读，纯属误导（规范 7.9 的"同步调整"由播放头完成）。

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
        // 所有"接纳成功"的路径都汇聚到这里：到达时刻一并落账
        self.note_arrival(ts);
    }

    /// 记录一个包的到达时刻（仅诊断用：总延迟估算）
    fn note_arrival(&mut self, ts: u64) {
        self.arrivals[self.arrival_pos] = Arrival { ts, at: Some(Instant::now()) };
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

// ═══════════════════════ 漂移校正 ═══════════════════════

/// 有界回落动作：水位跑出回落带时做的一次**单样本块**修正。
///
/// 所有动作都必须在 `SLIP_FADE_SAMPLES`（1 ms）的交叉淡化区内完成，
/// 由调用方的 PCM 处理函数负责；本模块只决定"丢/插多少"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slip {
    None,
    /// 丢弃 `n` 个样本（每声道）
    Drop(u32),
    /// 插入 `n` 个样本（每声道）
    Insert(u32),
}

impl Slip {
    /// 折算成"媒体时间偏移"：丢弃 → 播放头前移（正），插入 → 后退（负）
    pub fn signed_samples(self) -> i32 {
        match self {
            Slip::None => 0,
            Slip::Drop(n) => n as i32,
            Slip::Insert(n) => -(n as i32),
        }
    }
}

/// 一次校正评估的产物：比率微调（每 10 ms）+ 可能的有界回落（每秒 ≤1 次）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftUpdate {
    /// 重采样比率微调量，范围 ±[`RESAMPLE_MAX_ADJUST`]。
    /// 语义：`实际比率 = 固定比率(源/设备) × (1 + adjust)`。
    /// 水位偏高 → adjust > 0 → 每秒产出的设备帧变少 → 水位回落。
    pub adjust: f64,
    /// 有界回落（绝大多数情况下是 `Slip::None`）
    pub slip: Slip,
}

impl Default for DriftUpdate {
    fn default() -> Self {
        Self { adjust: 0.0, slip: Slip::None }
    }
}

/// 漂移 / 延迟校正器（规范 7.9）。
///
/// **第一级：受控重采样（主力）**
/// 水位偏差经一阶低通后按比例映射到重采样比率的 ±0.5%，每 10 ms 更新一次，
/// 单次步进 ≤0.0001。比率是连续量：声卡端听到的只是极其缓慢的"变调"，
/// 不存在任何样本级接缝——这是专业音频接收端的标准做法，也是彻底消灭
/// 离散咔嗒声的关键。
///
/// **第二级：有界回落（安全网）**
/// 只有当比率已经顶满、水位仍然持续跑出回落带时才启用：
/// * 高于目标 +30 ms 且连续 5 次评估都在带内 → 丢 1 ms；
/// * 低于目标 −10 ms → 插 1 ms；
/// * 两次之间至少间隔 1 s 音频时间（每秒最多 1 ms），绝不连续触发；
/// * 高带要求"稳定"，低带不要求——插入本身会把水位抬回带内，天然自限。
///
/// 两者都用**音频样本计数**计时，不读墙钟、不依赖任何定时器。
pub struct DriftController {
    /// 低通后的水位（ms），只用于比率支路
    water: f64,
    /// 当前比率微调量
    adjust: f64,
    primed: bool,
    last_ratio: u64,
    last_eval: u64,
    last_slip: u64,
    /// 高带连续评估计数
    streak: u32,
    pub ratio_updates: u64,
    pub slip_drops: u64,
    pub slip_inserts: u64,
    /// 累计回落的样本数（每声道）
    pub slipped_samples: u64,
    /// 比率微调累计造成的媒体时间偏移（样本，诊断用）
    pub adjust_integral: f64,
}

impl DriftController {
    pub fn new() -> Self {
        Self {
            water: 0.0,
            adjust: 0.0,
            primed: false,
            last_ratio: 0,
            last_eval: 0,
            last_slip: 0,
            streak: 0,
            ratio_updates: 0,
            slip_drops: 0,
            slip_inserts: 0,
            slipped_samples: 0,
            adjust_integral: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.water = 0.0;
        self.adjust = 0.0;
        self.primed = false;
        self.last_ratio = 0;
        self.last_eval = 0;
        self.last_slip = 0;
        self.streak = 0;
    }

    /// 当前比率微调量（供诊断/日志读取）
    pub fn adjust(&self) -> f64 {
        self.adjust
    }

    /// `water_ms` 为**总缓冲深度**（抖动缓冲 + 输出队列），
    /// `media_samples` 为累计输出的音频样本数（10 ms 帧 → 每帧 +480）。
    pub fn update(&mut self, water_ms: u32, media_samples: u64) -> DriftUpdate {
        if !self.primed {
            self.primed = true;
            self.water = water_ms as f64;
            self.last_ratio = media_samples;
            self.last_eval = media_samples;
            // 启动宽限从"现在"起算：前 2 s 不允许回落
            self.last_slip = media_samples;
            return DriftUpdate::default();
        }

        // ── 第一级：比率微调（每 10 ms 一次）──
        if media_samples.saturating_sub(self.last_ratio) >= RATIO_UPDATE_SAMPLES {
            self.last_ratio = media_samples;
            self.water += WATER_SMOOTH_ALPHA * (water_ms as f64 - self.water);
            // 水位偏高 → 需要"少产出"→ ratio 增大 → adjust 为正
            let err = self.water - JITTER_TARGET_MS as f64;
            let mag = err.abs();
            let want = if mag <= RATIO_DEADBAND_MS {
                0.0
            } else {
                let k = ((mag - RATIO_DEADBAND_MS) / (RATIO_FULL_MS - RATIO_DEADBAND_MS)).min(1.0);
                err.signum() * RESAMPLE_MAX_ADJUST * k
            };
            let step = (want - self.adjust).clamp(-RATIO_SLEW_STEP, RATIO_SLEW_STEP);
            self.adjust = (self.adjust + step).clamp(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST);
            self.ratio_updates += 1;
        }

        // ── 第二级：有界回落（每 100 ms 评估，每秒最多出手一次）──
        let slip = self.evaluate_slip(water_ms, media_samples);
        DriftUpdate { adjust: self.adjust, slip }
    }

    fn evaluate_slip(&mut self, water_ms: u32, media_samples: u64) -> Slip {
        if media_samples.saturating_sub(self.last_eval) < SLIP_EVAL_SAMPLES {
            return Slip::None;
        }
        self.last_eval = media_samples;

        // 启动宽限：水位从 0 爬到目标的过程中不回落
        if media_samples < SLIP_WARMUP_SAMPLES {
            self.streak = 0;
            return Slip::None;
        }

        let high = JITTER_TARGET_MS + SLIP_HIGH_MARGIN_MS;
        let low = JITTER_TARGET_MS.saturating_sub(SLIP_LOW_MARGIN_MS);

        if water_ms > high {
            self.streak += 1;
        } else {
            self.streak = 0;
        }

        // 速率闸门：每秒最多 1 ms，绝不连续触发（1 s 音频样本时间）
        if media_samples.saturating_sub(self.last_slip) < SLIP_MIN_INTERVAL_SAMPLES {
            return Slip::None;
        }

        if self.streak >= SLIP_STABLE_ROUNDS {
            self.streak = 0;
            self.last_slip = media_samples;
            self.slip_drops += 1;
            self.slipped_samples += SLIP_SAMPLES as u64;
            return Slip::Drop(SLIP_SAMPLES);
        }
        if water_ms < low {
            self.streak = 0;
            self.last_slip = media_samples;
            self.slip_inserts += 1;
            self.slipped_samples += SLIP_SAMPLES as u64;
            return Slip::Insert(SLIP_SAMPLES);
        }
        Slip::None
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
/// - `offset`：漂移校正累计偏移（样本），由有界回落的丢/插**和**重采样
///   比率微调的连续积分共同写入。
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
    /// 比率微调造成的**不足一个样本**的残余（避免每帧四舍五入吃掉校正量：
    /// u=0.005、每帧 480 帧时每帧只有 2.4 个样本，逐帧取整会丢掉 17%）
    offset_frac: f64,
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
            offset_frac: 0.0,
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
        self.offset_frac = 0.0;
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

    /// 重采样比率微调造成的媒体时间偏移（**连续量**，每帧调用）。
    ///
    /// 比率乘以 `(1+u)` 后，`n` 个设备帧对应的媒体时间变成 `n×(1+u)`，
    /// 也就是每个设备帧要多算 `u` 个媒体样本。逐帧量是小数（u=0.005 时
    /// 每 480 帧只有 2.4 个样本），因此必须用 f64 累积、只把整数部分
    /// 加到 `offset` 上；每帧四舍五入会丢掉近 17% 的校正量，
    /// 表现为水位与播放头缓慢背道而驰。
    pub fn note_ratio_correction(&mut self, samples: f64) {
        if !samples.is_finite() {
            return;
        }
        self.offset_frac += samples;
        let whole = self.offset_frac.trunc();
        if whole != 0.0 {
            self.offset = self.offset.saturating_add(whole as i64);
            self.offset_frac -= whole;
        }
    }

    pub fn reset(&mut self) {
        self.started = false;
        self.base_ts = 0;
        self.base_device_frames = 0;
        self.remainder = 0;
        self.last_device_frames = 0;
        self.last_value = 0;
        self.offset = 0;
        self.offset_frac = 0.0;
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

    // ── 漂移校正 ──

    /// 按 10 ms 一帧推进 `frames` 帧，返回期间出现的所有回落动作
    fn run_drift(d: &mut DriftController, water: u32, frames: u64, media: &mut u64) -> Vec<Slip> {
        let mut slips = Vec::new();
        for _ in 0..frames {
            *media += FRAME_SAMPLES as u64;
            let u = d.update(water, *media);
            if u.slip != Slip::None {
                slips.push(u.slip);
            }
        }
        slips
    }

    #[test]
    fn drift_on_target_does_nothing() {
        // 水位稳定在目标上：比率不动、不回落
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(JITTER_TARGET_MS, media);
        let slips = run_drift(&mut d, JITTER_TARGET_MS, 1000, &mut media);
        assert!(slips.is_empty());
        assert_eq!(d.adjust(), 0.0);
        assert_eq!(d.slip_drops, 0);
        assert_eq!(d.slip_inserts, 0);
    }

    #[test]
    fn drift_ignores_jitter_inside_deadband() {
        // 目标 ±5 ms 内的小抖动不得触发任何动作（否则会与水位的自然抖动打架）
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(JITTER_TARGET_MS, media);
        let mut slips = Vec::new();
        for i in 0..2000u64 {
            media += FRAME_SAMPLES as u64;
            let water = JITTER_TARGET_MS as i32 + if i % 2 == 0 { 4 } else { -4 };
            if d.update(water.max(0) as u32, media).slip != Slip::None {
                slips.push(i);
            }
        }
        assert!(slips.is_empty(), "死区内不得回落");
        assert!(d.adjust().abs() < 1e-9, "死区内不得动比率: {}", d.adjust());
    }

    #[test]
    fn drift_ratio_is_bounded_and_has_expected_sign() {
        // 水位高出 40 ms：比率向上顶，但绝不越过 ±0.5%
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(100, media);
        let mut max_adjust: f64 = 0.0;
        for _ in 0..200 {
            media += FRAME_SAMPLES as u64;
            let u = d.update(100, media).adjust;
            max_adjust = max_adjust.max(u.abs());
            assert!(u <= RESAMPLE_MAX_ADJUST + 1e-12, "比率越界: {u}");
        }
        assert!(max_adjust > RESAMPLE_MAX_ADJUST * 0.99, "高水位应把比率顶到上限，实际 {max_adjust}");
        assert!(d.adjust() > 0.0, "水位偏高时 adjust 必须为正");
    }

    #[test]
    fn drift_ratio_slew_is_gradual() {
        // 单次比率更新步进受限：不允许一帧之内跳到满幅（那是可闻的变调台阶）
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(300, media);
        let mut prev = 0.0f64;
        for _ in 0..60 {
            media += FRAME_SAMPLES as u64;
            let u = d.update(300, media).adjust;
            assert!((u - prev).abs() <= RATIO_SLEW_STEP + 1e-12, "步进过大: {prev} → {u}");
            prev = u;
        }
        assert!(prev.abs() > 0.0);
    }

    #[test]
    fn drift_ratio_decays_back_to_zero() {
        // 水位回到目标后比率必须归零，否则会永久变调
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(200, media);
        for _ in 0..200 {
            media += FRAME_SAMPLES as u64;
            d.update(200, media);
        }
        assert!(d.adjust() > 0.004);
        for _ in 0..400 {
            media += FRAME_SAMPLES as u64;
            d.update(JITTER_TARGET_MS, media);
        }
        assert_eq!(d.adjust(), 0.0, "回到目标后比率必须归零");
    }

    #[test]
    fn drift_slip_high_band_needs_stability() {
        // 单次尖峰（<500 ms）不得触发丢样
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(JITTER_TARGET_MS, media);
        // 先过启动宽限
        run_drift(&mut d, JITTER_TARGET_MS, 400, &mut media);
        // 300 ms 尖峰
        let slips = run_drift(&mut d, 200, 30, &mut media);
        assert!(slips.is_empty(), "不稳定尖峰不得回落: {slips:?}");
        // 回到目标再跑 1 s，仍然不得回落
        let slips = run_drift(&mut d, JITTER_TARGET_MS, 100, &mut media);
        assert!(slips.is_empty());
        assert_eq!(d.slip_drops, 0);
    }

    #[test]
    fn drift_slip_rate_is_one_ms_per_second() {
        // 水位持续高企：回落节奏必须是"每秒最多 1 ms、每次 1 ms"
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(200, media);
        // 10 s，仅统计宽限期之后的动作
        let slips = run_drift(&mut d, 200, 1000, &mut media);
        assert!(slips.iter().all(|s| *s == Slip::Drop(SLIP_SAMPLES)), "{slips:?}");
        // 每秒最多一次：10 s 内不会超过 9 次（首个 2 s 是宽限）
        assert!(slips.len() <= 9, "回落过频: {} 次 / 10 s", slips.len());
        assert!(slips.len() >= 6, "水位长期偏高却几乎不回落: {}", slips.len());
    }

    #[test]
    fn drift_slip_is_never_consecutive() {
        // 任意两次回落之间必须至少间隔 1 s 音频时间
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(200, media);
        let mut last: Option<u64> = None;
        for _ in 0..2000 {
            media += FRAME_SAMPLES as u64;
            if d.update(200, media).slip != Slip::None {
                if let Some(prev) = last {
                    assert!(
                        media - prev >= SLIP_MIN_INTERVAL_SAMPLES,
                        "两次回落间隔仅 {} 样本",
                        media - prev
                    );
                }
                last = Some(media);
            }
        }
        assert!(last.is_some());
    }

    #[test]
    fn drift_slip_low_band_inserts() {
        // 水位被打空（环即将欠载）：按 1 ms/s 插入，把水位抬回来
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(JITTER_TARGET_MS, media);
        run_drift(&mut d, JITTER_TARGET_MS, 400, &mut media);
        let slips = run_drift(&mut d, 10, 200, &mut media);
        assert!(!slips.is_empty(), "水位 10 ms 必须触发插入");
        assert!(slips.iter().all(|s| *s == Slip::Insert(SLIP_SAMPLES)), "{slips:?}");
        assert_eq!(d.slip_inserts, slips.len() as u64);
        assert_eq!(d.slip_drops, 0);
    }

    #[test]
    fn drift_slip_band_edges_are_exact() {
        // 高带 = 目标+30：等于 90 不动作，91 动作；低带 = 目标-10：等于 50 不动作
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(JITTER_TARGET_MS, media);
        let slips = run_drift(&mut d, JITTER_TARGET_MS + SLIP_HIGH_MARGIN_MS, 400, &mut media);
        assert!(slips.is_empty(), "带边不得动作: {slips:?}");

        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(JITTER_TARGET_MS, media);
        let slips = run_drift(&mut d, JITTER_TARGET_MS - SLIP_LOW_MARGIN_MS, 400, &mut media);
        assert!(slips.is_empty(), "低带边不得动作: {slips:?}");

        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(JITTER_TARGET_MS, media);
        let slips = run_drift(&mut d, JITTER_TARGET_MS + SLIP_HIGH_MARGIN_MS + 1, 400, &mut media);
        assert!(!slips.is_empty(), "越过带边必须动作");
    }

    #[test]
    fn drift_no_slip_during_warmup() {
        // 启动阶段水位从 0 爬升：哪怕长时间低位也不得插入
        let mut d = DriftController::new();
        let mut media = 0u64;
        d.update(0, media);
        let slips = run_drift(&mut d, 0, 190, &mut media); // 1.9 s
        assert!(slips.is_empty(), "启动宽限内不得回落: {slips:?}");
    }

    #[test]
    fn drift_slip_offsets_media_time() {
        // 回落/插入的符号语义：丢 → 播放头前移；插 → 播放头后退
        assert_eq!(Slip::Drop(SLIP_SAMPLES).signed_samples(), SLIP_SAMPLES as i32);
        assert_eq!(Slip::Insert(SLIP_SAMPLES).signed_samples(), -(SLIP_SAMPLES as i32));
        assert_eq!(Slip::None.signed_samples(), 0);
    }

    #[test]
    fn playhead_accumulates_fractional_ratio_correction() {
        // 每帧 2.4 个样本的连续校正不得被逐帧取整吃掉（取整会丢 17%）
        let mut clock = PlayheadClock::new(48_000);
        clock.start(0, 0);
        for _ in 0..100 {
            clock.note_ratio_correction(480.0 * 0.005);
        }
        let v = clock.value().unwrap();
        assert!((v as i64 - 240).abs() <= 1, "100 帧 × 2.4 样本 ≈ 240，实际 {v}");
        // 长跑 1000 帧：2400 样本，误差仍 ≤1（小数残留在 offset_frac 里）
        for _ in 0..900 {
            clock.note_ratio_correction(480.0 * 0.005);
        }
        let v = clock.value().unwrap();
        assert!((v as i64 - 2400).abs() <= 1, "1000 帧 ≈ 2400，实际 {v}");
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

    #[test]
    fn jb_prebuffer_threshold_is_three_frames() {
        // JITTER_START_MS = 30 → 3 帧即可起播（配合受控重采样的低水位稳态）
        let mut jb = JitterBuffer::new();
        for i in 0..2u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        assert_eq!(jb.pull(0, &mut [0u8; 1275]), Pull::Wait, "2 帧不得起播");
        insert(&mut jb, 2, 960, false, 3);
        assert_eq!(pull_tags(&mut jb, 0, 3), vec![1, 2, 3]);
    }

    #[test]
    fn jb_realigns_when_seq_lags_behind_ts() {
        // BUG-01/02 回归：发送端在 ACK 中断 / DTX 期间"时间戳前进、序号冻结"，
        // 接收端同时用 PLC/静音把 next_seq 推进了同样的量。恢复包时间戳连续、
        // 序号却落后——必须按时间戳重新对齐，而不是把整条后续流判成迟到。
        let mut jb = JitterBuffer::new();
        for i in 0..8u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        // 全部播掉（next_seq = 8, next_ts = 3840）
        pull_tags(&mut jb, 0, 8);
        // DTX/断流 500 ms：无包，播放头推进 50 帧
        for k in 0..50u64 {
            let _ = jb.pull(480 * (8 + k), &mut [0u8; 1275]);
        }
        assert_eq!(jb.next_seq(), 58);
        assert_eq!(jb.next_ts(), 480 * 58);

        // 恢复包：序号仍是 8（未随静音递增），时间戳正好接上，M=1
        let r = insert(&mut jb, 8, 480 * 58, true, 9);
        assert_eq!(r, InsertResult::Resync, "序号滞后必须重新对齐而不是丢弃");
        assert_eq!(jb.stats.seq_realign, 1);
        assert_eq!(jb.stats.late, 0);
        assert_eq!(jb.next_seq(), 8);
        assert_eq!(jb.next_ts(), 480 * 58);

        // 后续包全部正常接纳并出音
        for i in 1..=10u16 {
            assert_eq!(
                insert(&mut jb, 8 + i, 480 * (58 + i as u32), false, 10 + i as u8),
                InsertResult::Inserted
            );
        }
        let tags = pull_tags(&mut jb, 480 * 58, 11);
        assert_eq!(tags.len(), 11);
        assert!(tags.iter().all(|&t| t > 0), "重同步后不得再产生 PLC/静音");
    }

    #[test]
    fn jb_long_gap_across_seq_wrap_still_recovers() {
        // BUG-08 回归：断流跨度超过 32767 个包时，16 位有符号差值会把恢复包
        // 的序号扩成负数；只要时间戳合法就必须能重新对齐。
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        // 播掉一帧，进入 PLAYING
        let _ = jb.pull(0, &mut [0u8; 1275]);
        // 断流 40000 帧（约 6.7 分钟）：接收端靠静音推进
        let gap = 40_000u32;
        for k in 0..gap as u64 / 8 {
            let _ = jb.pull(480 * (1 + k * 8), &mut [0u8; 1275]);
        }
        // 恢复包时间戳正确接续，序号因回绕看起来"倒退"
        let resume_ts = 480 * gap;
        let r = insert(&mut jb, (gap as u16).wrapping_add(1), resume_ts, true, 2);
        assert_eq!(r, InsertResult::Resync);
        assert_eq!(jb.next_ts(), resume_ts as u64);
        assert!(jb.stats.late <= 1, "不得把恢复包判成迟到: {}", jb.stats.late);
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
