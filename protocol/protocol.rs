/// UDP2CAL UDP私有协议编解码 (Rust实现)
///
/// ## v2 统一协议 (15字节包头 + 负载)
/// 音频数据包和控制消息共用同一包头格式，所有包携带设备 ID 实现 1对1 P2P 过滤。
///
/// Byte 0:       PROTO_VERSION = 0x02 (8bit协议版本标识)
/// Byte 1:       [1bit is_audio][3bit codec][4bit sample_rate]
///               is_audio=1 音频数据, =0 控制消息
///               codec=1 (Opus), sample_rate=0-4
/// Byte 2:       [8bit msg_type]
///               0=TYPE_DATA, 1=TYPE_CONNECT, 2=TYPE_DISCOVER_REQ,
///               3=TYPE_DISCOVER_REPLY, 4=TYPE_CONNECT_ACK
/// Byte 3-4:     [16bit payload_len BE]
/// Byte 5:       [8bit bitrate/2 kbps] (0=auto, 仅 TYPE_DATA 有效)
///               保留字段：发送端写入实际码率，接收端**不据此工作**
///               （码率以字节数实测为准），仅用于抓包时标注。
/// Byte 6:       [8bit seq_num] (仅 TYPE_DATA 有效)
/// Byte 7-14:    [8 bytes device_id] (发送端唯一标识)
/// Byte 15+:     [payload]
///
/// 协议只有单一版本：byte[0] 必须等于 PROTO_VERSION，其余一律拒绝。
/// 不存在版本协商、降级或旧版本兼容路径。

pub const HEADER_SIZE: usize = 15;
pub const MAX_PAYLOAD: usize = 1472;
pub const MAX_PACKET: usize = HEADER_SIZE + MAX_PAYLOAD;
/// 重排窗口：可容忍的"超前"包数（含 expected 自身）
pub const MAX_REORDER: usize = 2;
/// 迟到/重复判定窗口：序号落后 next_seq 不超过该值即视为迟到，直接丢弃
pub const MAX_LATE: u8 = 8;
pub const PROTO_VERSION: u8 = 2;
pub const DEVICE_ID_SIZE: usize = 8;
pub const DISCOVER_PORT: u16 = 44043;

// --- 消息类型 ---
pub const TYPE_DATA: u8 = 0;          // 音频数据
pub const TYPE_CONNECT: u8 = 1;       // 连接请求
pub const TYPE_DISCOVER_REQ: u8 = 2;  // 发现请求
pub const TYPE_DISCOVER_REPLY: u8 = 3; // 发现回复
pub const TYPE_CONNECT_ACK: u8 = 4;   // 连接确认

// --- 采样率常量 ---
pub const SAMPLE_RATE_8K: u8 = 0;
pub const SAMPLE_RATE_12K: u8 = 1;
pub const SAMPLE_RATE_16K: u8 = 2;
pub const SAMPLE_RATE_24K: u8 = 3;
pub const SAMPLE_RATE_48K: u8 = 4;

pub const BITRATE_AUTO: u8 = 0;

// ═══════════════════════ 包头结构 ═══════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub version: u8,         // PROTO_VERSION
    pub is_audio: bool,       // true=音频数据, false=控制消息
    pub codec: u8,           // 编码类型 (1=Opus)
    pub sample_rate: u8,     // 采样率 ID
    pub msg_type: u8,        // 消息类型
    pub payload_len: u16,    // 负载长度
    pub bitrate: u8,         // 码率 (kbps/2, 0=auto)
    pub seq_num: u8,         // 包序号 (仅TYPE_DATA)
    pub device_id: [u8; DEVICE_ID_SIZE], // 发送端设备ID
}

impl Default for PacketHeader {
    fn default() -> Self {
        Self {
            version: PROTO_VERSION,
            is_audio: false,
            codec: 1,
            sample_rate: SAMPLE_RATE_48K,
            msg_type: 0,
            payload_len: 0,
            bitrate: 0,
            seq_num: 0,
            device_id: [0u8; DEVICE_ID_SIZE],
        }
    }
}

// ═══════════════════════ 编解码 ═══════════════════════

