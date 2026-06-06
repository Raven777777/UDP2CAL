/// UDP2Mic UDP私有协议编解码 (Rust实现)
/// v1: 固定6字节包头 (4字节v0包头 + 码率 + 扩展标记) + Opus负载
///
/// Byte 0: [1-bit ver:1][3-bit codec:1][4-bit sample_rate]
/// Byte 1: [8-bit seq_num]
/// Byte 2-3: [16-bit payload_len BE]
/// Byte 4: [8-bit bitrate_kbps] (0=auto/unknown, 1-255)
/// Byte 5: [8-bit flags/reserved]

pub const HEADER_SIZE: usize = 6;
pub const MAX_PAYLOAD: usize = 1472;
pub const MAX_PACKET: usize = HEADER_SIZE + MAX_PAYLOAD;
pub const MAX_REORDER: usize = 8;

// --- 采样率常量 ---
pub const SAMPLE_RATE_8K: u8 = 0;
pub const SAMPLE_RATE_12K: u8 = 1;
pub const SAMPLE_RATE_16K: u8 = 2;
pub const SAMPLE_RATE_24K: u8 = 3;
pub const SAMPLE_RATE_48K: u8 = 4;

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

// --- 码率助手 ---

/// 协议码率自动值（与 Kotlin BITRATE_AUTO 对齐）
pub const BITRATE_AUTO: u8 = 0;

/// 将 kbps 编码为协议头单字节：0→auto，1..255→(kbps/2)，支持 0–510kbps
/// 与 Kotlin computeBitrateId 100%对齐
pub fn compute_bitrate_id(kbps: u32) -> u8 {
    if kbps == 0 {
        BITRATE_AUTO
    } else {
        ((kbps / 2).min(255).max(1)) as u8
    }
}

/// 根据采样率建议的默认码率（协议编码字节，kbps/2）
/// 与 Android CaptureService 自动码率策略对齐：
///   48kHz→512kbps, 24kHz→256kbps, 16kHz→128kbps, ≤12kHz→64kbps
pub fn default_bitrate_for_sr(sr: u8) -> u8 {
    match sr {
        SAMPLE_RATE_8K  => compute_bitrate_id(64),   // 64kbps → 32
        SAMPLE_RATE_12K => compute_bitrate_id(64),   // 64kbps → 32
        SAMPLE_RATE_16K => compute_bitrate_id(128),  // 128kbps → 64
        SAMPLE_RATE_24K => compute_bitrate_id(256),  // 256kbps → 128
        SAMPLE_RATE_48K => compute_bitrate_id(512),  // 512kbps → 255 (coerced from 256)
        _ => compute_bitrate_id(64),                 // 64kbps 默认
    }
}

/// bitrate=0 表示 auto/unknown，返回建议的默认码率（协议编码字节）
pub fn resolve_bitrate(bitrate: u8, sample_rate: u8) -> u8 {
    if bitrate == 0 {
        default_bitrate_for_sr(sample_rate)
    } else {
        bitrate
    }
}

// --- 包头 ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub version: u8,
    pub codec: u8,
    pub sample_rate: u8,
    pub seq_num: u8,
    pub payload_len: u16,
    pub bitrate: u8, // kbps, 0=auto
    pub flags: u8,   // 保留/扩展标志
}

impl Default for PacketHeader {
    fn default() -> Self {
        Self {
            version: 1,
            codec: 1,
            sample_rate: SAMPLE_RATE_48K,
            seq_num: 0,
            payload_len: 0,
            bitrate: 0,
            flags: 0,
        }
    }
}

pub fn encode_header(header: &PacketHeader, buf: &mut [u8; HEADER_SIZE]) {
    buf[0] = (header.version & 0x01) << 7 | (header.codec & 0x07) << 4 | (header.sample_rate & 0x0F);
    buf[1] = header.seq_num;
    buf[2] = (header.payload_len >> 8) as u8;
    buf[3] = header.payload_len as u8;
    buf[4] = header.bitrate;
    buf[5] = header.flags;
}

