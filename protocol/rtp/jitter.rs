//! 接收抖动缓冲：按时间戳存放、拉取与 PLC 决策。

use std::sync::OnceLock;
use std::time::Instant;

use super::header::{OPUS_MAX_PAYLOAD, RtpPacket, extend_seq, extend_ts};
use super::{FRAME_MS, FRAME_SAMPLES, samples_to_ms};

/// 重排槽位
pub const MAX_REORDER_PACKETS: usize = 64;

/// 起播的**硬下限**：至少 3 帧可解码音频（队首之后是终点已知的洞时，
/// 洞由 PLC 补齐，不阻塞起播）。
///
/// 真正的起播水位是 [`JitterBuffer::set_start_prebuffer_ms`] 设置的目标
/// （默认与 [`WATER_TARGET_MS`](super::WATER_TARGET_MS) 相同）：先攒到目标再播，
/// 避免"30 ms 起播、再用 ±0.5% 的 ASRC 花 34 s 爬到 200 ms"这种隐性启动策略。
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
/// 连续 PLC 上限（12 帧）：阶跃裁剪（[`JitterBuffer::pull_trim`]）的缺口上限。
///
/// 它仍是 [`StepAbsorber`](super::StepAbsorber) 单次裁剪帧数的上界（见
/// `step.rs`），Android 常量表也按 12 帧对照。**不要**拿它当中段洞的填充上限：
/// 中段洞有 [`PLC_HOLE_MAX_MS`]。
pub const PLC_MAX_MS: u32 = 120;
/// 中段洞 PLC 上限（30 帧 / 300 ms）。
///
/// 判据：`highest_ts > next_ts`，即缺帧之后**已经有真包入槽**——洞的终点已知，
/// PLC 只是按消费节拍把这段洞填满。旧上限 120 ms 会把 120–300 ms 的网络空档
/// 后半段切成静音，恢复时接缝反而更明显。
///
/// 只用于 [`JitterBuffer::pull`] 的缺口分支；`pull_trim`（阶跃裁剪）仍按
/// [`PLC_MAX_MS`]，因为裁剪每帧都要过解码器、单次跳段必须短。
pub const PLC_HOLE_MAX_MS: u32 = 300;
/// 尾部洞 hold 上限（默认 12 帧 / 120 ms）。
///
/// 判据：`highest_ts <= next_ts`，缺口之后**没有**真包——终点未知，接收端
/// 无法区分“包还在路上”与“发送端已停”。hold 期间按消费节拍返回 PLC 但
/// **不推进** `next_ts`：迟到的包在倒计时内到达 `ts == next_ts` 时，下一轮
/// pull 仍能按原时间戳播出，整段无损。倒计时耗尽才判阻塞。
pub const PLC_HOLD_MS: u32 = 120;
/// hold 时长的环境变量覆盖名（现场调参不改编译，风格同 `water_target_ms()`）。
pub const PLC_HOLD_ENV: &str = "UDP2CAL_PLC_HOLD_MS";
/// 覆盖下界：至少 1 帧。单帧尾丢不能直接硬切到零，否则接缝就是一次爆音。
pub const PLC_HOLD_MIN_MS: u32 = FRAME_MS;
/// 覆盖上界：再长就不是 concealment，而是用 PLC 掩盖断流。
pub const PLC_HOLD_MAX_MS: u32 = 600;

/// 运行时尾部洞 hold 时长：默认 [`PLC_HOLD_MS`]，可用 [`PLC_HOLD_ENV`] 覆盖。
///
/// 与 `water_target_ms()` 同口径：只读一次环境变量（`OnceLock` 缓存），
/// 非法值（非数字、空串）静默退回默认值，结果钳到
/// [`PLC_HOLD_MIN_MS`, `PLC_HOLD_MAX_MS`]。
pub fn plc_hold_ms() -> u32 {
    static CACHED: OnceLock<u32> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var(PLC_HOLD_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map(|v| v.clamp(PLC_HOLD_MIN_MS, PLC_HOLD_MAX_MS))
            .unwrap_or(PLC_HOLD_MS)
    })
}

