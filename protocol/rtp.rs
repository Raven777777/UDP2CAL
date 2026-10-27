//! RTP 媒体承载（RTP version 2 / PT=111 / 48 kHz / 固定 10 ms）
//!
//! 本模块是 v3 媒体面的唯一真源，包含三部分：
//!
//! 1. **线格式**：[`RtpHeader`] / [`parse`] / [`write_packet`]，标准 12 字节 RTP 固定头，
//!    无 CSRC、无扩展、无填充，负载就是 Opus 包本身；
//! 2. **扩展序号/时间戳**：[`extend_seq`] / [`extend_ts`]，用有符号差值把 16/32 位
//!    回绕扩展成 64 位单调计数；
//! 3. **接收侧时钟模型**（ASRC）：三个时间参考必须分开——发送端媒体样本时间
//!    （RTP 时间戳）、接收端单调时钟（`Instant`）、接收端声卡实际消耗进度
//!    （平台层上报的 played/playback-head 帧数）。输出重采样真正需要的比率是
//!    "媒体 48 kHz / 声卡实际消耗率"：标称部分由 `source_rate / device_rate`
//!    给出，实际设备钟差由 [`DeviceRateEstimator`] 从 `(设备累计帧, QPC/单调时刻)`
//!    估计。RTP 到达时刻只用于抖动统计与延迟测量，**不进入 ASRC 比率**——
//!    否则 90 ms 级网络攒批会直接变成音高变化；旧的 [`RateEstimator`] 仅作为
//!    JNI 兼容/诊断保留，不在比率路径上。[`DriftController`] 只用**长时间低通后**
//!    的水位残差修相位。`JitterBuffer` 只负责按时间戳存放、给出媒体跨度与
//!    真实占用/连续可解码量；播放决策只看 PCM 提交期限，不看声卡缓冲。
//!
//! **播放决策与设备延迟的关系**：设备里已经排队的音频不会给解码器"再等
//! 一会儿"的余地——下一帧必须现在提交，否则回调只能输出静音。因此 PLC 的
//! 判据是 `ring_ms`（已解码、尚未提交的 PCM）见底，而不是 `ring + device`。

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
/// 起播的**硬下限**：至少 3 帧连续可解码音频。
///
/// 真正的起播水位是 [`JitterBuffer::set_start_prebuffer_ms`] 设置的目标
/// （默认与 [`WATER_TARGET_MS`] 相同）：先攒到目标再播，避免"30 ms 起播、
/// 再用 ±0.5% 的 ASRC 花 24 s 爬到 150 ms"这种隐性启动策略。
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

