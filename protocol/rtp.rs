//! RTP 媒体承载（RTP version 2 / PT=111 / 48 kHz / 固定 10 ms）
//!
//! 本模块是 v3 媒体面的唯一真源，包含三部分：
//!
//! 1. **线格式**：[`RtpHeader`] / [`parse`] / [`write_packet`]，标准 12 字节 RTP 固定头，
//!    无 CSRC、无扩展、无填充，负载就是 Opus 包本身；
//! 2. **扩展序号/时间戳**：[`extend_seq`] / [`extend_ts`]，用有符号差值把 16/32 位
//!    回绕扩展成 64 位单调计数；
//! 3. **接收侧时钟模型**（ASRC）：唯一时钟是声卡，唯一观测量是缓冲水位，
//!    唯一执行手段是重采样比率。`JitterBuffer` 只负责"按水位存放"并给出
//!    "还能听多久"；[`RateEstimator`] 用最小延迟滤波从 (时间戳, 到达时刻)
//!    回归出速率前馈，[`DriftController`] 用抗饱和 PI 把总水位钉在
//!    [`WATER_TARGET_MS`]。**播放决策只由水位驱动**，本模块内不存在任何
//!    定时器、`sleep`、墙钟计时或"设备帧 → 媒体时间戳"的换算链。

// ═══════════════════════ 常量 ═══════════════════════

use std::time::{Duration, Instant};

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
/// 目标总水位（15 帧）——**单一口径**：抖动缓冲 + PCM 环 + 声卡缓冲。
///
/// 旧实现把声卡缓冲排除在外（目标只约束抖动缓冲 + PCM 环），实际总水位
/// 因此被悄悄抬到 70–80 ms，"目标 60 ms"这句话失去意义。现在三者同口径
/// 相加：这个数就是"从可听点往后还压着多少音频"，也是控制器唯一的输入。
///
/// **这个值由环境实测决定：**
///
/// * v1.3.0 取 260 ms，是因为当时链路上有"每 60–120 s 一次、双向同时
///   600–1100 ms 的停摆"（手机全频段 Wi-Fi 扫描，`com.vivo.multinlp`）。
///   那 260 ms 里约 200 ms 纯粹是在扛扫描风暴——风暴一停就全是多余延迟。
/// * v1.4.0 关掉扫描（外加双端 DSCP 46）后，同一链路 4 次长跑 **0 次 >200 ms**，
///   最大空档 68–90 ms；按 ×1.6 余量取整 → **150 ms**（PCM 环 44 + 声卡 10 +
///   抖动缓冲 ~96），比 260 低 110 ms。
/// * 若环境里重新出现扫描源（换路由器 / 换手机 / 对方装新 App），水位会
///   周期性地被顶穿、`adjust` 贴回 ±5000。此时用环境变量
///   [`WATER_TARGET_ENV`] 临时抬高目标（`UDP2CAL_WATER_TARGET_MS=220`），
///   不要改编译常数；定值规则是"目标 ≥ 实测最大空档 ×1.5"。
///
/// 这是整套实现里唯一跟环境相关的常数。**不要靠压低它去赢延迟**：压低只会
/// 把延迟换成 PLC 破音与常年变调（见 [`STEP_TRIM_EXCESS_MS`] 的配套措施）。
pub const WATER_TARGET_MS: u32 = 150;

/// 目标水位的环境变量覆盖名（现场调参不改编译，见 [`water_target_ms`]）。
pub const WATER_TARGET_ENV: &str = "UDP2CAL_WATER_TARGET_MS";
/// 覆盖值的下界：必须 ≥ `PLC_LEAD_MS + DECODE_LIMIT_MS` 才有意义
pub const WATER_TARGET_MIN_MS: u32 = 80;
/// 覆盖值的上界：再大就是拿延迟换稳定性，超出说明环境已不适合实时音频
pub const WATER_TARGET_MAX_MS: u32 = 600;

/// 运行时目标水位：默认 [`WATER_TARGET_MS`]，可用 `UDP2CAL_WATER_TARGET_MS` 覆盖。
///
/// 只在首次调用时读一次环境变量（`OnceLock` 缓存）：控制器每 10 ms 调一次，
/// 不能每次都 `getenv`。取值被钳到 [`WATER_TARGET_MIN_MS`, `WATER_TARGET_MAX_MS`]，
/// 非法值（非数字、空串）静默退回常数。
pub fn water_target_ms() -> u32 {
    static CACHED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var(WATER_TARGET_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map(|v| v.clamp(WATER_TARGET_MIN_MS, WATER_TARGET_MAX_MS))
            .unwrap_or(WATER_TARGET_MS)
    })
}

// ── 阶跃吸收参数（方案 C：整帧裁剪）──
//
// 阶跃来自网络：AP 攒批交付时十几帧同时到，`jb` 一次涨 100 ms 以上。比率排水
// 只有 5 ms/s，排 100 ms 要 20 s，期间 PI 一直贴 ±5000 ppm（±8.6 音分）——
// 这就是听感上的"变调"。既然阶跃本身是离散事件，就用**整帧级**硬重同步吃掉它：
// 丢掉 n 帧、时间线硬前移，不产生 PLC、不做样本级丢插（接缝比整帧跳更差）。

/// 单次 tick 内 `jb.water_ms()` 增长超过它 → 判为阶跃（一次补齐）
pub const STEP_TRIM_GROWTH_MS: u32 = 80;
/// 总水位超过 `target + 它` → 判为阶跃（水位已被顶穿，不管增长量）
pub const STEP_TRIM_EXCESS_MS: u32 = 100;
/// 阶跃吸收的每分钟限额：超限后**只记录不裁剪**，避免参数过激时听成"跳音"
pub const STEP_TRIM_MAX_PER_MIN: u32 = 3;
/// 解码缓冲（PCM 环 + 声卡）的深度上限：达到它就不再预解码。
///
/// 这个阈值守的是**可听点**：解码缓冲是"已经解码、还没出声"的媒体量，
/// 缺口判定用的就是它（`next_ts − 播放头` 恒等于它）。旧的 40 ms 硬编码在
/// `main.rs` 里，P4 之后与缺口判定同处一个口径。
pub const DECODE_LIMIT_MS: u32 = 40;
/// 提前生成 PLC 的提前量：解码缓冲只剩这么多时，缺口已经逼近可听点
pub const PLC_LEAD_MS: u32 = 20;
/// 连续 PLC 上限（12 帧）
pub const PLC_MAX_MS: u32 = 120;
/// 触发重同步的时间戳跳变阈值
pub const RESYNC_GAP_MS: u32 = 200;

// ── 漂移校正参数（规范 7.9）──
//
// 唯一执行手段是重采样比率，前馈管速率、反馈管相位：
//
//   实际比率 = (源/设备) × (1 + 前馈估计) × (1 + PI 输出)
//
// 前馈估计回答"发送端的 1 秒有多长"（晶振频差），PI 只负责把水位残差收干净。
// 没有第二级"丢/插 1 ms"离散回落：样本级接缝只可能比连续比率更差，而任何
// 需要"记住偏移"的中间表示（播放头 + offset + offset_frac）都说明口径选错了。

