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
/// Byte 5:       [8bit bitrate_kbps] (0=auto, 仅 TYPE_DATA 有效)
/// Byte 6:       [8bit seq_num] (仅 TYPE_DATA 有效)
/// Byte 7-14:    [8 bytes device_id] (发送端唯一标识)
/// Byte 15+:     [payload]
///
/// 检测: byte[0] == 0x02 → v2 统一协议; byte[0] >= 0x80 → 旧 v1 (拒绝)

pub const HEADER_SIZE: usize = 15;
pub const MAX_PAYLOAD: usize = 1472;
pub const MAX_PACKET: usize = HEADER_SIZE + MAX_PAYLOAD;
pub const MAX_REORDER: usize = 8;
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

/// 检测是否为 v2 统一协议包
pub fn is_v2_packet(buf: &[u8]) -> bool {
    buf.len() >= 1 && buf[0] == PROTO_VERSION
}

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
pub fn generate_device_id() -> [u8; DEVICE_ID_SIZE] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut id = [0u8; DEVICE_ID_SIZE];
    for i in 0..DEVICE_ID_SIZE {
        let shift = (i * 8) % 128;
        id[i] = ((nanos >> shift) ^ (nanos.wrapping_shl(3 + i as u32) >> (i * 3))) as u8;
    }
    id[0] ^= 0xA5;
    id[4] ^= 0x5A;
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
}

impl ReorderBuffer {
    pub fn new() -> Self {
        const INIT: ReorderSlot = ReorderSlot {
            seq: 0, sample_rate: 0, len: 0,
            data: [0u8; MAX_PAYLOAD], valid: false,
        };
        Self { buf: [INIT; MAX_REORDER], write: 0, next_seq: 0 }
    }

    pub fn insert_and_drain<F>(&mut self, seq: u8, sample_rate: u8, payload: &[u8], mut f: F)
    where F: FnMut(u8, &[u8]),
    {
        let dist = seq.wrapping_sub(self.next_seq);
        if dist as usize >= MAX_REORDER {
            self.clear();
            self.next_seq = seq.wrapping_add(1);
            f(sample_rate, payload);
            return;
        }
        if seq == self.next_seq {
            f(sample_rate, payload);
            self.next_seq = self.next_seq.wrapping_add(1);
        } else {
            let len = payload.len().min(MAX_PAYLOAD);
            self.buf[self.write].seq = seq;
            self.buf[self.write].sample_rate = sample_rate;
            self.buf[self.write].len = len;
            self.buf[self.write].data[..len].copy_from_slice(&payload[..len]);
            self.buf[self.write].valid = true;
            self.write = (self.write + 1) % MAX_REORDER;
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
        assert!(is_v2_packet(&buf));
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
    fn test_reject_v1() {
        // Old v1 packets have byte 0 >= 0x80, should be rejected
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = 0x94; // old v1: ver=1, codec=1, sr=4
        buf[1] = 0;
        assert!(!is_v2_packet(&buf));
        assert!(decode_header(&buf).is_none());
    }

    #[test]
    fn test_reject_unknown_version() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = 0x03; // unknown version
        assert!(!is_v2_packet(&buf));
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
        rb.insert_and_drain(1, 1, &[2], |sr, data| out.push((sr, data.to_vec())));
        assert!(out.is_empty());
        rb.insert_and_drain(0, 1, &[1], |sr, data| out.push((sr, data.to_vec())));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], (1, vec![1]));
        assert_eq!(out[1], (1, vec![2]));
    }

    #[test]
    fn test_reorder_skip() {
        let mut rb = ReorderBuffer::new();
        let mut out: Vec<(u8, Vec<u8>)> = Vec::new();
        rb.insert_and_drain(8, 1, &[4], |sr, data| out.push((sr, data.to_vec())));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], (1, vec![4]));
    }
}