pub fn decode_header(buf: &[u8; HEADER_SIZE]) -> Option<PacketHeader> {
    let version = (buf[0] >> 7) & 0x01;
    if version != 1 {
        return None;
    }
    let codec = (buf[0] >> 4) & 0x07;
    if codec != 1 {
        return None;
    }
    let sample_rate = buf[0] & 0x0F;
    if sample_rate > SAMPLE_RATE_48K {
        return None;
    }
    let seq_num = buf[1];
    let payload_len = u16::from_be_bytes([buf[2], buf[3]]);
    if payload_len as usize > MAX_PAYLOAD {
        return None;
    }
    let bitrate = buf[4];
    let flags = buf[5];
    Some(PacketHeader {
        version,
        codec,
        sample_rate,
        seq_num,
        payload_len,
        bitrate,
        flags,
    })
}

/// 构建UDP音频包
/// bitrate=0 表示 auto（接收端使用默认码率显示）
pub fn build_packet(sample_rate: u8, seq_num: u8, payload: &[u8], bitrate: u8) -> Vec<u8> {
    let len = payload.len().min(MAX_PAYLOAD);
    let mut packet = Vec::with_capacity(HEADER_SIZE + len);
    let mut hdr_buf = [0u8; HEADER_SIZE];
    let header = PacketHeader {
        sample_rate,
        seq_num,
        payload_len: len as u16,
        bitrate,
        ..Default::default()
    };
    encode_header(&header, &mut hdr_buf);
    packet.extend_from_slice(&hdr_buf);
    packet.extend_from_slice(&payload[..len]);
    packet
}

// --- 零分配重排缓冲区 ---

struct ReorderSlot {
    seq: u8,
    sample_rate: u8, // 缓存每个包自己的采样率，修复乱序包解码爆音
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
            seq: 0,
            sample_rate: 0,
            len: 0,
            data: [0u8; MAX_PAYLOAD],
            valid: false,
        };
        Self {
            buf: [INIT; MAX_REORDER],
            write: 0,
            next_seq: 0,
        }
    }

    /// 零分配插入并顺带输出已就绪的包
    /// 对于顺序到达的包，直接通过回调输出，无任何堆分配和拷贝
    /// 对于乱序到达的包，拷贝到内部栈缓冲区
    pub fn insert_and_drain<F>(&mut self, seq: u8, sample_rate: u8, payload: &[u8], mut f: F)
    where
        F: FnMut(u8, &[u8]),
    {
        let dist = seq.wrapping_sub(self.next_seq);
        if dist as usize >= MAX_REORDER {
            // 序列号跳跃过大，清空缓冲区，直接输出当前包
            self.clear();
            self.next_seq = seq.wrapping_add(1);
            f(sample_rate, payload);
            return;
        }

        if seq == self.next_seq {
            // 【快速路径】顺序包，直接输出，零拷贝、零堆分配！
            f(sample_rate, payload);
            self.next_seq = self.next_seq.wrapping_add(1);
        } else {
            // 【乱序路径】拷贝到预分配的栈缓冲区
            let len = payload.len().min(MAX_PAYLOAD);
            self.buf[self.write].seq = seq;
            self.buf[self.write].sample_rate = sample_rate;
            self.buf[self.write].len = len;
            self.buf[self.write].data[..len].copy_from_slice(&payload[..len]);
            self.buf[self.write].valid = true;
            self.write = (self.write + 1) % MAX_REORDER;
        }

        // 检查是否有之前乱序但现在已就绪的包
        loop {
            let mut found = false;
            for i in 0..MAX_REORDER {
                if self.buf[i].valid && self.buf[i].seq == self.next_seq {
                    f(
                        self.buf[i].sample_rate,
                        &self.buf[i].data[..self.buf[i].len],
                    );
                    self.buf[i].valid = false;
                    self.next_seq = self.next_seq.wrapping_add(1);
                    found = true;
                    break;
                }
            }
            if !found {
                break;
            }
        }
    }

    fn clear(&mut self) {
        for slot in self.buf.iter_mut() {
            slot.valid = false;
        }
        self.write = 0;
    }

    pub fn reset(&mut self) {
        self.clear();
        self.next_seq = 0;
    }
}