/// 编码包头
pub fn encode_header(header: &PacketHeader, buf: &mut [u8; HEADER_SIZE]) {
    buf[0] = PROTO_VERSION;
    buf[1] = (if header.is_audio { 1u8 } else { 0 }) << 7
        | (header.codec & 0x07) << 4
        | (header.sample_rate & 0x0F);
    buf[2] = header.msg_type;
    buf[3] = (header.payload_len >> 8) as u8;
    buf[4] = header.payload_len as u8;
    buf[5] = header.bitrate;
    buf[6] = header.seq_num;
    buf[7..15].copy_from_slice(&header.device_id);
}

/// 解码包头
pub fn decode_header(buf: &[u8; HEADER_SIZE]) -> Option<PacketHeader> {
    if buf[0] != PROTO_VERSION {
        return None;
    }
    let is_audio = (buf[1] >> 7) & 0x01 == 1;
    let codec = (buf[1] >> 4) & 0x07;
    let sample_rate = buf[1] & 0x0F;
    if sample_rate > SAMPLE_RATE_48K {
        return None;
    }
    let msg_type = buf[2];
    if msg_type > TYPE_CONNECT_ACK {
        return None;
    }
    let payload_len = u16::from_be_bytes([buf[3], buf[4]]);
    if payload_len as usize > MAX_PAYLOAD {
        return None;
    }
    let bitrate = buf[5];
    let seq_num = buf[6];
    let mut device_id = [0u8; DEVICE_ID_SIZE];
    device_id.copy_from_slice(&buf[7..15]);
    Some(PacketHeader {
        version: PROTO_VERSION,
        is_audio,
        codec,
        sample_rate,
        msg_type,
        payload_len,
        bitrate,
        seq_num,
        device_id,
    })
}

/// 构建数据包（分配新 Vec）
pub fn build_packet(
    is_audio: bool, msg_type: u8, sample_rate: u8, seq_num: u8,
    device_id: &[u8; DEVICE_ID_SIZE], payload: &[u8], bitrate: u8,
) -> Vec<u8> {
    let len = payload.len().min(MAX_PAYLOAD);
    let mut packet = Vec::with_capacity(HEADER_SIZE + len);
    let mut hdr_buf = [0u8; HEADER_SIZE];
    let header = PacketHeader {
        is_audio,
        msg_type,
        sample_rate,
        seq_num,
        payload_len: len as u16,
        bitrate,
        device_id: *device_id,
        ..Default::default()
    };
    encode_header(&header, &mut hdr_buf);
    packet.extend_from_slice(&hdr_buf);
    packet.extend_from_slice(&payload[..len]);
    packet
}

/// 构建数据包（写入预分配缓冲区，零分配）
pub fn build_packet_to(
    dest: &mut [u8], offset: usize,
    is_audio: bool, msg_type: u8, sample_rate: u8, seq_num: u8,
    device_id: &[u8; DEVICE_ID_SIZE], payload: &[u8], bitrate: u8,
) -> Option<usize> {
    let len = payload.len().min(MAX_PAYLOAD);
    let total = HEADER_SIZE + len;
    if offset + total > dest.len() {
        return None;
    }
    dest[offset] = PROTO_VERSION;
    dest[offset + 1] = (if is_audio { 1u8 } else { 0 }) << 7
        | (1 & 0x07) << 4 | (sample_rate & 0x0F);
    dest[offset + 2] = msg_type;
    dest[offset + 3] = (len >> 8) as u8;
    dest[offset + 4] = len as u8;
    dest[offset + 5] = bitrate;
    dest[offset + 6] = seq_num;
    dest[offset + 7..offset + 15].copy_from_slice(device_id);
    dest[offset + HEADER_SIZE..offset + HEADER_SIZE + len].copy_from_slice(&payload[..len]);
    Some(total)
}

// ═══════════════════════ 工具函数 ═══════════════════════

pub fn sample_rate_to_hz(sr: u8) -> u32 {
    match sr {
        SAMPLE_RATE_8K => 8000,
        SAMPLE_RATE_12K => 12000,
        SAMPLE_RATE_16K => 16000,
        SAMPLE_RATE_24K => 24000,
        SAMPLE_RATE_48K => 48000,
        _ => 48000,
    }
}

pub fn hz_to_sample_rate(hz: u32) -> u8 {
    match hz {
        8000 => SAMPLE_RATE_8K,
        12000 => SAMPLE_RATE_12K,
        16000 => SAMPLE_RATE_16K,
        24000 => SAMPLE_RATE_24K,
        48000 => SAMPLE_RATE_48K,
        _ => SAMPLE_RATE_48K,
    }
}