/// 重采样比率的微调上限（±0.5%）
pub const RESAMPLE_MAX_ADJUST: f64 = 0.005;
/// 比率更新周期：每 10 ms（1 帧）更新一次
pub const RATIO_UPDATE_SAMPLES: u64 = FRAME_SAMPLES as u64;
/// 单次比率更新的最大步进。0.0001/10 ms = 1%/s，
/// 到达 ±0.5% 满幅需要 0.5 s——既是变调不可闻的斜坡，也足够跟上真实漂移。
pub const RATIO_SLEW_STEP: f64 = 0.0001;
/// 比例增益：水位偏差每 1 ms → 0.005% 比率。
///
/// 刻意比旧的"偏差 20 ms 即 0.5%"小 5 倍：速率差已经由前馈吃掉，比例项
/// 只处理慢变化；放大它会直接把这个闭环变成"抖动 → 变调"的调制器
/// （规范 §六 风险 2）。20 ms 偏差 → 1000 ppm。
pub const PI_KP_PER_MS: f64 = 0.00005;
/// 积分时间常数（秒）。规范 §六 风险 2 要求 ≥5 s；取 10 s 让积分项只吸收
/// 前馈的残余误差，不被网络抖动牵着走。`KI = KP / Ti`。
pub const PI_TI_S: f64 = 10.0;
/// 水位一阶低通系数（每次更新 10 ms，等效时间常数 ≈ 150 ms）
pub const WATER_SMOOTH_ALPHA: f64 = 0.065;
/// PI 的最小更新间隔（ms）：更短的间隔只累计、不评估
pub const PI_MIN_UPDATE_MS: f64 = 5.0;

// ── 速率前馈参数（规范 §四）──

/// 观测窗长度（秒）。
///
/// 精度下界 `σ_δ ≈ σ_J·√12·√τ_j/T^1.5`（τ_j≈50 ms）：T=10 s 在 σ_J=2 ms 时
/// 约 49 ppm，T=30 s 约 9 ppm。典型晶振差 100 ppm 要靠 30 s 窗口才压得住，
/// 所以取 30 s；窗口未满时照常出估计并上报 1σ，日志里能直接判断可信度。
pub const RATE_WINDOW_S: f64 = 30.0;
/// 最小延迟滤波的分桶数（每桶只保留桶内最小单向时延）
pub const RATE_BUCKETS: usize = 30;
/// 前馈估计钳位（±2%）：真实晶振差 ≤100 ppm，两个量级之外才是安全边界
pub const RATE_FEEDFORWARD_MAX: f64 = 0.02;
/// 至少这么多桶（=5 s 跨度）才对外出估计
pub const RATE_MIN_BUCKETS: usize = 6;
/// 单次观测允许的最大媒体跳变：超过它说明时间线被重建（重同步、发送端
/// 重启、断流后补齐），旧窗口的 `t_local` 与 `media` 已不可比，必须作废重算。
/// 正常相邻两包只差 480 样本，这里给 2 s 的宽松门限。
pub const RATE_JUMP_MAX_SAMPLES: u64 = OPUS_SAMPLE_RATE as u64 * 2;

#[inline]
pub const fn ms_to_samples(ms: u32) -> u32 {
    ms * FRAME_SAMPLES / FRAME_MS
}

#[inline]
pub const fn samples_to_ms(samples: u64) -> u64 {
    samples * 1000 / OPUS_SAMPLE_RATE as u64
}