/// 单次 tick 内 `jb.water_ms()` 增长超过它 → 只作为诊断，不再单独触发裁剪。
///
/// 一次 90 ms 的网络攒批到货是正常现象：只要缓冲垫吸收了它，水位随后会被
/// 正常消费掉。把"单次增长"当成积压证据会误裁正常网络抖动。
pub const STEP_TRIM_GROWTH_MS: u32 = 80;
/// 总水位超过 `target + 它` 且**持续** [`STEP_TRIM_HOLD_MS`] 才判为积压
pub const STEP_TRIM_EXCESS_MS: u32 = 100;
/// 持续超额的确认时长：网络攒批通常 <200 ms，持续 1.5 s 才说明真的多压了音频
pub const STEP_TRIM_HOLD_MS: u32 = 1500;
/// 阶跃吸收的每分钟限额：超限后**只记录不裁剪**，避免参数过激时听成"跳音"
pub const STEP_TRIM_MAX_PER_MIN: u32 = 3;
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
/// 比例增益：水位偏差每 1 ms → 0.002% 比率（20 ppm）。
///
/// 速率差由前馈 + 设备时钟估计承担；反馈只收**长期**残差。比例项刻意做小，
/// 再叠加 [`WATER_SMOOTH_TAU_MS`] 的长低通，
/// 90 ms 的周期性网络攒批不会变成可闻的音高调制。
pub const PI_KP_PER_MS: f64 = 0.00002;
/// 积分时间常数（秒）：只吃晶振偏置，不追网络抖动
pub const PI_TI_S: f64 = 180.0;
/// 积分项权限（±0.1% = ±1000 ppm ≈ 1.7 音分）
pub const PI_INTEGRAL_MAX: f64 = 0.001;
/// 水位低通时间常数（ms）。旧值 150 ms 会把 1.2 s 周期的攒批直接放进 PI；
/// 4 s 让"成批到达"在控制环里衰减约 20 倍，只剩长期平均水位。
pub const WATER_SMOOTH_TAU_MS: f64 = 4000.0;

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
        Self {
            marker,
            payload_type: OPUS_PAYLOAD_TYPE,
            seq,
            timestamp,
            ssrc,
        }
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
    RtpHeader::new(marker, seq, timestamp, ssrc)
        .encode((&mut dest[..RTP_HEADER_SIZE]).try_into().ok()?);
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
/// [`JitterBuffer::pull_trim`]，因此可以在同一把锁里完成决定、在外面记录日志。
///
/// 判据：真实占槽水位 `occupancy + pipe` > `target + `[`STEP_TRIM_EXCESS_MS`]，并且
/// **连续保持** [`STEP_TRIM_HOLD_MS`]。单次水位增长只用于诊断，不再单独触发：
/// 一次 90 ms 的攒批到货如果被缓冲垫吸收，就不是积压。
///
/// 限额：每 60 s 最多裁剪 [`STEP_TRIM_MAX_PER_MIN`] 次；超限后仍返回建议帧数，
/// 但 `limited = true`，调用方必须**不裁剪只记录**。
///
/// 调用方还必须把“刚发生过 PLC/欠载”的缓冲排除在裁剪之外（`can_trim=false`）：
/// 欠载后的高水位多半是恢复回填，不是真的多压了音频。
pub struct StepAbsorber {
    last_water_ms: u32,
    /// 第一次 `decide` 只登记基准，不把启动爬升当成积压
    primed: bool,
    /// 连续超额开始时刻；跌破阈值/不可裁剪时清零
    over_since: Option<Instant>,
    window_start: Instant,
    trims_in_window: u32,
    /// 本窗口内的限额日志是否已经打过（节流用，见 [`StepAbsorber::take_limited_log`]）
    limited_logged: bool,
    /// 命中“持续超额”的次数（含被限额压制的，诊断）
    pub hits: u64,
}