/// 尾部洞 hold 的填充帧数（[`plc_hold_ms`] / 10 ms，至少 1 帧）。
pub fn plc_hold_frames() -> u32 {
    (plc_hold_ms() / FRAME_MS).max(1)
}
/// 触发重同步的时间戳跳变阈值
pub const RESYNC_GAP_MS: u32 = 200;
/// 连续多少个“帧对齐、非 marker、远超重排窗口”的包才判定时间线断裂。
///
/// 单个远跳可能是伪造（只丢弃）；连续多个更可能是 marker 丢失的合法断流，
/// 或接收端播放头因背压冻结。阈值 3 ≈ 30 ms，恢复延迟可忽略。
const FAR_JUMP_REBASE_AFTER: u32 = 3;

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
    /// PLC 已到上限 / 尾部洞 hold 超时：输出静音（该帧的播放头已越过）
    Silence { ts: u64 },
    /// 水位不足或还没到缺口：调用方继续等（不得用定时器"补"）
    Wait,
}

/// 尾部洞（终点未知）的 hold 状态。
///
/// 中段洞后面已有真包，终点已知，PLC 按帧推进 `next_ts` 即可；尾部洞后面
/// 什么都没有，接收端无法区分“包还在路上”与“发送端已停”。因此先倒计时喂
/// PLC 且**不推进** `next_ts`，把“这一帧还来不来得及”的裁决推迟到倒计时结束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoldState {
    /// 不在 hold：初始 / 真包已拉出 / 重同步复位
    Idle,
    /// 倒计时中，值为还可填充的帧数（每消费一帧 -1，不推进 `next_ts`）
    Active(u32),
    /// 倒计时耗尽：判定流已阻塞，之后每帧推进时间线并输出静音
    TimedOut,
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
    /// 其中尾部洞 hold 期的填充帧（不推进播放头；是 `plc_frames` 的子集）
    pub hold_frames: u64,
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
    /// 中段洞的连续 PLC 帧数（推进时间线的那一类；尾部洞 hold 不计入）
    plc_run: u32,
    /// 连续被丢弃的“M=0 且远超重排窗口”包数（见 [`FAR_JUMP_REBASE_AFTER`]）
    far_jumps: u32,
    /// 尾部洞 hold 状态（见 [`HoldState`]）
    hold: HoldState,
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
            far_jumps: 0,
            hold: HoldState::Idle,
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
    /// 比率花 34 s 爬到 200 ms”的隐性过程。
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

        // 迟到：播放头已越过该时间戳（时间戳是播放顺序的唯一权威）。
        // 必须先于非整帧重同步判定：否则旧时间线的重排迟到包余数与新时间线
        // 不同，会被误判成又一次时间线断裂，白白重置解码器并丢掉新缓冲。
        if ext_ts < self.next_ts {
            self.stats.late += 1;
            return InsertResult::Dropped;
        }

        // 发送端按墙钟补缺口/丢积压时，补出的时间戳未必是整帧（如缺 35 ms
        // 补 1680 样本）。播放头只按 ±480 前进，一旦错开非零余数就永远
        // 匹配不到任何包：PLC 到上限后无限静音、槽位堆满后每包记溢出。
        // 非整帧跳变必然是时间线断裂，直接按新时间线重同步。
        if ext_ts % FRAME_SAMPLES as u64 != self.next_ts % FRAME_SAMPLES as u64 {
            self.last_rebase_reason = format!(
                "时间戳非整帧跳变（偏移 {} 样本）",
                ext_ts.abs_diff(self.next_ts) % FRAME_SAMPLES as u64
            );
            self.rebase(ext_seq, ext_ts);
            self.store(pkt, ext_seq, ext_ts);
            self.next_seq = ext_seq + 1;
            self.stats.resyncs += 1;
            self.stats.inserted += 1;
            return InsertResult::Resync;
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

        // 非 marker 的超前跳跃超过一个重排窗口：发送端任何 >640 ms 的断点都会
        // 置 M=1，合法流不会走到这里。接纳它会把 `highest_ts`/水位钉在高位
        // （最长约 640 ms），伪造包随后还会被当成真实音频播一次。
        //
        // 但接收端播放头可能因背压冻结（ring 满 / 未起播）超过 640 ms，此时
        // 连续到达的 M=0 包也会全部越过窗口；只按单包丢弃会永久静音且 1 s
        // 空闲 reset 永不触发（丢包仍刷新最后收包时间）。因此连续 N 个远跳
        // 视为时间线断裂，按 M=1 的语义重同步。
        //
        // shortcut: 伪造者连发 N 个远跳包仍能触发重同步；非整帧远跳或 M=1
        // 也会直接触发（与现有重同步同级 DoS），单个 M=0 帧对齐包不会。
        // 要根治需要会话认证/包签名。
        if !pkt.marker
            && ext_ts.saturating_sub(self.next_ts)
                > MAX_REORDER_PACKETS as u64 * FRAME_SAMPLES as u64
        {
            self.far_jumps += 1;
            if self.far_jumps < FAR_JUMP_REBASE_AFTER {
                self.stats.overflow += 1;
                return InsertResult::Dropped;
            }
            self.last_rebase_reason =
                format!("M=0 连续远跳 {} 次，重同步", self.far_jumps);
            self.rebase(ext_seq, ext_ts);
            self.store(pkt, ext_seq, ext_ts);
            self.next_seq = ext_seq + 1;
            self.stats.resyncs += 1;
            self.stats.inserted += 1;
            return InsertResult::Resync;
        }
        // 时间线正常（包在窗口内）：连续远跳计数归零。
        self.far_jumps = 0;

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
    ///
    /// # 缺口分支的推进不变量
    ///
    /// * 中段洞（`highest_ts > next_ts`，终点已知）：PLC/静音都推进 `next_ts`，
    ///   PLC 上限 [`PLC_HOLE_MAX_MS`]。
    /// * 尾部洞（`highest_ts <= next_ts`，终点未知）：hold 期内返回 [`Pull::Plc`]
    ///   但**不推进** `next_ts`；倒计时耗尽才推进一帧并转 [`Pull::Silence`]。
    ///   因此消费侧不得假设“每收到一个 PLC 时间线就前进 10 ms”。
    pub fn pull(&mut self, ring_ms: u32, _pipe_ms: u32, out: &mut [u8]) -> Pull {
        if !self.started {
            return Pull::Wait;
        }
        if !self.playing {
            // 队首是空洞（槽里没有可播媒体）：尚未播出，跳到最小时间戳无损。
            // 否则基准包之后的第一个缺口会永远挡住 `contiguous_frames()`。
            if self.find_ts(self.next_ts).is_none()
                && self.highest_ts > self.next_ts
                && let Some(ts) = self.slots.iter().filter(|s| s.valid).map(|s| s.ts).min()
            {
                self.next_ts = ts;
            }
            let enough_frames = self.contiguous_frames() >= (JITTER_START_MS / FRAME_MS) as usize;
            let enough_water = self.water_ms() >= self.start_ms;
            // 队首有真包、后面是**终点已知**的中段洞时，洞可由 PLC 填满；
            // 若仍要求 3 连帧，丢一帧就会永久停在预缓冲（后续包只会撑满槽位）。
            let recoverable_hole = self.highest_ts > self.next_ts
                && self.occupancy_frames() >= (JITTER_START_MS / FRAME_MS) as usize;
            if (enough_frames || recoverable_hole) && enough_water {
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
            self.hold = HoldState::Idle;
            return Pull::Packet { len, ts };
        }

        // PCM 提交缓冲还有余量：缺口尚未到期，继续等。
        // 设备延迟（pipe_ms 里的 device 部分）不能作为“再等等”的理由。
        if ring_ms > PLC_LEAD_MS {
            return Pull::Wait;
        }

        // 提交余量已经见底：下一帧现在就要，否则声卡回调只能输出静音。
        let ts = self.next_ts;

        // 中段洞：缺帧之后已经有真包入槽（`highest_ts > next_ts`），终点已知。
        // 按消费节拍用 PLC 填满整段洞，时间线照常推进——恢复时接缝两侧都是
        // 连续媒体，不需要靠“提前切静音”表达断流。
        if self.highest_ts > self.next_ts {
            self.hold = HoldState::Idle;
            let max_run = PLC_HOLE_MAX_MS / FRAME_MS;
            self.advance();
            if self.plc_run < max_run {
                self.plc_run += 1;
                self.stats.plc_frames += 1;
                if self.plc_run > self.stats.max_plc_run {
                    self.stats.max_plc_run = self.plc_run;
                }
                return Pull::Plc { ts };
            }
            return Pull::Silence { ts };
        }

        // 尾部洞：终点未知。hold 期间返回 PLC 但**不推进** next_ts——每帧 PLC
        // 只对应一次消费，迟到的包在倒计时内到达 `ts == next_ts` 时，下一轮
        // pull 仍能按原时间戳播出，整段无损。倒计时耗尽才判阻塞：推进一帧、
        // 转静音（消费侧接 10 ms 淡出），之后的损失上界就是 H。
        if matches!(self.hold, HoldState::Idle) {
            self.hold = HoldState::Active(plc_hold_frames());
        }
        match self.hold {
            HoldState::Active(0) | HoldState::TimedOut => {
                self.hold = HoldState::TimedOut;
                self.advance();
                Pull::Silence { ts }
            }
            HoldState::Active(left) => {
                self.hold = HoldState::Active(left - 1);
                self.stats.plc_frames += 1;
                self.stats.hold_frames += 1;
                Pull::Plc { ts }
            }
            HoldState::Idle => unreachable!("hold 已在上面初始化"),
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
            self.hold = HoldState::Idle;
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
        self.far_jumps = 0;
        self.hold = HoldState::Idle;
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
        self.far_jumps = 0;
        self.hold = HoldState::Idle;
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
    /// 取的是 `entry.ts <= ts` 且时间戳最大的记录：帧可能乱序到达，按到达
    /// 顺序倒查会把后到的旧包当成这一帧（少报一个重排窗口的延迟）。查不到
    /// （例如刚重同步、记录已被覆盖）返回 `None`，调用方应跳过本次延迟估算。
    pub fn arrival_of(&self, ts: u64) -> Option<Instant> {
        let mut best: Option<(u64, Instant)> = None;
        for a in self.arrivals.iter() {
            let Some(at) = a.at else { continue };
            if a.ts <= ts && best.is_none_or(|(best_ts, _)| a.ts >= best_ts) {
                best = Some((a.ts, at));
            }
        }
        best.map(|(_, at)| at)
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
        // 一帧洞后面已有真包（终点已知）：必须按中段洞推进，不得被 hold 拖住
        assert_eq!(jb.next_ts(), 2400);
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
    fn jb_tail_hole_holds_then_silences_without_advancing_plc_run() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        for i in 1..4u16 {
            insert(&mut jb, i, 480 * i as u32, false, i as u8 + 1);
        }
        assert_eq!(pull_tags(&mut jb, 0, 4), vec![1, 2, 3, 4]);
        // 播完后长时间没有新包：尾部洞（终点未知）→ hold 倒计时
        let hold = plc_hold_frames();
        let held_ts = jb.next_ts();
        let mut plc = 0;
        let mut silence = 0;
        for _ in 0..(hold as u64 + 10) {
            match jb.pull(0, 0, &mut [0u8; 1275]) {
                Pull::Plc { ts } => {
                    assert_eq!(ts, held_ts, "hold 期间 PLC 必须对准播放头");
                    assert_eq!(jb.next_ts(), held_ts, "hold 期间不得推进播放头");
                    plc += 1;
                }
                Pull::Silence { .. } => silence += 1,
                _ => {}
            }
        }
        assert_eq!(plc, hold as usize);
        assert!(silence > 0);
        assert!(jb.is_playing());
        assert_eq!(jb.plc_run(), 0, "hold 不是普通连续 PLC");
        assert_eq!(jb.stats.hold_frames, hold as u64);
        assert_eq!(jb.stats.plc_frames, hold as u64);
    }

    #[test]
    fn jb_mid_hole_plc_fills_the_whole_hole() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        // 跳 4..34（30 帧），第 34 帧到达：洞的终点已知
        insert(&mut jb, 34, 480 * 34, false, 99);
        assert_eq!(pull_tags(&mut jb, 0, 4), vec![1, 2, 3, 4]);

        let mut out = [0u8; OPUS_MAX_PAYLOAD];
        let mut plc = 0;
        for expected_ts in (480u64 * 4..480 * 34).step_by(480) {
            assert_eq!(
                jb.pull(0, 0, &mut out),
                Pull::Plc { ts: expected_ts },
                "中段洞必须整段用 PLC 填满"
            );
            plc += 1;
        }
        assert_eq!(plc, (PLC_HOLE_MAX_MS / FRAME_MS) as usize);
        assert!(
            plc > (PLC_MAX_MS / FRAME_MS) as usize,
            "中段洞上限必须宽于 12 帧"
        );
        assert_eq!(
            jb.pull(0, 0, &mut out),
            Pull::Packet {
                len: 1,
                ts: 480 * 34
            }
        );
        assert_eq!(out[0], 99);
        assert_eq!(jb.plc_run(), 0, "真包拉出后归零");
        assert_eq!(jb.stats.hold_frames, 0, "终点已知不得走 hold");
    }

    #[test]
    fn jb_tail_hole_hold_takes_a_late_packet_losslessly() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        assert_eq!(pull_tags(&mut jb, 0, 4), vec![1, 2, 3, 4]);
        let held_ts = jb.next_ts();
        let mut out = [0u8; OPUS_MAX_PAYLOAD];

        assert_eq!(jb.pull(0, 0, &mut out), Pull::Plc { ts: held_ts });
        assert_eq!(jb.next_ts(), held_ts, "hold 尚未到期，播放头必须停住");

        // 迟到的第 4 帧在倒计时内到达：下一轮按原时间戳播出，内容无损
        assert_eq!(
            insert(&mut jb, 4, held_ts as u32, false, 42),
            InsertResult::Inserted
        );
        assert_eq!(
            jb.pull(0, 0, &mut out),
            Pull::Packet {
                len: 1,
                ts: held_ts
            }
        );
        assert_eq!(out[0], 42);
        assert_eq!(jb.plc_run(), 0);
        assert_eq!(jb.stats.hold_frames, 1);

        // 再次断流：hold 必须重新从满额开始（与 plc_run 同生命周期）
        let hold = plc_hold_frames();
        let mut plc = 0;
        for _ in 0..hold {
            match jb.pull(0, 0, &mut out) {
                Pull::Plc { .. } => plc += 1,
                other => panic!("尾部洞应持续 hold，得到 {other:?}"),
            }
        }
        assert_eq!(plc, hold);
        let timeout_ts = jb.next_ts();
        assert_eq!(jb.pull(0, 0, &mut out), Pull::Silence { ts: timeout_ts });
        assert_eq!(jb.next_ts(), timeout_ts + FRAME_SAMPLES as u64);
    }

    #[test]
    fn jb_tail_hold_timeout_advances_and_recovers_on_late_packet() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        assert_eq!(pull_tags(&mut jb, 0, 4), vec![1, 2, 3, 4]);
        let mut out = [0u8; OPUS_MAX_PAYLOAD];
        let hold = plc_hold_frames();
        let held_ts = jb.next_ts();
        for _ in 0..hold {
            assert!(matches!(jb.pull(0, 0, &mut out), Pull::Plc { .. }));
            assert_eq!(jb.next_ts(), held_ts);
        }
        // 倒计时耗尽：推进一帧、转静音
        assert_eq!(jb.pull(0, 0, &mut out), Pull::Silence { ts: held_ts });
        assert_eq!(jb.next_ts(), held_ts + FRAME_SAMPLES as u64);
        // 之后按帧推进；迟到的恢复包仍能按当前播放头播出
        let resume_ts = jb.next_ts();
        assert_eq!(
            insert(&mut jb, 4, resume_ts as u32, false, 7),
            InsertResult::Inserted
        );
        assert_eq!(
            jb.pull(0, 0, &mut out),
            Pull::Packet {
                len: 1,
                ts: resume_ts
            }
        );
        assert_eq!(out[0], 7);
    }

    #[test]
    fn jb_packet_ahead_ends_tail_hold_and_switches_to_mid_hole() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        assert_eq!(pull_tags(&mut jb, 0, 4), vec![1, 2, 3, 4]);
        let mut out = [0u8; OPUS_MAX_PAYLOAD];
        let held_ts = jb.next_ts();
        assert_eq!(jb.pull(0, 0, &mut out), Pull::Plc { ts: held_ts });
        assert_eq!(jb.next_ts(), held_ts, "hold 中播放头不动");

        // 缺包没到，但更远的包到了：洞的终点已知，必须改走中段洞推进
        insert(&mut jb, 6, 480 * 6, false, 7);
        assert_eq!(jb.pull(0, 0, &mut out), Pull::Plc { ts: held_ts });
        assert_eq!(jb.next_ts(), held_ts + FRAME_SAMPLES as u64);
        // 下一帧缺口（held_ts+480）同样终点已知
        assert_eq!(
            jb.pull(0, 0, &mut out),
            Pull::Plc {
                ts: held_ts + FRAME_SAMPLES as u64
            }
        );
        assert_eq!(
            jb.pull(0, 0, &mut out),
            Pull::Packet {
                len: 1,
                ts: 480 * 6
            }
        );
        assert_eq!(jb.stats.hold_frames, 1, "终点已知后不得继续 hold");
        assert_eq!(jb.plc_run(), 0);
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
    fn jb_unaligned_timestamp_jump_realigns_instead_of_stalling() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        // 播掉一帧进入 PLAYING（next_ts = 480，余数 0）
        assert_eq!(pull_tags(&mut jb, 0, 1), vec![1]);
        let epoch = jb.epoch;

        // 发送端补 35 ms 缺口 = 1680 样本（非 480 倍数）：不重同步就会永久错开半帧
        let jump_ts = 480 * 3 + 1680;
        assert_eq!(insert(&mut jb, 4, jump_ts, false, 9), InsertResult::Resync);
        assert!(jb.epoch > epoch);
        assert_eq!(jb.next_ts(), jump_ts as u64);
        assert!(!jb.is_playing(), "重同步后必须回到预缓冲");
        assert!(jb.last_rebase_reason.contains("非整帧"));

        // 新时间线整帧连续：必须能正常起播，而不是无限 PLC/静音
        for i in 1..=3u16 {
            insert(&mut jb, 4 + i, jump_ts + 480 * i as u32, false, 10 + i as u8);
        }
        assert_eq!(pull_tags(&mut jb, 0, 4), vec![9, 11, 12, 13]);
    }

    #[test]
    fn jb_late_packet_after_unaligned_resync_does_not_rebase_again() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        // 播掉一帧：旧时间线 next_ts = 480，余数 0
        assert_eq!(pull_tags(&mut jb, 0, 1), vec![1]);

        // 正向非整帧前跳：重同步到余数 240 的新时间线
        let jump_ts = 480 * 3 + 1680;
        assert_eq!(insert(&mut jb, 4, jump_ts, false, 9), InsertResult::Resync);
        let epoch = jb.epoch;
        let next_ts = jb.next_ts();
        let highest_ts = jb.highest_ts();
        let water = jb.water_ms();
        let occupied = jb.occupancy_frames();

        // 旧时间线的重排迟到包余数不同；不得触发第二次时间线重同步。
        let late_ts = 480 * 4 + 200;
        assert!(late_ts < next_ts as u32);
        assert_eq!(insert(&mut jb, 5, late_ts, false, 10), InsertResult::Dropped);

        assert_eq!(jb.epoch, epoch, "迟到包不得递增 epoch");
        assert_eq!(jb.next_ts(), next_ts);
        assert_eq!(jb.highest_ts(), highest_ts);
        assert_eq!(jb.water_ms(), water);
        assert_eq!(jb.occupancy_frames(), occupied);
        assert_eq!(jb.stats.resyncs, 1);
        assert_eq!(jb.stats.late, 1);
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
    fn jb_starts_despite_a_known_mid_hole_after_the_base_packet() {
        // 基准包之后丢 1 帧、后续全部到达：连续量永远停在 1。
        // 起播条件必须允许“终点已知的中段洞”，否则永久静音。
        let mut jb = JitterBuffer::new();
        assert_eq!(insert(&mut jb, 0, 0, true, 1), InsertResult::Started);
        assert_eq!(insert(&mut jb, 2, 960, false, 3), InsertResult::Inserted);
        for i in 3..20u16 {
            insert(&mut jb, i, 480 * i as u32, false, i as u8 + 1);
        }
        assert_eq!(pull_tags(&mut jb, 0, 3), vec![1, 0xFE, 3]);
        assert!(jb.is_playing(), "已知终点的中段洞不得阻塞起播");
        assert_eq!(pull_tags(&mut jb, 0, 5), vec![4, 5, 6, 7, 8]);
    }

    #[test]
    fn jb_skips_a_leading_hole_before_starting() {
        // 队首没有可播媒体时（重同步/复位后的罕见状态），跳到最小时间戳：
        // 尚未播出，不丢任何真实音频。
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        for i in 4..10u16 {
            insert(&mut jb, i, 480 * i as u32, false, i as u8 + 1);
        }
        // 手动模拟“队首槽位缺失”的边界状态：next_ts 指向空洞，槽里只有更晚的包
        let head = jb.find_ts(0).unwrap();
        jb.slots[head].valid = false;
        jb.next_ts = 480;
        assert_eq!(pull_tags(&mut jb, 0, 3), vec![5, 6, 7]);
        assert!(jb.is_playing());
    }

    #[test]
    fn jb_drops_unmarked_far_future_timestamp() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        let highest = jb.highest_ts();
        let water = jb.water_ms();
        // seq 在窗口内、时间戳 +10 s、M=0：伪造包不得拉高 highest_ts/水位
        assert_eq!(
            insert(&mut jb, 5, 480 * 3 + 480_000, false, 99),
            InsertResult::Dropped
        );
        assert_eq!(jb.stats.overflow, 1);
        assert_eq!(jb.highest_ts(), highest, "highest_ts 不得被伪造包拉高");
        assert_eq!(jb.water_ms(), water, "水位不得饱和到 640 ms");
        // 真实音频顺序播放，伪造包不会被当成音频
        assert_eq!(pull_tags(&mut jb, 0, 4), vec![1, 2, 3, 4]);
    }

    #[test]
    fn jb_recovers_from_consecutive_unmarked_far_jumps() {
        // 接收端播放头冻结（如 PCM 环满）超过 640 ms 后，发送端仍以 M=0、
        // 连续序号发新时间线：单包丢弃，连续第 3 个必须重同步自愈。
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        let base = 480 * 3 + 38_400; // next_ts + 800 ms
        // seq = next_seq + 1：断流期间还丢了一个包
        assert_eq!(insert(&mut jb, 5, base, false, 5), InsertResult::Dropped);
        assert_eq!(
            insert(&mut jb, 6, base + 480, false, 6),
            InsertResult::Dropped
        );
        assert_eq!(
            insert(&mut jb, 7, base + 960, false, 7),
            InsertResult::Resync,
            "连续远跳必须重同步，不能永久丢弃"
        );
        for i in 8..12u16 {
            insert(
                &mut jb,
                i,
                base + 480 * (i as u32 - 5),
                false,
                i as u8,
            );
        }
        // 新时间线按序起播，后续真包能播出
        assert_eq!(pull_tags(&mut jb, 0, 3), vec![7, 8, 9]);
        assert!(jb.is_playing());
    }

    #[test]
    fn jb_three_unmarked_far_jumps_rebase_instead_of_saturating_water() {
        let mut jb = JitterBuffer::new();
        for i in 0..4u16 {
            insert(&mut jb, i, 480 * i as u32, i == 0, i as u8 + 1);
        }
        let epoch = jb.epoch;
        let far = 480 * 3 + 480_000; // next_ts + 10 s
        assert_eq!(insert(&mut jb, 4, far, false, 5), InsertResult::Dropped);
        assert_eq!(
            insert(&mut jb, 5, far + 480, false, 6),
            InsertResult::Dropped
        );
        // 前两个伪造包不得拉高最高时间戳/水位
        assert_eq!(jb.highest_ts(), 480 * 3);
        assert_eq!(jb.water_ms(), 40);
        // 第三个触发重同步：缓冲被重置到新基准，水位不得饱和到 640 ms
        assert_eq!(
            insert(&mut jb, 6, far + 960, false, 7),
            InsertResult::Resync
        );
        assert!(jb.epoch > epoch);
        assert!(!jb.is_playing());
        assert_eq!(jb.next_ts(), far as u64 + 960);
        assert_eq!(jb.highest_ts(), far as u64 + 960);
        assert_eq!(jb.water_ms(), FRAME_MS, "重同步后水位必须归到单帧");
        assert_eq!(jb.stats.overflow, 2);
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
        // 断流 500 ms：尾部洞先 hold H 帧（不推进），耗尽后按静音推进；next_seq 不动
        let hold = plc_hold_frames() as u64;
        for _ in 0..50u64 {
            let _ = jb.pull(0, 0, &mut [0u8; 1275]);
        }
        assert_eq!(jb.next_seq(), 8, "PLC/静音不得发明发送端序号");
        let resume_ts = 480 * (8 + 50 - hold);
        assert_eq!(jb.next_ts(), resume_ts);
        let epoch = jb.epoch;

        // 恢复包序号看起来“倒退”（2 < 8），时间戳正好接上播放头
        let r = insert(&mut jb, 2, resume_ts as u32, false, 9);
        assert_eq!(r, InsertResult::Inserted, "序号滞后不得丢弃真实音频");
        assert_eq!(jb.stats.seq_realign, 1);
        assert_eq!(jb.stats.resyncs, 0, "不得整块清空缓冲");
        assert_eq!(jb.epoch, epoch);
        assert_eq!(jb.next_ts(), resume_ts);
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
    fn arrival_of_out_of_order_uses_the_matching_timestamp() {
        let mut jb = JitterBuffer::new();
        insert(&mut jb, 0, 0, true, 1);
        insert(&mut jb, 2, 960, false, 2); // 乱序先到
        let at_960 = jb.arrival_of(960).expect("960 已到达");
        insert(&mut jb, 1, 480, false, 3); // 旧包后到
        // 查询 960 不得返回后到的 480 的到达时刻（按到达顺序倒查会返回它）
        assert_eq!(jb.arrival_of(960), Some(at_960));
        assert!(jb.arrival_of(480).is_some());
    }

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