/// 媒体样本数 → 秒（速率估计器与日志用，f64 精度）
#[inline]
pub fn samples_to_secs(samples: u64) -> f64 {
    samples as f64 / OPUS_SAMPLE_RATE as f64
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
    /// 因阶跃吸收被跳过的整帧总数（方案 C，见 [`JitterBuffer::skip_frames`]）
    pub step_trims: u64,
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
    next_seq: u64,
    next_ts: u64,
    highest_seq: u64,
    highest_ts: u64,
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
            last_rebase_reason: String::new(),
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

    /// 当前水位（毫秒）：**已到达但还没播出去的媒体量**。
    ///
    /// 用 `highest_ts − next_ts` 而不是 `contiguous_frames()`：后者在"队首那一帧
    /// 还没到"（乱序/迟到）时会瞬间塌到 0，而后面明明还压着十几帧——把它当控制
    /// 信号会让 PI 误以为缓冲空了、反向拉满比率，实机上是水位长期锯齿的主因。
    /// `highest_ts − next_ts` 恒等于"发送端已交付、本地还没消费"的帧数，乱序不塌陷。
    pub fn water_ms(&self) -> u32 {
        if self.highest_ts < self.next_ts {
            return 0;
        }
        let frames = ((self.highest_ts - self.next_ts) / FRAME_SAMPLES as u64 + 1)
            .min(MAX_REORDER_PACKETS as u64);
        frames as u32 * FRAME_MS
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
            self.last_rebase_reason = format!(
                "序号滞后 {} 帧，按时间戳重新对齐",
                self.next_seq.saturating_sub(ext_seq)
            );
            self.rebase(ext_seq, ext_ts);
            self.store(pkt, ext_seq, ext_ts);
            self.stats.resyncs += 1;
            self.stats.seq_realign += 1;
            self.stats.inserted += 1;
            return InsertResult::Resync;
        }

        // M=1 且时间戳跳变超过阈值 → 重同步；跳变很小只更新标记状态
        if pkt.marker && gap_ms > RESYNC_GAP_MS as u64 {
            self.last_rebase_reason = format!("M=1 且时间戳跳变 {}ms", gap_ms);
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

    /// 按**缓冲水位**拉取下一帧（规范 §二）。
    ///
    /// 两个入参是同一个量的两种口径，都从"可听点之前还压着多少媒体"派生：
    /// * `ring_ms`：已解码、已写进 PCM 环，但还没被声卡接手的媒体量；
    /// * `pipe_ms`：`ring_ms` + 声卡已接手但还没播完的（`device_latency`）。
    ///
    /// 为什么不能只用一个量：生产节流约束的是**我们自己能控制的**那段（PCM 环），
    /// 声卡缓冲是既定开销，把它算进节流门会让"设备延迟 40 ms"直接死锁；
    /// 而缺口判定必须用 `pipe_ms`——只有可听点真的逼近缺口时才该补 PLC。
    ///
    /// 旧实现用 48 kHz 播放头时间戳判定缺口，两者恒等：
    /// `next_ts − playhead_ts` 正是 `pipe_ms` 折算的样本量，所以这里直接用
    /// 水位表述，从而删掉整条"设备帧 → 媒体时间戳"的换算链。
    ///
    /// 返回 `Wait` 表示"现在不该生产"，调用方必须继续等待，而不得用定时器补帧。
    pub fn pull(&mut self, ring_ms: u32, pipe_ms: u32, out: &mut [u8]) -> Pull {
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
            self.slots[idx].valid = false;
            let ts = self.next_ts;
            self.advance();
            self.plc_run = 0;
            return Pull::Packet { len, ts };
        }

        // 可听点之前还压着的媒体量已经见底，才生成补偿
        if pipe_ms <= PLC_LEAD_MS {
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

    /// 阶跃吸收：硬跳 `n` 帧（方案 C，规范 §七）。
    ///
    /// 用于"一次补齐把水位顶穿"的场合：丢掉播放头起的 `n` 帧、时间线直接前移，
    /// 让水位一次回到目标附近，而不是让 PI 用 ±0.5%（5 ms/s）慢慢排水——
    /// 排 100 ms 要 20 s，那 20 s 里比率一直贴钳位，折合 ±8.6 音分，就是听感
    /// 上的变调。
    ///
    /// 三条边界：
    /// * 只在 PLAYING 且前方确有媒体时才动（`next_ts < highest_ts`），绝不越过
    ///   已到达的最新帧去空转；
    /// * **不**产生 PLC 帧、**不**推进 `epoch`：解码器与重采样器都不需要重建，
    ///   只是少放几帧；
    /// * 计数器记在 `stats.step_trims`，便于按"每分钟 ≤3 次"做限额与观察。
    ///
    /// 返回真正跳过的帧数（可能小于 `n`）。
    pub fn skip_frames(&mut self, n: u32) -> u32 {
        if !self.started || !self.playing || n == 0 {
            return 0;
        }
        let mut skipped = 0u32;
        for _ in 0..n {
            // 前方没有已到达的媒体就停：继续 advance 只会把播放头推进到虚空
            if self.next_ts >= self.highest_ts {
                break;
            }
            if let Some(idx) = self.find_ts(self.next_ts) {
                self.slots[idx].valid = false;
            }
            self.advance();
            skipped += 1;
        }
        if skipped > 0 {
            // 跳掉的帧不构成 PLC 连续段：新的一段从下一帧重新起算
            self.plc_run = 0;
            self.stats.step_trims += skipped as u64;
        }
        skipped
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
        self.started = false;
        self.playing = false;
        self.plc_run = 0;
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

// ═══════════════════════ 阶跃吸收（方案 C） ═══════════════════════

/// [`StepAbsorber::decide`] 的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepDecision {
    /// 建议跳过的帧数（0 = 本次不动）
    pub frames: u32,
    /// 命中判据但被每分钟限额压制：**只记录、不裁剪**
    pub limited: bool,
}

impl StepDecision {
    pub const NONE: Self = Self {
        frames: 0,
        limited: false,
    };
}

/// 阶跃吸收的判据 + 限额（方案 C）。
///
/// 单独成形是为了让 Windows（`main.rs` 解码循环）与 Android
/// （`CaptureService.reversePlayLoop`）用**同一套**阈值与限额，两端不必各写一份。
/// 本结构只看数字、不碰缓冲：调用方拿到 `frames` 后自己调
/// [`JitterBuffer::skip_frames`]，因此可以在同一把锁里完成决定、在外面记录日志。
///
/// 判据（命中任一即阶跃）：
/// * 单次 tick 内 `jb.water_ms()` 增长 > [`STEP_TRIM_GROWTH_MS`]；
/// * 总水位（`jb + pipe`）> `target + `[`STEP_TRIM_EXCESS_MS`]。
///
/// 限额：每 60 s 最多裁剪 [`STEP_TRIM_MAX_PER_MIN`] 次；超限后仍返回建议帧数，
/// 但 `limited = true`，调用方必须**不裁剪只记录**——参数过激时听感是"跳音"，
/// 宁可留着水位高也不能让它连续跳。
pub struct StepAbsorber {
    last_water_ms: u32,
    /// 是否已经建立"上一次水位"基准。第一次 `decide` 只登记不下结论：
    /// 否则启动时 `last_water_ms = 0` 会把正常水位当成一次巨大阶跃。
    primed: bool,
    window_start: Instant,
    trims_in_window: u32,
    /// 本窗口内的限额日志是否已经打过（节流用，见 [`StepAbsorber::take_limited_log`]）
    limited_logged: bool,
    /// 命中判据的次数（含被限额压制的，诊断）
    pub hits: u64,
}

impl StepAbsorber {
    pub fn new() -> Self {
        Self {
            last_water_ms: 0,
            primed: false,
            window_start: Instant::now(),
            trims_in_window: 0,
            limited_logged: false,
            hits: 0,
        }
    }

    /// 复位（重连 / 重同步 / 停止）：清掉"上一次增长量"与限额窗口
    pub fn reset(&mut self) {
        self.last_water_ms = 0;
        self.primed = false;
        self.window_start = Instant::now();
        self.trims_in_window = 0;
        self.limited_logged = false;
    }

    /// 当前 60 s 窗口内已用掉的裁剪次数（诊断，见规范 §7.13）
    pub fn trims_in_window(&self) -> u32 {
        self.trims_in_window
    }

    /// 限额压制日志的节流闸：**每个 60 s 窗口只放行一次**。
    ///
    /// 被压制时水位并不会自己降下来，所以判据每个 tick 都会再次命中；
    /// 不节流的话 Windows 编解码循环（≈250 次/s）与 Android 反向播放
    /// 线程（≈200 次/s）会把日志刷爆，把真正有用的 `[Latency]`/`[Reverse]`
    /// 挤出轮转窗口。返回 true 表示“本次可以打一条”。
    pub fn take_limited_log(&mut self) -> bool {
        if self.limited_logged {
            false
        } else {
            self.limited_logged = true;
            true
        }
    }

    /// 决定本 tick 要不要裁剪。
    ///
    /// `jb_water_ms` 为抖动缓冲水位，`pipe_ms` 为解码缓冲（PCM 环 + 声卡）水位，
    /// `target_ms` 为当前目标（通常 [`water_target_ms`]），`can_trim` 由调用方给
    /// （`jb.is_playing()`）：未进入 PLAYING 时水位本来就在爬升，不能当成阶跃。
    ///
    /// 无论是否可裁剪都会更新"上一次水位"，否则预缓冲期积累的增长量会在刚好
    /// 进入 PLAYING 的那一 tick 全部算成阶跃。
    pub fn decide(
        &mut self,
        jb_water_ms: u32,
        pipe_ms: u32,
        can_trim: bool,
        target_ms: u32,
    ) -> StepDecision {
        let grew = jb_water_ms.saturating_sub(self.last_water_ms);
        self.last_water_ms = jb_water_ms;
        if !self.primed {
            // 第一条观测只登记：从 0 爬到正常水位不是阶跃
            self.primed = true;
            return StepDecision::NONE;
        }
        if !can_trim {
            return StepDecision::NONE;
        }
        let total = jb_water_ms.saturating_add(pipe_ms);
        let over = total > target_ms.saturating_add(STEP_TRIM_EXCESS_MS);
        if grew <= STEP_TRIM_GROWTH_MS && !over {
            return StepDecision::NONE;
        }
        self.hits += 1;
        if self.window_start.elapsed() >= Duration::from_secs(60) {
            self.window_start = Instant::now();
            self.trims_in_window = 0;
            self.limited_logged = false;
        }
        // 只跳到目标：向上取整到整帧，且不超过 PLC 上限（避免一次跳掉半秒）
        let excess = total.saturating_sub(target_ms);
        let frames = excess.div_ceil(FRAME_MS).min(PLC_MAX_MS / FRAME_MS);
        if frames == 0 {
            return StepDecision::NONE;
        }
        if self.trims_in_window >= STEP_TRIM_MAX_PER_MIN {
            return StepDecision {
                frames,
                limited: true,
            };
        }
        self.trims_in_window += 1;
        StepDecision {
            frames,
            limited: false,
        }
    }
}

impl Default for StepAbsorber {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════ 速率估计（前馈） ═══════════════════════

// 前馈管速率、反馈管相位（规范 §四）。两条支路互不污染，所以各自可以挑最
// 合适的传感器：
//
// * 前馈用 **(RTP 扩展时间戳, 到达时刻)**：时间戳按 480/包推进，丢包不会让它
//   少算——它是发送端媒体时间线的无偏观测。用"已收样本数"则会每丢一包少 480，
//   估计值被丢包率直接带偏；
// * 反馈用 **缓冲水位**：它是闭环量，只能留在闭环里。把水位塞进速率回归会让
//   校正结果污染自己的输入，收敛点跟着漂。
//
// 观测对取的是"最新到达的那个包"的时间戳与到达时刻，两者来自同一条记录，
// 因此不含播放/解码节奏的任何抖动；再用最小延迟滤波（每桶只留最小单向时延）
// 剔掉排队造成的正向偏移——排队只会让单向时延变大，所以最小值几乎不受影响
// （NTP min-filter / WebRTC 自适应最小延迟同源）。

/// 一个最小延迟桶
#[derive(Clone, Copy)]
struct RateBucket {
    /// 该桶的序号（自 `t0` 起第几个桶）；`u64::MAX` 表示空桶
    epoch: u64,
    /// 桶中心时刻（秒，相对 `t0`）
    t: f64,
    /// 桶内观察到的最小单向时延（秒，相对首个观测）
    min_u: f64,
}

const EMPTY_BUCKET: RateBucket = RateBucket { epoch: u64::MAX, t: 0.0, min_u: f64::INFINITY };

/// 速率前馈估计器：回归出"发送端的 1 秒有多长"。
///
/// 输出 `feedforward` 是无量纲的速率残差：`+100 ppm` 表示发送端媒体时钟
/// 比本机时钟快 100 ppm，重采样比率必须相应调大 100 ppm 才能不涨水位。
/// 它只依赖本地单调时钟与 RTP 时间戳，不需要任何跨设备时间戳交换。
pub struct RateEstimator {
    /// 观测窗起点（首个观测的本地时刻）
    t0: Option<Instant>,
    /// 观测窗起点对应的媒体时间戳
    ts0: u64,
    /// 上一次喂入的时间戳（同一个包不重复喂）
    last_ts: u64,
    buckets: [RateBucket; RATE_BUCKETS],
    /// 当前估计（速率残差，无量纲）；样本不足时为 `None`
    feedforward: Option<f64>,
    /// 估计的 1σ（ppm），供日志判断可信度
    sigma_ppm: f64,
    /// 参与拟合的桶数
    used: usize,
    /// 累计观测数
    obs: u64,
}

impl RateEstimator {
    pub fn new() -> Self {
        Self {
            t0: None,
            ts0: 0,
            last_ts: 0,
            buckets: [EMPTY_BUCKET; RATE_BUCKETS],
            feedforward: None,
            sigma_ppm: 0.0,
            used: 0,
            obs: 0,
        }
    }

    /// 观测一个包：`ts` 是它的 RTP 扩展时间戳，`arrived` 是它进入接收端的
    /// 本地时刻。重复喂同一个时间戳会被忽略。
    pub fn observe(&mut self, ts: u64, arrived: Instant) {
        let t0 = match self.t0 {
            Some(t) => t,
            None => {
                // 首个观测只建立基准：窗口从"现在"起算
                self.t0 = Some(arrived);
                self.ts0 = ts;
                self.last_ts = ts;
                return;
            }
        };
        if ts == self.last_ts {
            return;
        }
        if ts < self.ts0 {
            // 基准之后又出现更早的时间戳：只可能是重同步换了基准，
            // 调用方本应 reset()；这里直接忽略，不污染现有窗口。
            return;
        }
        if ts.saturating_sub(self.last_ts) > RATE_JUMP_MAX_SAMPLES {
            // 时间线被重建（重同步/发送端重启/断流补齐）：旧窗口的
            // (t_local, media) 对已经不可比，接着用会让回归斜率变成垃圾
            // （实测会出现 ±2000 ppm 的假前馈）。从这一包重新起算。
            self.reset();
            self.t0 = Some(arrived);
            self.ts0 = ts;
            self.last_ts = ts;
            return;
        }
        self.last_ts = ts;
        self.obs += 1;

        let t_local = arrived.saturating_duration_since(t0).as_secs_f64();
        let media = samples_to_secs(ts - self.ts0);
        // u 只随两侧钟差变化：网络排队只会让它变大，最小延迟滤波正为此
        let u = t_local - media;

        let bucket_s = RATE_WINDOW_S / RATE_BUCKETS as f64;
        let epoch = (t_local / bucket_s) as u64;
        let idx = (epoch % RATE_BUCKETS as u64) as usize;
        let b = &mut self.buckets[idx];
        if b.epoch != epoch {
            *b = RateBucket { epoch, t: (epoch as f64 + 0.5) * bucket_s, min_u: u };
        } else if u < b.min_u {
            b.min_u = u;
        }
        self.fit(epoch);
    }

    /// 最小二乘拟合桶最小值。每次观测都重算：只有 30 个点，代价可忽略，
    /// 换来的是"任何时刻都能立刻读出当前估计"。
    fn fit(&mut self, cur_epoch: u64) {
        let mut n = 0usize;
        let mut sx = 0.0f64;
        let mut sy = 0.0f64;
        for b in self.buckets.iter() {
            if !self.bucket_live(b, cur_epoch) {
                continue;
            }
            n += 1;
            sx += b.t;
            sy += b.min_u;
        }
        self.used = n;
        if n < RATE_MIN_BUCKETS {
            // 窗口还没攒够：宁可不出估计（前馈置 0），也不要拿噪声去改比率
            self.feedforward = None;
            self.sigma_ppm = 0.0;
            return;
        }
        let mx = sx / n as f64;
        let my = sy / n as f64;
        let mut sxx = 0.0f64;
        let mut sxy = 0.0f64;
        for b in self.buckets.iter() {
            if !self.bucket_live(b, cur_epoch) {
                continue;
            }
            let dx = b.t - mx;
            sxx += dx * dx;
            sxy += dx * (b.min_u - my);
        }
        if sxx <= 0.0 {
            self.feedforward = None;
            self.sigma_ppm = 0.0;
            return;
        }
        let slope = sxy / sxx;
        let mut ss = 0.0f64;
        for b in self.buckets.iter() {
            if !self.bucket_live(b, cur_epoch) {
                continue;
            }
            let r = b.min_u - (my + slope * (b.t - mx));
            ss += r * r;
        }
        let dof = (n as f64 - 2.0).max(1.0);
        // 残差标准差 → 斜率标准差：σ_slope = s / √Sxx
        let sigma_slope = (ss / dof).sqrt() / sxx.sqrt();
        self.sigma_ppm = sigma_slope * 1e6;
        // u = 本地时间 − 媒体时间 的斜率：媒体时钟快 → u 变小 → 斜率 < 0
        // → 媒体时间 / 本地时间的残差 = −斜率
        let ff = (-slope).clamp(-RATE_FEEDFORWARD_MAX, RATE_FEEDFORWARD_MAX);
        self.feedforward = Some(ff);
    }

    /// 该桶是否仍在当前窗口内（被回收的桶 epoch 落后一整圈）
    fn bucket_live(&self, b: &RateBucket, cur_epoch: u64) -> bool {
        b.epoch != u64::MAX
            && b.min_u.is_finite()
            && cur_epoch.saturating_sub(b.epoch) < RATE_BUCKETS as u64
    }

    /// 当前前馈估计（速率残差，无量纲）；窗口未攒够时为 `None`
    pub fn feedforward(&self) -> Option<f64> {
        self.feedforward
    }

    /// 当前前馈估计（ppm），供日志
    pub fn feedforward_ppm(&self) -> Option<f64> {
        self.feedforward.map(|f| f * 1e6)
    }

    /// 估计的 1σ（ppm）
    pub fn sigma_ppm(&self) -> f64 {
        self.sigma_ppm
    }

    /// 参与拟合的桶数（窗口是否攒满看它）
    pub fn used_buckets(&self) -> usize {
        self.used
    }

    pub fn observations(&self) -> u64 {
        self.obs
    }

    /// 重同步 / 新会话：`highest_ts` 会重建基准，观测窗必须一起作废，
    /// 否则回归会把两条不同时间线混在一起算。
    pub fn reset(&mut self) {
        self.t0 = None;
        self.ts0 = 0;
        self.last_ts = 0;
        self.buckets = [EMPTY_BUCKET; RATE_BUCKETS];
        self.feedforward = None;
        self.sigma_ppm = 0.0;
        self.used = 0;
    }
}

impl Default for RateEstimator {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════ 漂移校正（反馈 PI） ═══════════════════════

/// 漂移校正器：**抗饱和 PI**，输入是单一口径的总水位（规范 7.9）。
///
/// 为什么是 PI 而不是纯比例：纯比例必然留下静差（水位永远差一点，比率永远
/// 差一截），而"稳态比率残差 <20 ppm"只有积分项能做到。为什么积分时间常数
/// 必须 ≥5 s：积分项看到的是同一个带网络抖动的水位，时间常数太小会把抖动
/// 积成变调（规范 §六 风险 2）。
///
/// 计时用**声卡已消费帧数**而不是媒体样本数：断流期间媒体样本不前进，墙钟
/// 却在走，用媒体样本计时会让积分项在断流时"冻结"，恢复后一次性补偿过头。
pub struct DriftController {
    device_rate: u32,
    /// 目标总水位（ms）：默认 [`WATER_TARGET_MS`]，可被环境变量/现场调参覆盖
    target_ms: u32,
    /// 低通后的总水位（ms），只用于 PI 输入
    water: f64,
    /// 当前比率微调量
    adjust: f64,
    /// 积分项
    integral: f64,
    primed: bool,
    last_device: u64,
    /// 评估次数（诊断）
    pub ratio_updates: u64,
}

impl DriftController {
    pub fn new(device_rate: u32) -> Self {
        Self::with_target(device_rate, water_target_ms())
    }

    /// 显式指定目标水位（单测与现场重定值用）。
    pub fn with_target(device_rate: u32, target_ms: u32) -> Self {
        Self {
            device_rate: device_rate.max(1),
            target_ms: target_ms.clamp(WATER_TARGET_MIN_MS, WATER_TARGET_MAX_MS),
            water: 0.0,
            adjust: 0.0,
            integral: 0.0,
            primed: false,
            last_device: 0,
            ratio_updates: 0,
        }
    }

    /// 当前目标总水位（ms）
    pub fn target_ms(&self) -> u32 {
        self.target_ms
    }

    pub fn reset(&mut self) {
        self.water = 0.0;
        self.adjust = 0.0;
        self.integral = 0.0;
        self.primed = false;
        self.last_device = 0;
    }

    /// 当前比率微调量（供诊断/日志读取）
    pub fn adjust(&self) -> f64 {
        self.adjust
    }

    /// 低通后的水位（诊断：与瞬时水位对比可以看出控制器有多"钝"）
    pub fn water(&self) -> f64 {
        self.water
    }

    /// 积分项（诊断：长期顶满说明比率钳位不够用）
    pub fn integral(&self) -> f64 {
        self.integral
    }

    /// `water_ms` 为**单一口径总水位**（抖动缓冲 + PCM 环 + 声卡缓冲），
    /// `device_frames` 为声卡已消费的设备帧数（真实时间）。
    ///
    /// 返回新的比率微调量：水位偏高 → 返回正数 → 每秒产出的设备帧变少。
    pub fn update(&mut self, water_ms: u32, device_frames: u64) -> f64 {
        let dframes = device_frames.saturating_sub(self.last_device);
        if dframes == 0 {
            return self.adjust;
        }
        let dt_ms = dframes as f64 * 1000.0 / self.device_rate as f64;
        if dt_ms < PI_MIN_UPDATE_MS {
            // 太密的调用只累计时间，不评估：低通与积分都按真实 dt 缩放
            return self.adjust;
        }
        self.last_device = device_frames;

        if !self.primed {
            self.primed = true;
            self.water = water_ms as f64;
            return self.adjust;
        }

        // ── 水面低通（τ≈150 ms，带宽≈1 Hz：把 10 ms 的帧量化噪声滤掉）──
        let steps = (dt_ms / FRAME_MS as f64).max(1e-3);
        let alpha = 1.0 - (1.0 - WATER_SMOOTH_ALPHA).powf(steps);
        self.water += alpha * (water_ms as f64 - self.water);

        let err = self.water - self.target_ms as f64;

        // ── 比例项 ──
        let p = (err * PI_KP_PER_MS).clamp(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST);

        // ── 积分项（抗饱和：输出顶满且误差还在往同方向顶时冻结）──
        let ki = PI_KP_PER_MS / PI_TI_S;
        let i_next = (self.integral + ki * err * (dt_ms / 1000.0))
            .clamp(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST);
        let want = p + i_next;
        let saturated = want.abs() >= RESAMPLE_MAX_ADJUST;
        if !saturated || err * i_next < 0.0 {
            self.integral = i_next;
        }

        // ── 斜率限制（1%/s，按真实 dt 缩放）：不让比例项跳变成可闻的变调 ──
        let target = want.clamp(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST);
        let slew = RATIO_SLEW_STEP * steps.max(1.0);
        let step = (target - self.adjust).clamp(-slew, slew);
        self.adjust = (self.adjust + step).clamp(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST);
        self.ratio_updates += 1;
        self.adjust
    }
}

impl Default for DriftController {
    fn default() -> Self {
        Self::new(OPUS_SAMPLE_RATE)
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
        assert_eq!(jb.pull(PLC_LEAD_MS + 10, PLC_LEAD_MS + 10, &mut [0u8; 1275]), Pull::Wait);
        // 解码缓冲只剩 20 ms（1920 - 20 ms）→ 缺口到期，PLC
        assert_eq!(jb.pull(PLC_LEAD_MS, PLC_LEAD_MS, &mut [0u8; 1275]), Pull::Plc { ts: 1920 });
        // 下一帧是真实包
        assert_eq!(jb.pull(PLC_LEAD_MS, PLC_LEAD_MS, &mut [0u8; 1275]), Pull::Packet { len: 1, ts: 2400 });
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
        assert_eq!(pull_tags(&mut jb, 0, 2), vec![2, 3]);

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
        assert_eq!(jb.pull(0, 0, &mut [0u8; 1275]), Pull::Wait);
        assert!(jb.epoch > 0);
    }

    // ── 漂移校正（抗饱和 PI）──

    /// 按 10 ms 一帧推进 `frames` 帧（声卡消费帧数同步前进），返回最终的比率微调
    fn run_pi(d: &mut DriftController, water: u32, frames: u64, dev: &mut u64) -> f64 {
        for _ in 0..frames {
            *dev += 480;
            d.update(water, *dev);
        }
        d.adjust()
    }

    #[test]
    fn drift_on_target_does_nothing() {
        // 水位稳定在目标上：比率一动不动
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(WATER_TARGET_MS, dev);
        let a = run_pi(&mut d, WATER_TARGET_MS, 1000, &mut dev);
        assert_eq!(a, 0.0);
        assert_eq!(d.integral(), 0.0);
    }

    #[test]
    fn drift_ratio_has_expected_sign_and_stays_bounded() {
        // 水位高出目标 40 ms：比率向上顶，但绝不越过 ±0.5%
        let high = WATER_TARGET_MS + 40;
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(high, dev);
        // KP 只有 50 ppm/ms，40 ms 偏差 → 2000 ppm；要顶到 0.5% 的钳位
        // 必须等积分项长起来（Ti = 10 s，约 15 s 到位）
        for _ in 0..4000 {
            dev += 480;
            let u = d.update(high, dev);
            assert!(u.abs() <= RESAMPLE_MAX_ADJUST + 1e-12, "比率越界: {u}");
            assert!(u >= -1e-12, "水位偏高时 adjust 不得为负: {u}");
        }
        assert!(d.adjust() > RESAMPLE_MAX_ADJUST * 0.99, "高水位应把比率顶到上限");
        // 反方向对称
        let low = WATER_TARGET_MS - 60;
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(low, dev);
        let a = run_pi(&mut d, low, 4000, &mut dev);
        assert!(a < -RESAMPLE_MAX_ADJUST * 0.99, "缺水应把比率顶到下限: {a}");
    }

    #[test]
    fn drift_ratio_slew_is_gradual() {
        // 单次比率更新步进受限：不允许一帧之内跳到满幅（那是可闻的变调台阶）
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(300, dev);
        let mut prev = 0.0f64;
        for _ in 0..60 {
            dev += 480;
            let u = d.update(300, dev);
            assert!((u - prev).abs() <= RATIO_SLEW_STEP + 1e-12, "步进过大: {prev} → {u}");
            prev = u;
        }
        assert!(prev > 0.0);
    }

    /// 闭环仿真一帧：水位是"流入 10 ms − 流出 10 ms×(1+adjust)"的积分。
    /// 这就是真实系统里的抖动缓冲水位：比率调大 → 每帧产出变少 → 水位回落。
    fn plant_step(water: f64, adjust: f64) -> f64 {
        (water - 10.0 * adjust).clamp(0.0, 640.0)
    }

    #[test]
    fn drift_closed_loop_settles_on_target_without_offset() {
        // PI 与纯比例的根本区别：纯比例必然留静差（水位永远差一点），
        // 这里要求稳态偏差 → 0，且稳态比率 → 0（不再有任何变调）。
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        let mut water = 20.0f64; // 起步水位远低于目标
        d.update(water as u32, dev);
        let mut tail = Vec::new();
        for i in 0..40_000u64 {
            dev += 480;
            let a = d.update(water as u32, dev);
            water = plant_step(water, a);
            if i >= 35_000 {
                tail.push(water);
            }
        }
        let mean = tail.iter().sum::<f64>() / tail.len() as f64;
        assert!(
            (mean - WATER_TARGET_MS as f64).abs() < 2.0,
            "稳态水位必须收敛到目标: {mean}"
        );
        assert!(d.adjust().abs() < 3e-4, "稳态比率必须收敛到零: {}", d.adjust());
    }

    #[test]
    fn drift_closed_loop_tracks_a_clock_skew() {
        // 闭环里出现 300 ppm 的持续速率差（前馈没跟上的部分）：PI 必须靠积分项
        // 把比率推到 −300 ppm 附近，让水位稳定在目标上（无静差）。
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        let mut water = WATER_TARGET_MS as f64;
        d.update(water as u32, dev);
        let skew = 300e-6;
        for _ in 0..12_000u64 {
            dev += 480;
            let a = d.update(water as u32, dev);
            // 发送端比本机快 skew → 每 10 ms 多进来 10×skew 的媒体
            water = plant_step(water + 10.0 * skew, a);
        }
        assert!(
            (water - WATER_TARGET_MS as f64).abs() < 2.0,
            "有速率差时水位仍需钉在目标: {water}"
        );
        let total = d.adjust();
        // 发送端快 → 缓冲净增 → 必须播得更快（正的比率微调）才能把水位推回去
        assert!(
            (total - skew).abs() < 60e-6,
            "比率必须收敛到 +skew（残余 {:.0} ppm）",
            (total - skew) * 1e6
        );
    }

    #[test]
    fn drift_integral_is_anti_windup() {
        // 长期饱和不得让积分项越界蓄能，否则退饱和时会长时间反向过冲
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(300, dev);
        run_pi(&mut d, 300, 30_000, &mut dev); // 300 s 极端高水位
        assert!(
            d.integral().abs() <= RESAMPLE_MAX_ADJUST + 1e-12,
            "积分项溢出: {}",
            d.integral()
        );
        assert!(d.adjust() <= RESAMPLE_MAX_ADJUST + 1e-12);
    }

    #[test]
    fn drift_jitter_in_band_barely_modulates_ratio() {
        // 规范 §六 风险 2：比例项不得把网络抖动变成变调。
        // ±4 ms、周期 1 s 的水位抖动 → 比率调制必须远小于 500 ppm。
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(WATER_TARGET_MS, dev);
        let mut worst = 0.0f64;
        for i in 0..3000u64 {
            dev += 480;
            let w = if (i / 100) % 2 == 0 {
                WATER_TARGET_MS + 4
            } else {
                WATER_TARGET_MS - 4
            };
            worst = worst.max(d.update(w, dev).abs());
        }
        assert!(worst < 0.0005, "抖动被放大成变调: {worst}");
    }

    #[test]
    fn drift_ignores_repeated_updates_without_time_passing() {
        // 计时用声卡帧数：没有新的设备帧就不评估，避免忙等循环虚增积分
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(WATER_TARGET_MS, dev);
        run_pi(&mut d, 120, 100, &mut dev);
        let updates = d.ratio_updates;
        let adjust = d.adjust();
        for _ in 0..50 {
            d.update(1000, dev);
        }
        assert_eq!(d.ratio_updates, updates);
        assert_eq!(d.adjust(), adjust);
    }

    #[test]
    fn drift_works_at_44k1_device_rate() {
        // 计时按设备率折算：44.1 kHz 上 441 帧 = 10 ms
        let high = WATER_TARGET_MS + 60;
        let mut d = DriftController::new(44_100);
        let mut dev = 0u64;
        d.update(high, dev);
        for _ in 0..2000 {
            dev += 441;
            d.update(high, dev);
        }
        assert!(d.adjust() > 0.001, "44.1 kHz 上高水位也必须顶起比率: {}", d.adjust());
        assert!(d.ratio_updates > 1000);
    }

    // ── 速率前馈估计 ──

    /// 合成观测：发送端媒体时钟按 `skew` 快于本地时钟，到达时刻叠加
    /// `jitter_us` 的**单向**排队延迟（排队只会让包到得更晚）。
    fn feed_estimator(est: &mut RateEstimator, seconds: f64, skew: f64, jitter_us: f64) {
        let base = Instant::now();
        let step_us = 10_000.0; // 每 10 ms 一个包
        let n = (seconds * 100.0) as u64;
        let mut seed = 0x1234_5678u64;
        for i in 0..n {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let r = ((seed >> 33) as f64) / ((1u64 << 31) as f64); // [0,1)
            let t_us = (i as f64 + 1.0) * step_us;
            let media_samples = (t_us * 1e-6 * (1.0 + skew) * 48_000.0).round() as u64;
            let arrive_us = t_us + r * jitter_us;
            let at = base + std::time::Duration::from_micros(arrive_us as u64);
            est.observe(media_samples, at);
        }
    }

    #[test]
    fn rate_estimator_needs_a_window_before_publishing() {
        let mut est = RateEstimator::new();
        assert_eq!(est.feedforward(), None);
        feed_estimator(&mut est, 3.0, 0.0, 0.0);
        // 3 s 只有 3 个桶，不够 RATE_MIN_BUCKETS → 不出估计（宁可前馈置 0）
        assert_eq!(est.feedforward(), None);
        let mut est = RateEstimator::new();
        feed_estimator(&mut est, 10.0, 0.0, 0.0);
        assert!(est.feedforward().is_some(), "窗口攒够后必须出估计");
    }

    #[test]
    fn rate_estimator_detects_fast_sender() {
        // 发送端快 200 ppm：前馈估计必须是 +200 ppm（±15 ppm）
        let mut est = RateEstimator::new();
        feed_estimator(&mut est, 40.0, 200e-6, 0.0);
        let ppm = est.feedforward_ppm().expect("必须出估计");
        assert!((ppm - 200.0).abs() < 15.0, "估计偏差过大: {ppm} ppm");
        assert!(est.sigma_ppm() < 20.0, "σ 过大: {} ppm", est.sigma_ppm());
        assert!(est.used_buckets() >= RATE_MIN_BUCKETS);
    }

    #[test]
    fn rate_estimator_detects_slow_sender() {
        let mut est = RateEstimator::new();
        feed_estimator(&mut est, 40.0, -300e-6, 0.0);
        let ppm = est.feedforward_ppm().expect("必须出估计");
        assert!((ppm + 300.0).abs() < 15.0, "估计偏差过大: {ppm} ppm");
    }

    #[test]
    fn rate_estimator_rejects_queueing_bursts() {
        // 单向排队延迟（0–20 ms 随机）不得把估计带偏：最小延迟滤波要把它吃掉
        let mut est = RateEstimator::new();
        feed_estimator(&mut est, 40.0, 0.0, 20_000.0);
        let ppm = est.feedforward_ppm().expect("必须出估计");
        assert!(ppm.abs() < 30.0, "排队抖动污染了估计: {ppm} ppm");
    }

    #[test]
    fn rate_estimator_is_clamped() {
        let mut est = RateEstimator::new();
        feed_estimator(&mut est, 40.0, 0.10, 0.0); // 10% 的荒谬钟差
        let ppm = est.feedforward_ppm().expect("必须出估计");
        assert!(ppm <= RATE_FEEDFORWARD_MAX * 1e6 + 1.0, "前馈未钳位: {ppm} ppm");
    }

    #[test]
    fn rate_estimator_reset_restarts_the_window() {
        let mut est = RateEstimator::new();
        feed_estimator(&mut est, 20.0, 200e-6, 0.0);
        assert!(est.feedforward().is_some());
        est.reset();
        assert_eq!(est.feedforward(), None);
        assert_eq!(est.used_buckets(), 0);
        // 重同步换了时间线基准：旧窗口绝不能继续参与回归
        feed_estimator(&mut est, 2.0, 200e-6, 0.0);
        assert_eq!(est.feedforward(), None);
    }

    #[test]
    fn rate_estimator_discards_a_rebuilt_timeline() {
        // 实机：手机重连/断流补齐后 highest_ts 会突然跳一大段，而 epoch 未必变。
        // 旧窗口的 (t_local, media) 对不再可比，必须整窗作废，否则回归斜率变成垃圾
        // （实测出现过 −2000 ppm 的假前馈）。
        let mut est = RateEstimator::new();
        feed_estimator(&mut est, 20.0, 200e-6, 0.0);
        assert!(est.feedforward().is_some());
        let last = 0u64; // 时间线跳变：下一包比上一包晚了 10 s 的媒体
        let _ = last;
        let base = Instant::now();
        est.observe(20 * 48_000 + 10 * 48_000, base);
        assert_eq!(est.feedforward(), None, "跳变后旧窗口必须作废");
        assert_eq!(est.used_buckets(), 0);
        // 作废后从新基准重新积累，仍然能测出速率
        let ts0 = 30 * 48_000u64;
        for i in 0..2000u64 {
            let at = base + std::time::Duration::from_micros(i * 10_000 + 10_000);
            est.observe(ts0 + (i + 1) * 480, at);
        }
        assert!(est.feedforward().is_some(), "重建后应重新出估计");
    }

    #[test]
    fn rate_estimator_ignores_repeated_timestamp() {
        let mut est = RateEstimator::new();
        let base = Instant::now();
        est.observe(0, base);
        est.observe(480, base + std::time::Duration::from_millis(10));
        let obs = est.observations();
        est.observe(480, base + std::time::Duration::from_millis(12));
        assert_eq!(est.observations(), obs, "同一个时间戳只能记一次");
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
        assert_eq!(jb.pull(0, 0, &mut [0u8; 1275]), Pull::Wait, "2 帧不得起播");
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
        for _ in 0..50u64 {
            let _ = jb.pull(0, 0, &mut [0u8; 1275]);
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
        let tags = pull_tags(&mut jb, 0, 11);
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
        assert!(jb.stats.late <= 1, "不得把恢复包判成迟到: {}", jb.stats.late);
    }

    // ── 调度：单一水位口径 ──

    #[test]
    fn pull_paces_on_decoded_buffer() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        // 解码缓冲已达上限：帧就在手上也不生产（这就是"预解码节流"）
        assert_eq!(jb.pull(DECODE_LIMIT_MS, DECODE_LIMIT_MS, &mut [0u8; 1275]), Pull::Wait);
        assert_eq!(jb.pull(DECODE_LIMIT_MS + 40, DECODE_LIMIT_MS + 40, &mut [0u8; 1275]), Pull::Wait);
        // 掉到上限以下才生产
        assert_eq!(
            jb.pull(DECODE_LIMIT_MS - 10, DECODE_LIMIT_MS - 10, &mut [0u8; 1275]),
            Pull::Packet { len: 1, ts: 0 }
        );
    }

    #[test]
    fn scheduler_thresholds_are_ordered() {
        // 目标水位必需覆盖整个流水线，节流点必需在 PLC 提前量之上，
        // 否则"该生产"与"该补 PLC"会互相打架。
        assert!(PLC_LEAD_MS < DECODE_LIMIT_MS);
        assert!(DECODE_LIMIT_MS < WATER_TARGET_MS);
        assert!(JITTER_START_MS < WATER_TARGET_MS);
    }

    // ── 阶跃吸收（方案 C）──

    #[test]
    fn water_target_default_is_150_and_override_is_clamped() {
        assert_eq!(WATER_TARGET_MS, 150);
        assert!(WATER_TARGET_MIN_MS < WATER_TARGET_MS);
        assert!(WATER_TARGET_MS < WATER_TARGET_MAX_MS);
        // 环境变量缓存后的值必须落在允许区间内（未设置时等于常数）
        assert!((WATER_TARGET_MIN_MS..=WATER_TARGET_MAX_MS).contains(&water_target_ms()));
    }

    #[test]
    fn jb_skip_frames_advances_the_timeline() {
        let mut jb = JitterBuffer::new();
        for i in 0..12u16 {
            insert(&mut jb, 100 + i, 4800 + 480 * i as u32, i == 0, i as u8 + 1);
        }
        assert_eq!(jb.water_ms(), 120);
        assert_eq!(pull_tags(&mut jb, 0, 1), vec![1]);
        // 跳 4 帧：时间线前移 40 ms，不产生 PLC，不计入 epoch
        let epoch = jb.epoch;
        assert_eq!(jb.skip_frames(4), 4);
        assert_eq!(jb.water_ms(), 120 - 10 - 40);
        assert_eq!(jb.stats.step_trims, 4);
        assert_eq!(jb.stats.plc_frames, 0);
        assert_eq!(jb.epoch, epoch);
        // 跳过的帧确实不会被再抬出来：下一帧是第 6 个包的 tag
        assert_eq!(pull_tags(&mut jb, 0, 1), vec![6]);
    }

    #[test]
    fn jb_skip_frames_never_runs_past_arrived_media() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        // 先进 PLAYING（ring_ms >= DECODE_LIMIT_MS 让 pull 返回 Wait、不推进播放头）
        let mut buf = [0u8; OPUS_MAX_PAYLOAD];
        assert_eq!(jb.pull(DECODE_LIMIT_MS, 0, &mut buf), Pull::Wait);
        assert!(jb.is_playing());
        // 只到了 4 帧（40 ms），要求跳 12 帧：只能跳到最后一帧之前（最新那帧必须留着）
        assert_eq!(jb.highest_ts(), 480 * 3);
        let skipped = jb.skip_frames(12);
        assert_eq!(skipped, 3, "不得把播放头推进虚空");
        assert_eq!(jb.next_ts(), jb.highest_ts());
        assert_eq!(jb.water_ms(), FRAME_MS, "剩下的必须是最后那一帧");
        assert_eq!(jb.stats.step_trims, 3);
        assert_eq!(jb.stats.plc_frames, 0);
    }

    #[test]
    fn jb_skip_frames_needs_a_playing_buffer() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        insert(&mut jb, 1, 480, false, 2);
        // 未进入 PLAYING（不足 JITTER_START_MS）：水位还在爬升，不能当阶跃
        assert!(!jb.is_playing());
        assert_eq!(jb.skip_frames(1), 0);
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
        // 播放头推到第 5 帧（next_seq = 104, next_ts = 6720）
        assert_eq!(pull_tags(&mut jb, 0, 4).len(), 4);
        assert_eq!(jb.next_seq(), 104);
        // 序号 102 却带着未来的时间戳：按时间戳重新对齐，滞后量算得出来
        assert_eq!(insert(&mut jb, 102, 7680, false, 9), InsertResult::Resync);
        assert_eq!(jb.last_rebase_reason, "序号滞后 2 帧，按时间戳重新对齐");
        assert_eq!(jb.stats.seq_realign, 1);
        // 新会话不能带着上一次的旧原因
        jb.reset();
        assert!(jb.last_rebase_reason.is_empty());
    }

    #[test]
    fn step_absorber_ignores_normal_water_noise() {
        let mut a = StepAbsorber::new();
        // 稳态：水位在目标 ±2 帧内跳，总量远低于 target+EXCESS
        for w in [150u32, 160, 140, 155, 145, 150] {
            assert_eq!(a.decide(w, 44, true, 150), StepDecision::NONE);
        }
        assert_eq!(a.hits, 0);
    }

    #[test]
    fn step_absorber_trims_on_a_single_batch_growth() {
        let mut a = StepAbsorber::new();
        assert_eq!(a.decide(150, 44, true, 150), StepDecision::NONE);
        // 一次补齐 120 ms → 命中"增长 > 80 ms"；帧数按**总水位**算：
        // total = 270 + 44 = 314，excess = 164 → ceil(164/10) = 17，被上限截到 12
        let d = a.decide(270, 44, true, 150);
        assert!(!d.limited);
        assert_eq!(d.frames, PLC_MAX_MS / FRAME_MS);
        assert_eq!(a.hits, 1);
    }

    #[test]
    fn step_absorber_trims_when_water_exceeds_target_window() {
        let mut a = StepAbsorber::new();
        // 没有大增长，但总量超过 target + STEP_TRIM_EXCESS_MS
        assert_eq!(a.decide(190, 60, true, 150), StepDecision::NONE);
        let d = a.decide(210, 60, true, 150);
        assert_eq!(d.frames, 12);
    }

    #[test]
    fn step_absorber_respects_the_per_minute_limit() {
        let mut a = StepAbsorber::new();
        let mut trimmed = 0;
        let mut limited = 0;
        for _ in 0..(STEP_TRIM_MAX_PER_MIN + 2) {
            // 每轮先回到目标附近，再制造一次 120 ms 阶跃
            a.decide(150, 44, true, 150);
            let d = a.decide(270, 44, true, 150);
            if d.frames == 0 {
                continue;
            }
            if d.limited {
                limited += 1;
            } else {
                trimmed += 1;
            }
        }
        assert_eq!(trimmed, STEP_TRIM_MAX_PER_MIN as i32);
        assert!(limited >= 1, "超限必须只记录不裁剪");
    }

    #[test]
    fn step_absorber_only_acts_once_playing() {
        let mut a = StepAbsorber::new();
        // 预缓冲期水位从 0 爬升：不得当成阶跃
        let d = a.decide(210, 44, false, 150);
        assert_eq!(d, StepDecision::NONE);
        assert_eq!(a.hits, 0);
        // 同一水位在 PLAYING 下也不算"增长"，但总量 254 已过 target+100
        let d = a.decide(210, 44, true, 150);
        assert_eq!(d.frames, 11);
    }
}