impl StepAbsorber {
    pub fn new() -> Self {
        Self {
            last_water_ms: 0,
            primed: false,
            over_since: None,
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
        self.over_since = None;
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

    /// 决定本 tick 要不要裁剪（使用当前时刻，供生产代码调用）。
    pub fn decide(
        &mut self,
        jb_water_ms: u32,
        pipe_ms: u32,
        can_trim: bool,
        target_ms: u32,
    ) -> StepDecision {
        self.decide_at(jb_water_ms, pipe_ms, can_trim, target_ms, Instant::now())
    }

    /// 可注入时钟的版本：单元测试据此验证“持续 1.5 s 才裁剪”，不必真的 sleep。
    pub fn decide_at(
        &mut self,
        jb_water_ms: u32,
        pipe_ms: u32,
        can_trim: bool,
        target_ms: u32,
        now: Instant,
    ) -> StepDecision {
        self.last_water_ms = jb_water_ms;
        if !self.primed {
            self.primed = true;
            return StepDecision::NONE;
        }
        if !can_trim {
            self.over_since = None;
            return StepDecision::NONE;
        }
        let total = jb_water_ms.saturating_add(pipe_ms);
        let over = total > target_ms.saturating_add(STEP_TRIM_EXCESS_MS);
        if !over {
            self.over_since = None;
            return StepDecision::NONE;
        }
        let since = *self.over_since.get_or_insert(now);
        if now.saturating_duration_since(since) < Duration::from_millis(STEP_TRIM_HOLD_MS as u64) {
            return StepDecision::NONE;
        }
        self.hits += 1;
        if now.saturating_duration_since(self.window_start) >= Duration::from_secs(60) {
            self.window_start = now;
            self.trims_in_window = 0;
            self.limited_logged = false;
        }
        // 只跳到目标：向上取整到整帧，且不超过 PLC 上限（避免一次跳掉半秒）
        let excess = total.saturating_sub(target_ms);
        let frames = excess.div_ceil(FRAME_MS).min(PLC_MAX_MS / FRAME_MS);
        if frames == 0 {
            return StepDecision::NONE;
        }
        // 裁剪后必须重新积累一个完整的确认窗口，避免连续 tick 连跳
        self.over_since = None;
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

const EMPTY_BUCKET: RateBucket = RateBucket {
    epoch: u64::MAX,
    t: 0.0,
    min_u: f64::INFINITY,
};

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
            *b = RateBucket {
                epoch,
                t: (epoch as f64 + 0.5) * bucket_s,
                min_u: u,
            };
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

// ═══════════════════ 设备时钟速率估计（前馈） ═══════════════════

/// 把"设备帧进度 / QPC 时间"拟合成设备时钟相对标称采样率的频差。
///
/// 这是 Plan §3 的第三个时间参照：RTP 媒体时间、接收端 `Instant`、设备进度。
/// 旧 `RateEstimator` 用 RTP 时间戳对 `Instant`，得到的是"发送端媒体时钟相对
/// 本机单调钟"；但重采样器真正要补偿的是"发送端媒体时钟相对**声卡设备时钟**"。
/// 两者之差就是设备晶振误差，本估计器把它单独测出来，避免把网络到达抖动
/// 误当成频差。
///
/// 观测方式是每秒取一个 `(QPC 秒, 设备累计帧)` 点，对最近 [`RATE_BUCKETS`]
/// 个点做最小二乘。`feedforward()` 返回的是**比率修正量**：设备时钟快 →
/// 返回负值（重采样比率要变小，才能每秒产出更多设备帧）。
#[derive(Clone, Copy)]
struct DeviceObservation {
    t: f64,
    device: u64,
}

const EMPTY_DEVICE_OBS: DeviceObservation = DeviceObservation { t: 0.0, device: 0 };

pub struct DeviceRateEstimator {
    /// 设备标称采样率（播放回调实际使用的速率，不一定是 48 kHz）
    nominal_rate: f64,
    t0: Option<Instant>,
    dev0: u64,
    last_device: u64,
    last_epoch: u64,
    obs: [DeviceObservation; RATE_BUCKETS],
    pos: usize,
    used: usize,
    feedforward: Option<f64>,
    /// 拟合斜率相对值的不确定度（ppm，1σ）
    sigma_ppm: f64,
}

impl DeviceRateEstimator {
    pub fn new() -> Self {
        Self::with_rate(OPUS_SAMPLE_RATE)
    }

    /// 指定设备标称速率。Windows 输出设备可能是 44.1 kHz，此时不能拿 48 kHz
    /// 当基准，否则会把“设备本来就是 44.1k”误判成 −8.16% 的巨大钟差。
    pub fn with_rate(nominal_rate: u32) -> Self {
        Self {
            nominal_rate: nominal_rate.max(1) as f64,
            t0: None,
            dev0: 0,
            last_device: 0,
            last_epoch: 0,
            obs: [EMPTY_DEVICE_OBS; RATE_BUCKETS],
            pos: 0,
            used: 0,
            feedforward: None,
            sigma_ppm: 0.0,
        }
    }

    pub fn reset(&mut self) {
        let rate = self.nominal_rate as u32;
        *self = Self::with_rate(rate);
    }

    /// 当前前馈比率修正（None = 样本不足）。正值表示设备时钟偏慢。
    pub fn feedforward(&self) -> Option<f64> {
        self.feedforward
    }

    pub fn sigma_ppm(&self) -> f64 {
        self.sigma_ppm
    }

    pub fn buckets(&self) -> usize {
        self.used
    }

    /// `device_frames` 是平台报告的**累计已播放设备帧**（Windows：
    /// `AudioWriter::played_frames()`；Android：`AudioTrack` 播放头对应的累计帧）。
    pub fn observe(&mut self, device_frames: u64, now: Instant) {
        let Some(t0) = self.t0 else {
            self.t0 = Some(now);
            self.dev0 = device_frames;
            self.last_device = device_frames;
            return;
        };
        if device_frames < self.last_device {
            // 设备帧回退（AudioTrack flush / 音频路径重建）：旧窗口不可比，
            // 必须从新基准重算，而不是永远忽略后续样本。
            self.reset();
            self.observe(device_frames, now);
            return;
        }
        if device_frames == self.last_device {
            return;
        }
        // 设备进度跳变超过 2 s：说明音频路径被重建 / 长时间挂起，旧窗口不可比
        if device_frames - self.last_device > RATE_JUMP_MAX_SAMPLES {
            self.reset();
            self.observe(device_frames, now);
            return;
        }
        self.last_device = device_frames;

        let t = now.saturating_duration_since(t0).as_secs_f64();
        let bucket_secs = (RATE_WINDOW_S / RATE_BUCKETS as f64).max(0.5);
        let epoch = (t / bucket_secs) as u64;
        if epoch == self.last_epoch {
            return;
        }
        self.last_epoch = epoch;
        self.obs[self.pos] = DeviceObservation {
            t,
            device: device_frames - self.dev0,
        };
        self.pos = (self.pos + 1) % RATE_BUCKETS;
        self.used = (self.used + 1).min(RATE_BUCKETS);
        self.fit();
    }

    fn fit(&mut self) {
        let n = self.used;
        if n < RATE_MIN_BUCKETS {
            self.feedforward = None;
            return;
        }
        let mut sx = 0.0;
        let mut sy = 0.0;
        for o in self.obs[..n].iter() {
            sx += o.t;
            sy += o.device as f64;
        }
        let mx = sx / n as f64;
        let my = sy / n as f64;
        let mut sxx = 0.0;
        let mut sxy = 0.0;
        for o in self.obs[..n].iter() {
            let dx = o.t - mx;
            sxx += dx * dx;
            sxy += dx * (o.device as f64 - my);
        }
        if sxx <= 1e-9 {
            self.feedforward = None;
            return;
        }
        let slope = sxy / sxx;
        // device/s 相对设备标称速率的偏差；比率为"每个输出消耗多少输入"，故取负
        let ratio_err = slope / self.nominal_rate - 1.0;
        self.feedforward = Some((-ratio_err).clamp(-RATE_FEEDFORWARD_MAX, RATE_FEEDFORWARD_MAX));

        let mut sse = 0.0;
        for o in self.obs[..n].iter() {
            let pred = my + slope * (o.t - mx);
            let e = o.device as f64 - pred;
            sse += e * e;
        }
        let sigma_slope = if n > 2 {
            (sse / (n as f64 - 2.0) / sxx).sqrt()
        } else {
            0.0
        };
        self.sigma_ppm = if slope.abs() > 1e-9 {
            (sigma_slope / slope.abs() * 1e6).abs()
        } else {
            0.0
        };
    }
}

impl Default for DeviceRateEstimator {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════ 漂移校正（反馈 PI） ═══════════════════════

/// 漂移校正器：**只收长期残差的抗饱和 PI**。
///
/// 频差的主项（发送端媒体时钟 vs 声卡设备时钟）由
/// [`DeviceRateEstimator`] 前馈承担；本控制器只负责把水位低通后的慢偏差
/// 收干净。因此增益刻意很小、低通很长（[`WATER_SMOOTH_TAU_MS`] = 4 s），
/// 网络 90 ms 攒批不会变成可闻的音高调制。
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
    /// 返回反馈比率微调量：水位偏高 → 正数 → 重采样比率变大 → 每秒产出的
    /// 设备帧变少（见 `AudioWriter::set_ratio_adjust`）。
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

        // ── 水面低通（τ=4 s：只留长期平均，90 ms 攒批衰减约 20 倍）──
        let steps = (dt_ms / FRAME_MS as f64).max(1e-3);
        let alpha = 1.0 - (-dt_ms / WATER_SMOOTH_TAU_MS).exp();
        self.water += alpha * (water_ms as f64 - self.water);

        // 4 s 低通已经把攒批噪声压到 <1 ms，这里直接用低通后的误差；
        // 不加死区，否则稳态会停在 target ± deadband，永远收不干净。
        let diff = self.water - self.target_ms as f64;

        // ── 比例项 ──
        let p = (diff * PI_KP_PER_MS).clamp(-RESAMPLE_MAX_ADJUST, RESAMPLE_MAX_ADJUST);

        // ── 积分项（权限 ±PI_INTEGRAL_MAX，且抗饱和：输出顶满且误差还在
        //    往同方向顶时冻结）──
        let ki = PI_KP_PER_MS / PI_TI_S;
        let i_next =
            (self.integral + ki * diff * (dt_ms / 1000.0)).clamp(-PI_INTEGRAL_MAX, PI_INTEGRAL_MAX);
        let want = p + i_next;
        let saturated = want.abs() >= RESAMPLE_MAX_ADJUST;
        if !saturated || diff * i_next < 0.0 {
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
            [
                0x80, 0xEF, 0x12, 0x34, 0x00, 0x00, 0x04, 0xB0, 0xDE, 0xAD, 0xBE, 0xEF, 0xF8, 0xFF,
                0xFE
            ]
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
        assert_eq!(
            write_packet(&mut buf, false, 1, 0, 7, &payload),
            Some(RTP_MAX_PACKET)
        );
        assert_eq!(UDP_IP_MAX_PACKET, 1315);
        assert!(write_packet(&mut buf, false, 1, 0, 7, &vec![0u8; 1276]).is_none());
        assert!(write_packet(&mut buf, false, 1, 0, 7, &[]).is_none());
        // 缓冲区不足
        let mut small = vec![0u8; 20];
        assert!(write_packet(&mut small, false, 1, 0, 7, &[0u8; 9]).is_none());
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
    fn prebuffer_phase_winds_up_the_integral_to_the_negative_stop() {
        // 预缓冲阶段（抖动缓冲还没攒够、还没出声）水位必然低于目标。
        // 若照常喂 PI，积分会一路被压到负饱和，开播后只能按 `ki·diff·dt`
        // （≈3 ppm/s）慢慢退，稳态水位因此被顶在目标之上 10–30 ms 长达数分钟。
        // 现场日志指纹：开播瞬间 `反馈 -1100ppm`，之后 -457 → -294 → +21 → +65。
        // 因此调用方必须以“是否在出声”为门控（Windows 解码线程已如此）。
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(0, dev); // 起播瞬间：水位 0
                          // 120 s 的“水位远低于目标”（预缓冲 + 断流）：积分以 ≈12 ppm/s 往下积，
                          // 约 82 s 触到 -1000 ppm 的权限下限。
        for _ in 0..12_000 {
            dev += 480; // 10 ms
            d.update(40, dev);
        }
        assert!(
            d.integral() <= -PI_INTEGRAL_MAX + 1e-9,
            "低水位应把积分项压到负饱和: {}",
            d.integral()
        );
    }

    #[test]
    fn drift_ratio_has_expected_sign_and_stays_bounded() {
        // 水位高出目标 40 ms：比率向上顶，但绝不越过 ±0.5%
        let high = WATER_TARGET_MS + 40;
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(high, dev);
        // KP 只有 20 ppm/ms，40 ms 偏差 → 800 ppm；积分项权限 1000 ppm
        // （Ti = 180 s，约 300 s 长满），所以比率封顶在 +0.18% 附近。
        for _ in 0..30_000 {
            dev += 480;
            let u = d.update(high, dev);
            assert!(u.abs() <= RESAMPLE_MAX_ADJUST + 1e-12, "比率越界: {u}");
            assert!(u >= -1e-12, "水位偏高时 adjust 不得为负: {u}");
        }
        assert!(
            d.integral() >= PI_INTEGRAL_MAX - 1e-9,
            "高水位应把积分项推到权限上限: {}",
            d.integral()
        );
        assert!(
            d.adjust() >= PI_INTEGRAL_MAX * 0.99,
            "高水位应把比率推到积分权限之上: {}",
            d.adjust()
        );
        // 反方向对称
        let low = WATER_TARGET_MS - 60;
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        d.update(low, dev);
        let a = run_pi(&mut d, low, 30_000, &mut dev);
        assert!(
            a <= -PI_INTEGRAL_MAX * 0.99,
            "缺水应把比率推到积分权限之下: {a}"
        );
        assert!(a > -RESAMPLE_MAX_ADJUST, "不得越过钳位: {a}");
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
            assert!(
                (u - prev).abs() <= RATIO_SLEW_STEP + 1e-12,
                "步进过大: {prev} → {u}"
            );
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
        // 水位以整毫秒进入控制器（生产也是整毫秒），量化会留下 < 1 帧的
        // 极限环；这里只要求收敛到目标附近，不要求浮点仿真级的零误差。
        assert!(
            (mean - WATER_TARGET_MS as f64).abs() < 8.0,
            "稳态水位必须收敛到目标: {mean}"
        );
        assert!(
            d.adjust().abs() < 3e-4,
            "稳态比率必须收敛到零: {}",
            d.adjust()
        );
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
        // Ti = 90 s、KP = 20 ppm/ms → ζ≈0.67、ω_n≈0.015 rad/s，
        // 整定时间 ≈4/(ζ·ω_n) ≈ 400 s，所以这里跑 900 s。
        for _ in 0..90_000u64 {
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
            d.integral().abs() <= PI_INTEGRAL_MAX + 1e-12,
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
    fn drift_jitter_steps_do_not_become_a_pitch_cycle() {
        // 现场症状（v1.5.0 调参）：网络抖动每次都会敲响"水位 → 比率"环。
        // 水位是速率差的纯积分，再套一个积分就是双积分器：振铃周期几十秒，
        // 幅度顶到 ±0.5%（8.6 音分），听感就是"几十秒一次循环的变调"。
        //
        // 这里用固定的伪随机抖动台阶（每 5 s 一次、幅度 0–25 ms）喂闭环，
        // 要求任意 30 s 窗口内 adjust 的峰峰值 < 2000 ppm（≈3.5 音分）。
        // 实测：旧调参（KP=5e-5 / Ti=10 / 积分权限 ±0.5%）在同一序列上是
        // 7808 ppm ≈ 13.5 音分——正是现场"几十秒一次循环的变调"。
        let mut d = DriftController::new(48_000);
        let mut dev = 0u64;
        let mut water = WATER_TARGET_MS as f64;
        d.update(water as u32, dev);
        let mut seed = 0x1234_5678_9ABC_DEF0u64;
        let mut next_step_at = 50u64;
        let mut hist: Vec<f64> = Vec::new();
        for i in 0..60_000u64 {
            // 600 s
            if i == next_step_at {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let r = ((seed >> 33) as f64) / ((1u64 << 31) as f64); // 0..1
                let sign = if (seed >> 20) & 1 == 1 { 1.0 } else { -1.0 };
                water = (water + sign * 25.0 * r).clamp(0.0, 640.0);
                next_step_at = i + 500; // 5 s
            }
            dev += 480;
            let a = d.update(water as u32, dev);
            water = plant_step(water, a);
            hist.push(a);
        }
        // 后半段每 30 s 取一个窗口，看峰峰值
        let mut worst = 0.0f64;
        let mut start = 30_000usize;
        while start + 3000 <= hist.len() {
            let seg = &hist[start..start + 3000];
            let hi = seg.iter().copied().fold(f64::MIN, f64::max);
            let lo = seg.iter().copied().fold(f64::MAX, f64::min);
            worst = worst.max(hi - lo);
            start += 1000;
        }
        assert!(
            worst < 0.002,
            "抖动被放大成周期性变调（30 s 峰峰 {:.0} ppm ≈ {:.1} 音分）",
            worst * 1e6,
            worst * 1e6 / 578.0
        );
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
    fn device_rate_estimator_tracks_fast_and_slow_clock() {
        for (ppm, sign) in [(200.0, -1.0), (-300.0, 1.0)] {
            let mut est = DeviceRateEstimator::new();
            let base = Instant::now();
            let rate = 48000.0 * (1.0 + ppm / 1e6);
            // 第一个点只建立基准；随后每秒一个点，攒满 6 桶后发布。
            for sec in 0..=8u64 {
                let frames = (rate * sec as f64) as u64;
                est.observe(frames, base + Duration::from_secs(sec));
            }
            let ff = est.feedforward().expect("enough buckets");
            assert!(
                (ff - sign * ppm.abs() / 1e6).abs() < 20e-6,
                "ppm={ppm} ff={ff}"
            );
            assert!(est.buckets() >= RATE_MIN_BUCKETS);
        }
    }

    #[test]
    fn device_rate_estimator_uses_device_nominal_rate() {
        let mut est = DeviceRateEstimator::with_rate(44100);
        let base = Instant::now();
        for sec in 0..=8u64 {
            est.observe(44100 * sec, base + Duration::from_secs(sec));
        }
        let ff = est.feedforward().expect("enough buckets");
        assert!(
            ff.abs() < 20e-6,
            "44.1k nominal must not look like drift: {ff}"
        );
    }

    #[test]
    fn device_rate_estimator_resets_after_jump() {
        let mut est = DeviceRateEstimator::new();
        let base = Instant::now();
        for sec in 0..=8u64 {
            est.observe(48000 * sec, base + Duration::from_secs(sec));
        }
        assert!(est.feedforward().is_some());
        // 2 s 以上的跳变说明时间线重建：旧窗必须作废
        est.observe(
            48000 * 8 + RATE_JUMP_MAX_SAMPLES + 1,
            base + Duration::from_secs(9),
        );
        assert_eq!(est.buckets(), 0);
        assert!(est.feedforward().is_none());

        // 设备帧回退同样必须重建（AudioTrack flush / 音频路径重建）
        for sec in 0..=8u64 {
            est.observe(48000 * (10 + sec), base + Duration::from_secs(10 + sec));
        }
        assert!(est.feedforward().is_some());
        est.observe(0, base + Duration::from_secs(20));
        assert_eq!(est.buckets(), 0);
        assert!(est.feedforward().is_none());
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
        assert!(
            d.adjust() > 0.001,
            "44.1 kHz 上高水位也必须顶起比率: {}",
            d.adjust()
        );
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
        assert!(
            ppm <= RATE_FEEDFORWARD_MAX * 1e6 + 1.0,
            "前馈未钳位: {ppm} ppm"
        );
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

    // ── 调度：单一水位口径 ──

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
    fn scheduler_thresholds_are_ordered() {
        // 目标水位必需覆盖整个流水线，节流点必需在 PLC 提前量之上，
        // 否则"该生产"与"该补 PLC"会互相打架。
        // 常量比较在编译期求值：改错常量直接编译失败，不必等跑测试。
        const { assert!(PLC_LEAD_MS < DECODE_LIMIT_MS) };
        const { assert!(DECODE_LIMIT_MS < WATER_TARGET_MS) };
        const { assert!(JITTER_START_MS < WATER_TARGET_MS) };
    }

    // ── 阶跃吸收（方案 C）──

    #[test]
    fn water_target_default_is_150_and_override_is_clamped() {
        assert_eq!(WATER_TARGET_MS, 150);
        // 同上：编译期求值
        const { assert!(WATER_TARGET_MIN_MS < WATER_TARGET_MS) };
        const { assert!(WATER_TARGET_MS < WATER_TARGET_MAX_MS) };
        // 环境变量缓存后的值必须落在允许区间内（未设置时等于常数）
        assert!((WATER_TARGET_MIN_MS..=WATER_TARGET_MAX_MS).contains(&water_target_ms()));
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

    #[test]
    fn step_absorber_ignores_normal_water_noise() {
        let mut a = StepAbsorber::new();
        let t0 = Instant::now();
        // 稳态：水位在目标 ±2 帧内跳，总量远低于 target+EXCESS
        for (i, w) in [150u32, 160, 140, 155, 145, 150].into_iter().enumerate() {
            let d = a.decide_at(w, 44, true, 150, t0 + Duration::from_secs(i as u64));
            assert_eq!(d, StepDecision::NONE);
        }
        assert_eq!(a.hits, 0);
    }

    #[test]
    fn step_absorber_waits_for_sustained_excess() {
        let mut a = StepAbsorber::new();
        let t0 = Instant::now();
        assert_eq!(a.decide_at(150, 44, true, 150, t0), StepDecision::NONE);
        // 一次 120 ms 攒批到货：不能立刻裁，先确认它是否被消费掉
        assert_eq!(a.decide_at(270, 44, true, 150, t0), StepDecision::NONE);
        assert_eq!(
            a.decide_at(270, 44, true, 150, t0 + Duration::from_millis(1000)),
            StepDecision::NONE
        );
        // 持续 1.5 s 仍是 high → 说明真的多压了音频
        let d = a.decide_at(270, 44, true, 150, t0 + Duration::from_millis(1600));
        assert!(!d.limited);
        assert_eq!(d.frames, PLC_MAX_MS / FRAME_MS);
        assert_eq!(a.hits, 1);
    }

    #[test]
    fn step_absorber_clears_timer_when_water_falls_back() {
        let mut a = StepAbsorber::new();
        let t0 = Instant::now();
        a.decide_at(150, 44, true, 150, t0);
        a.decide_at(270, 44, true, 150, t0);
        // 攒批被缓冲吸收，总水位回到阈值内：计时清零
        a.decide_at(160, 44, true, 150, t0 + Duration::from_millis(1000));
        let d = a.decide_at(270, 44, true, 150, t0 + Duration::from_millis(1600));
        assert_eq!(d, StepDecision::NONE, "必须重新积累完整确认窗口");
    }

    #[test]
    fn step_absorber_trims_when_water_exceeds_target_window() {
        let mut a = StepAbsorber::new();
        let t0 = Instant::now();
        a.decide_at(150, 44, true, 150, t0);
        assert_eq!(
            a.decide_at(190, 60, true, 150, t0 + Duration::from_millis(1600)),
            StepDecision::NONE
        );
        a.decide_at(210, 60, true, 150, t0 + Duration::from_millis(1700));
        let d = a.decide_at(210, 60, true, 150, t0 + Duration::from_millis(3300));
        assert_eq!(d.frames, 12);
    }

    #[test]
    fn step_absorber_respects_the_per_minute_limit() {
        let mut a = StepAbsorber::new();
        let t0 = Instant::now();
        let mut trimmed = 0;
        let mut limited = 0;
        for i in 0..(STEP_TRIM_MAX_PER_MIN + 2) {
            let base = t0 + Duration::from_millis(2000 + i as u64 * 2000);
            a.decide_at(150, 44, true, 150, base);
            a.decide_at(270, 44, true, 150, base);
            let d = a.decide_at(270, 44, true, 150, base + Duration::from_millis(1600));
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
        let t0 = Instant::now();
        // 预缓冲期水位从 0 爬升：不得当成积压
        assert_eq!(a.decide_at(210, 44, false, 150, t0), StepDecision::NONE);
        assert_eq!(a.hits, 0);
        // 进入 PLAYING 后也必须连续超额 1.5 s
        assert_eq!(
            a.decide_at(210, 44, true, 150, t0 + Duration::from_millis(1600)),
            StepDecision::NONE
        );
        let d = a.decide_at(210, 44, true, 150, t0 + Duration::from_millis(3200));
        assert_eq!(d.frames, 11);
    }
}