pub fn compute_bitrate_id(kbps: u32) -> u8 {
    if kbps == 0 { BITRATE_AUTO } else { ((kbps / 2).min(255).max(1)) as u8 }
}

// ── 采样率 ↔ 码率的规范参考实现 ──
//
// 这三个函数是"协议规范的可执行定义"：两端都可以各自实现，但取值必须一致。
// 当前 Windows 端只用 hz/常量做校验（真实码率以字节数实测），Android 端在
// OpusEncoder.computeBitrateId 里有一份等价实现——修改任何一处都要同步另一处。

/// 自动码率对应的默认值（kbps）
pub fn default_bitrate_for_sr(sr: u8) -> u8 {
    match sr {
        SAMPLE_RATE_8K  => compute_bitrate_id(64),
        SAMPLE_RATE_12K => compute_bitrate_id(64),
        SAMPLE_RATE_16K => compute_bitrate_id(128),
        SAMPLE_RATE_24K => compute_bitrate_id(256),
        SAMPLE_RATE_48K => compute_bitrate_id(512),
        _ => compute_bitrate_id(64),
    }
}

pub fn resolve_bitrate(bitrate: u8, sample_rate: u8) -> u8 {
    if bitrate == 0 { default_bitrate_for_sr(sample_rate) } else { bitrate }
}

/// 生成 8 字节设备 ID
///
/// 设备 ID 是 P2P 独占唯一的准入凭据，因此不能只由时间戳推导：
/// 历史实现用 `SystemTime` 纳秒位运算，同网段主机可推算出一个窄的候选区间。
/// 这里混入 `RandomState`（运行期由 OS 熵初始化）与自增序号，把取值面扩大到
/// 进程级随机 + 时间 + 计数。
///
/// **注意**：这不构成密码学安全。协议本身没有鉴权（device_id 每秒随
/// DISCOVER_REPLY 明文广播），该机制只防误连、不防伪造。
pub fn generate_device_id() -> [u8; DEVICE_ID_SIZE] {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();

    let mut id = [0u8; DEVICE_ID_SIZE];
    for (i, byte) in id.iter_mut().enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(nanos);
        hasher.write_u64(seq);
        hasher.write_u32(pid);
        hasher.write_u8(i as u8);
        *byte = hasher.finish().to_le_bytes()[i % 8];
    }
    if id.iter().all(|&b| b == 0) {
        id[0] = 1; // 全零视为无效 ID
    }
    id
}

// ═══════════════════════ 重排缓冲区 ═══════════════════════

struct ReorderSlot {
    seq: u8,
    sample_rate: u8,
    len: usize,
    data: [u8; MAX_PAYLOAD],
    valid: bool,
}

pub struct ReorderBuffer {
    buf: [ReorderSlot; MAX_REORDER],
    write: usize,
    next_seq: u8,
    /// 连续被判定为"迟到"的包数；超阈值说明其实是一次大跳变，强制重同步
    late_streak: u8,
    /// 是否已收到过第一个包；未收到时首包无条件作为基准
    started: bool,
}

impl ReorderBuffer {
    pub fn new() -> Self {
        const INIT: ReorderSlot = ReorderSlot {
            seq: 0, sample_rate: 0, len: 0,
            data: [0u8; MAX_PAYLOAD], valid: false,
        };
        Self { buf: [INIT; MAX_REORDER], write: 0, next_seq: 0, late_streak: 0, started: false }
    }