impl Default for ReorderBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let header = PacketHeader {
            version: 1,
            codec: 1,
            sample_rate: SAMPLE_RATE_48K,
            seq_num: 42,
            payload_len: 320,
            bitrate: 32,
            flags: 0,
        };
        let mut buf = [0u8; HEADER_SIZE];
        encode_header(&header, &mut buf);
        let decoded = decode_header(&buf).unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn test_decode_v0_rejected() {
        // v0 packet: version=0 should be rejected by v1 decoder
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = 0x12; // ver=0, codec=1, sr=2
        buf[1] = 0;
        buf[2] = 0x01;
        buf[3] = 0x00;
        buf[4] = 0;
        buf[5] = 0;
        assert!(decode_header(&buf).is_none());
    }

    #[test]
    fn test_decode_invalid_codec() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = 0xA0; // ver=1, codec=0 (invalid)
        assert!(decode_header(&buf).is_none());
    }

    #[test]
    fn test_decode_invalid_sr() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = 0x9F; // ver=1, codec=1, sr=15 (invalid)
        assert!(decode_header(&buf).is_none());
    }

    #[test]
    fn test_decode_oversized() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = 0x94; // ver=1, codec=1, sr=4 (48k)
        buf[2] = 0x06;
        buf[3] = 0x00; // len=1536 > MAX_PAYLOAD
        assert!(decode_header(&buf).is_none());
    }

    #[test]
    fn test_resolve_bitrate() {
        // 自动码率 → 与 Android auto-bitrate 对齐 (kbps/2 协议编码)
        assert_eq!(resolve_bitrate(0, SAMPLE_RATE_48K), compute_bitrate_id(512)); // 255
        assert_eq!(resolve_bitrate(0, SAMPLE_RATE_24K), compute_bitrate_id(256)); // 128
        assert_eq!(resolve_bitrate(0, SAMPLE_RATE_16K), compute_bitrate_id(128)); // 64
        assert_eq!(resolve_bitrate(0, SAMPLE_RATE_12K), compute_bitrate_id(64));  // 32
        assert_eq!(resolve_bitrate(0, SAMPLE_RATE_8K),  compute_bitrate_id(64));  // 32
        // 显式码率直接透传
        assert_eq!(resolve_bitrate(64, SAMPLE_RATE_48K), 64);
        assert_eq!(resolve_bitrate(255, SAMPLE_RATE_48K), 255);
    }

    #[test]
    fn test_compute_bitrate_id() {
        assert_eq!(compute_bitrate_id(0), 0);     // auto
        assert_eq!(compute_bitrate_id(64), 32);   // 64/2
        assert_eq!(compute_bitrate_id(128), 64);  // 128/2
        assert_eq!(compute_bitrate_id(256), 128); // 256/2
        assert_eq!(compute_bitrate_id(512), 255); // 512/2=256, coerced to 255 (OPUS 510kbps ceiling)
    }

    #[test]
    fn test_build_packet_with_bitrate() {
        let payload = vec![0u8; 100];
        let packet = build_packet(SAMPLE_RATE_48K, 5, &payload, 32);
        assert_eq!(packet.len(), HEADER_SIZE + 100);
        let hdr: [u8; HEADER_SIZE] = packet[..HEADER_SIZE].try_into().unwrap();
        let decoded = decode_header(&hdr).unwrap();
        assert_eq!(decoded.sample_rate, SAMPLE_RATE_48K);
        assert_eq!(decoded.seq_num, 5);
        assert_eq!(decoded.payload_len, 100);
        assert_eq!(decoded.bitrate, 32);
    }

    // --- 更新重排缓冲区测试以匹配零分配 API ---

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
        assert!(out.is_empty()); // 包1未就绪
        
        rb.insert_and_drain(0, 1, &[1], |sr, data| out.push((sr, data.to_vec())));
        assert_eq!(out.len(), 2); // 包0就绪，包1随之就绪
        assert_eq!(out[0], (1, vec![1]));
        assert_eq!(out[1], (1, vec![2]));
    }

    #[test]
    fn test_reorder_skip() {
        let mut rb = ReorderBuffer::new();
        let mut out: Vec<(u8, Vec<u8>)> = Vec::new();
        
        rb.insert_and_drain(8, 1, &[4], |sr, data| out.push((sr, data.to_vec())));
        assert_eq!(out.len(), 1); // 跳跃过大直接输出
        assert_eq!(out[0], (1, vec![4]));
    }
}