    pub fn insert_and_drain<F>(&mut self, seq: u8, sample_rate: u8, payload: &[u8], mut f: F)
    where F: FnMut(u8, &[u8]),
    {
        // 首包作为基准，避免发送端从 seq=1 起步时首帧被误判为乱序
        if !self.started {
            self.started = true;
            self.clear();
            self.next_seq = seq.wrapping_add(1);
            f(sample_rate, payload);
            return;
        }

        // dist = seq 相对 next_seq 的"向前"距离（u8 回绕，天然支持序号翻转）
        let dist = seq.wrapping_sub(self.next_seq);

        // ── 迟到 / 重复包：dist 落在 [256-MAX_LATE, 255] ──
        // 必须与"向前跳变"区分开，否则一个重复包就会清空缓冲并把序号倒回去。
        if dist >= 0u8.wrapping_sub(MAX_LATE) {
            self.late_streak = self.late_streak.saturating_add(1);
            // 连续迟到过多 → 实际是发送端大跨度跳变（丢包/重启），强制重同步
            if self.late_streak < MAX_LATE.saturating_mul(4) {
                return;
            }
            self.late_streak = 0;
            self.clear();
            self.next_seq = seq.wrapping_add(1);
            f(sample_rate, payload);
            return;
        }
        self.late_streak = 0;

        if dist == 0 {
            f(sample_rate, payload);
            self.next_seq = self.next_seq.wrapping_add(1);
        } else if (dist as usize) < MAX_REORDER {
            // 窗口内乱序：先入槽，等 expected 到达后统一排空
            let len = payload.len().min(MAX_PAYLOAD);
            self.buf[self.write].seq = seq;
            self.buf[self.write].sample_rate = sample_rate;
            self.buf[self.write].len = len;
            self.buf[self.write].data[..len].copy_from_slice(&payload[..len]);
            self.buf[self.write].valid = true;
            self.write = (self.write + 1) % MAX_REORDER;
        } else {
            // 真正的跳变（丢包 / 发送端重启 / 长时间静默后恢复）：重置并立即输出
            self.clear();
            self.next_seq = seq.wrapping_add(1);
            f(sample_rate, payload);
            return;
        }

        loop {
            let mut found = false;
            for i in 0..MAX_REORDER {
                if self.buf[i].valid && self.buf[i].seq == self.next_seq {
                    f(self.buf[i].sample_rate, &self.buf[i].data[..self.buf[i].len]);
                    self.buf[i].valid = false;
                    self.next_seq = self.next_seq.wrapping_add(1);
                    found = true;
                    break;
                }
            }
            if !found { break; }
        }
    }

    fn clear(&mut self) {
        for slot in self.buf.iter_mut() { slot.valid = false; }
        self.write = 0;
    }

    pub fn reset(&mut self) {
        self.clear();
        self.next_seq = 0;
        self.late_streak = 0;
        self.started = false;
    }
}

impl Default for ReorderBuffer {
    fn default() -> Self { Self::new() }
}

// ═══════════════════════ 测试 ═══════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_audio_roundtrip() {
        let dev_id = generate_device_id();
        let header = PacketHeader {
            is_audio: true,
            msg_type: TYPE_DATA,
            sample_rate: SAMPLE_RATE_48K,
            seq_num: 42,
            payload_len: 320,
            bitrate: 32,
            device_id: dev_id,
            ..Default::default()
        };
        let mut buf = [0u8; HEADER_SIZE];
        encode_header(&header, &mut buf);
        let decoded = decode_header(&buf).unwrap();
        assert_eq!(decoded.is_audio, true);
        assert_eq!(decoded.msg_type, TYPE_DATA);
        assert_eq!(decoded.sample_rate, SAMPLE_RATE_48K);
        assert_eq!(decoded.seq_num, 42);
        assert_eq!(decoded.payload_len, 320);
        assert_eq!(decoded.bitrate, 32);
        assert_eq!(decoded.device_id, dev_id);
    }

    #[test]
    fn test_encode_decode_connect() {
        let dev_id = [0xAA; DEVICE_ID_SIZE];
        let header = PacketHeader {
            is_audio: false,
            msg_type: TYPE_CONNECT,
            device_id: dev_id,
            ..Default::default()
        };
        let mut buf = [0u8; HEADER_SIZE];
        encode_header(&header, &mut buf);
        let decoded = decode_header(&buf).unwrap();
        assert_eq!(decoded.is_audio, false);
        assert_eq!(decoded.msg_type, TYPE_CONNECT);
        assert_eq!(decoded.device_id, dev_id);
    }

    #[test]
    fn test_reject_unknown_version() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = 0x03; // unknown version
        assert!(decode_header(&buf).is_none());
    }

    #[test]
    fn test_reject_invalid_msg_type() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = PROTO_VERSION;
        buf[2] = 0xFF; // invalid msg type
        assert!(decode_header(&buf).is_none());
    }

    #[test]
    fn test_reject_invalid_sr() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = PROTO_VERSION;
        buf[2] = TYPE_DATA;
        buf[1] = 0x1F; // sr=15 (invalid)
        assert!(decode_header(&buf).is_none());
    }

    #[test]
    fn test_reject_oversized_payload() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = PROTO_VERSION;
        buf[2] = TYPE_DATA;
        buf[3] = 0x06;
        buf[4] = 0x00; // len=1536 > MAX_PAYLOAD
        assert!(decode_header(&buf).is_none());
    }

    #[test]
    fn test_build_audio_packet() {
        let dev_id = generate_device_id();
        let payload = vec![0xABu8; 100];
        let packet = build_packet(true, TYPE_DATA, SAMPLE_RATE_48K, 5, &dev_id, &payload, 32);
        assert_eq!(packet.len(), HEADER_SIZE + 100);
        let hdr: [u8; HEADER_SIZE] = packet[..HEADER_SIZE].try_into().unwrap();
        let decoded = decode_header(&hdr).unwrap();
        assert_eq!(decoded.msg_type, TYPE_DATA);
        assert_eq!(decoded.sample_rate, SAMPLE_RATE_48K);
        assert_eq!(decoded.seq_num, 5);
        assert_eq!(decoded.payload_len, 100);
        assert_eq!(decoded.bitrate, 32);
        assert_eq!(decoded.device_id, dev_id);
    }

    #[test]
    fn test_build_control_packet() {
        let dev_id = [0xBB; DEVICE_ID_SIZE];
        let packet = build_packet(false, TYPE_CONNECT, 0, 0, &dev_id, &[], 0);
        assert_eq!(packet.len(), HEADER_SIZE);
        let hdr: [u8; HEADER_SIZE] = packet[..HEADER_SIZE].try_into().unwrap();
        let decoded = decode_header(&hdr).unwrap();
        assert_eq!(decoded.msg_type, TYPE_CONNECT);
        assert_eq!(decoded.device_id, dev_id);
    }

    #[test]
    fn test_build_packet_to_buffer() {
        let dev_id = [0xCC; DEVICE_ID_SIZE];
        let payload = [0x01, 0x02, 0x03, 0x04];
        let mut buf = [0u8; 32];
        let written = build_packet_to(&mut buf, 2, false, TYPE_DISCOVER_REPLY, 0, 0, &dev_id, &payload, 0).unwrap();
        assert_eq!(written, HEADER_SIZE + 4);
        assert_eq!(buf[2], PROTO_VERSION);
        assert_eq!(buf[4], TYPE_DISCOVER_REPLY);
        assert_eq!(&buf[9..17], &dev_id);
        assert_eq!(&buf[17..21], &payload);
    }

    #[test]
    fn test_resolve_bitrate() {
        assert_eq!(resolve_bitrate(0, SAMPLE_RATE_48K), compute_bitrate_id(512));
        assert_eq!(resolve_bitrate(0, SAMPLE_RATE_24K), compute_bitrate_id(256));
        assert_eq!(resolve_bitrate(0, SAMPLE_RATE_16K), compute_bitrate_id(128));
        assert_eq!(resolve_bitrate(0, SAMPLE_RATE_12K), compute_bitrate_id(64));
        assert_eq!(resolve_bitrate(0, SAMPLE_RATE_8K),  compute_bitrate_id(64));
        assert_eq!(resolve_bitrate(64, SAMPLE_RATE_48K), 64);
    }

    #[test]
    fn test_compute_bitrate_id() {
        assert_eq!(compute_bitrate_id(0), 0);
        assert_eq!(compute_bitrate_id(64), 32);
        assert_eq!(compute_bitrate_id(128), 64);
        assert_eq!(compute_bitrate_id(256), 128);
        assert_eq!(compute_bitrate_id(512), 255);
    }

    #[test]
    fn test_generate_device_id_not_all_zero() {
        let id = generate_device_id();
        assert!(id.iter().any(|&b| b != 0));
        let id2 = generate_device_id();
        assert!(id != id2);
    }

    // --- 重排缓冲区测试 ---

    #[test]
    fn test_reorder_in_order() {
        let mut rb = ReorderBuffer::new();
        let mut out: Vec<(u8, Vec<u8>)> = Vec::new();
        rb.insert_and_drain(0, 1, &[1], |sr, data| out.push((sr, data.to_vec())));
        rb.insert_and_drain(1, 1, &[2], |sr, data| out.push((sr, data.to_vec())));
        rb.insert_and_drain(2, 1, &[3], |sr, data| out.push((sr, data.to_vec())));
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], (1, vec![1]));
        assert_eq!(out[1], (1, vec![2]));
        assert_eq!(out[2], (1, vec![3]));
    }

    #[test]
    fn test_reorder_out_of_order() {
        let mut rb = ReorderBuffer::new();
        let mut out: Vec<(u8, Vec<u8>)> = Vec::new();
        rb.insert_and_drain(0, 1, &[1], |sr, data| out.push((sr, data.to_vec())));
        rb.insert_and_drain(2, 1, &[3], |sr, data| out.push((sr, data.to_vec())));
        assert_eq!(out.len(), 1); // 2 入槽等待 1
        rb.insert_and_drain(1, 1, &[2], |sr, data| out.push((sr, data.to_vec())));
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], (1, vec![1]));
        assert_eq!(out[1], (1, vec![2]));
        assert_eq!(out[2], (1, vec![3]));
    }

    #[test]
    fn test_reorder_first_packet_is_baseline() {
        // 发送端从 seq=1 起步时，首包必须被立即接受（不能等一个永远不会来的 seq=0）
        let mut rb = ReorderBuffer::new();
        let mut out: Vec<u8> = Vec::new();
        rb.insert_and_drain(1, 1, &[1], |_sr, d| out.push(d[0]));
        rb.insert_and_drain(2, 1, &[2], |_sr, d| out.push(d[0]));
        assert_eq!(out, vec![1, 2]);
    }

    #[test]
    fn test_reorder_skip() {
        let mut rb = ReorderBuffer::new();
        let mut out: Vec<(u8, Vec<u8>)> = Vec::new();
        rb.insert_and_drain(8, 1, &[4], |sr, data| out.push((sr, data.to_vec())));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], (1, vec![4]));
    }

    #[test]
    fn test_reorder_duplicate_is_dropped() {
        let mut rb = ReorderBuffer::new();
        let mut out: Vec<u8> = Vec::new();
        for (seq, tag) in [(0u8, 0u8), (1, 1), (1, 9), (2, 2)] {
            rb.insert_and_drain(seq, 1, &[tag], |_sr, d| out.push(d[0]));
        }
        // 重复的 seq=1 必须被丢弃，只输出 0,1,2
        assert_eq!(out, vec![0, 1, 2]);
    }

    #[test]
    fn test_reorder_out_of_order_then_late() {
        let mut rb = ReorderBuffer::new();
        let mut out: Vec<u8> = Vec::new();
        rb.insert_and_drain(0, 1, &[0], |_sr, d| out.push(d[0]));
        rb.insert_and_drain(2, 1, &[2], |_sr, d| out.push(d[0])); // 入槽等待
        rb.insert_and_drain(1, 1, &[1], |_sr, d| out.push(d[0])); // 触发排空：1,2
        rb.insert_and_drain(0, 1, &[9], |_sr, d| out.push(d[0])); // 迟到包 → 丢弃
        rb.insert_and_drain(3, 1, &[3], |_sr, d| out.push(d[0]));
        assert_eq!(out, vec![0, 1, 2, 3]);
    }

    #[test]
    fn test_reorder_big_forward_jump_resyncs() {
        let mut rb = ReorderBuffer::new();
        let mut out: Vec<u8> = Vec::new();
        rb.insert_and_drain(0, 1, &[0], |_sr, d| out.push(d[0]));
        // 模拟长时间静默后恢复：序号大幅超前（200 包）
        rb.insert_and_drain(200, 1, &[200], |_sr, d| out.push(d[0]));
        rb.insert_and_drain(201, 1, &[201], |_sr, d| out.push(d[0]));
        assert_eq!(out, vec![0, 200, 201]);
    }

    #[test]
    fn test_reorder_late_streak_forces_resync() {
        let mut rb = ReorderBuffer::new();
        let mut out: Vec<u8> = Vec::new();
        rb.insert_and_drain(100, 1, &[100], |_sr, d| out.push(d[0]));
        rb.insert_and_drain(101, 1, &[101], |_sr, d| out.push(d[0]));
        // next_seq = 102，seq=94 属于"落后 8"，单次应被丢弃
        for _ in 0..31 {
            rb.insert_and_drain(94, 1, &[94], |_sr, d| out.push(d[0]));
        }
        assert_eq!(out, vec![100, 101]);
        // 连续迟到达到阈值 → 判定为发送端大跨度回退，强制重同步并输出
        rb.insert_and_drain(94, 1, &[94], |_sr, d| out.push(d[0]));
        assert_eq!(out, vec![100, 101, 94]);
    }
}
