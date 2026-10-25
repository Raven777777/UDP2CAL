//! UDP2CAL 控制协议 v3（Rust 真源实现）
//!
//! 媒体面不再是控制包的一种：所有音频都走标准 RTP（见 [`rtp`] 模块），
//! 本模块只负责控制面。
//!
//! ## 固定头（15 字节）
//! ```text
//! Byte 0:       0x03                     控制协议版本
//! Byte 1:       0x00                     保留（发送写 0，接收忽略）
//! Byte 2:       msg_type
//! Byte 3-4:     payload_len (u16 BE)
//! Byte 5:       0x00                     保留
//! Byte 6:       0x00                     保留
//! Byte 7-14:    device_id[8]
//! Byte 15+:     payload
//! ```
//!
//! 控制包与 RTP 共用同一个 UDP socket，接收端必须先看首字节再分流：
//! `0x80..=0xBF` 为 RTP，`0x03` 为控制包，其余丢弃（见 [`demux`]）。
//!
//! 协议只有单一版本：`byte[0]` 必须等于 [`CONTROL_VERSION`]，不做协商、
//! 不降级、不探测 v2。

pub mod rtp;

// ═══════════════════════ 常量 ═══════════════════════

pub const CONTROL_VERSION: u8 = 0x03;
pub const CONTROL_HEADER_SIZE: usize = 15;
/// 控制负载上限；超过即丢弃（见规范 5.1）
pub const CONTROL_MAX_PAYLOAD: usize = 512;

pub const DEVICE_ID_SIZE: usize = 8;
pub const DISCOVER_PORT: u16 = 44043;
pub const DEFAULT_LISTEN_PORT: u16 = 44044;

/// CONNECT 发送间隔（保活 + 参数同步）
pub const KEEPALIVE_INTERVAL_MS: u64 = 1000;
/// Android 侧 ACK 超时
pub const ACK_TIMEOUT_MS: u64 = 3000;
/// Windows 侧绑定释放超时
pub const PEER_TIMEOUT_MS: u64 = 3000;

// ── 消息类型 ──
/// v3 废弃：媒体走 RTP，控制包中出现该类型一律丢弃
pub const TYPE_DATA: u8 = 0;
pub const TYPE_CONNECT: u8 = 1;
pub const TYPE_DISCOVER_REQ: u8 = 2;
pub const TYPE_DISCOVER_REPLY: u8 = 3;
pub const TYPE_CONNECT_ACK: u8 = 4;
/// 已知类型的上界；5..=255 收到后忽略（不报错）
pub const TYPE_MAX_KNOWN: u8 = TYPE_CONNECT_ACK;

// ── 负载长度 ──
pub const CONNECT_PAYLOAD_SIZE: usize = 15;
pub const CONNECT_ACK_PAYLOAD_SIZE: usize = 6;
/// DISCOVER_REPLY 中设备名上限（不含 2 字节 listen_port）
pub const DEVICE_NAME_MAX: usize = 254;

// ── Opus 参数（控制面只传反向编码器配置）──
pub const OPUS_PAYLOAD_TYPE: u8 = 111;
pub const OPUS_FRAME_MS: u8 = 10;

pub const OPUS_FLAG_VBR: u8 = 0x01;
pub const OPUS_FLAG_DTX: u8 = 0x02;
pub const OPUS_FLAG_VBR_CONSTRAINT: u8 = 0x04;
pub const OPUS_FLAG_FEC: u8 = 0x08;
pub const OPUS_FLAGS_RESERVED_MASK: u8 = 0xF0;

/// 自动码率（`bitrate_kbps == 0`）映射值
pub const BITRATE_AUTO_KBPS: u16 = 512;
/// 码率硬上限：1024 kbps 会产出 1280 字节负载，超过 Opus/RTP 上限
pub const BITRATE_MAX_KBPS: u16 = 1000;

/// Opus 复杂度上限（`0` 表示使用默认值）
pub const OPUS_COMPLEXITY_MAX: u8 = 10;

// ═══════════════════════ 固定头 ═══════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlHeader {
    pub msg_type: u8,
    pub payload_len: u16,
    pub device_id: [u8; DEVICE_ID_SIZE],
}

impl ControlHeader {
    pub const fn new(msg_type: u8, payload_len: u16, device_id: [u8; DEVICE_ID_SIZE]) -> Self {
        Self { msg_type, payload_len, device_id }
    }

    /// 写入 15 字节固定头；保留字节强制写 0。
    pub fn encode(&self, buf: &mut [u8; CONTROL_HEADER_SIZE]) {
        buf[0] = CONTROL_VERSION;
        buf[1] = 0;
        buf[2] = self.msg_type;
        buf[3] = (self.payload_len >> 8) as u8;
        buf[4] = self.payload_len as u8;
        buf[5] = 0;
        buf[6] = 0;
        buf[7..15].copy_from_slice(&self.device_id);
    }

    /// 解析固定头。**只校验头部自身**（长度 ≥15、版本、负载上限）；
    /// 数据报总长度与 `payload_len` 是否一致由 [`parse_control`] 负责。
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < CONTROL_HEADER_SIZE {
            return None;
        }
        if buf[0] != CONTROL_VERSION {
            return None;
        }
        let payload_len = u16::from_be_bytes([buf[3], buf[4]]);
        if payload_len as usize > CONTROL_MAX_PAYLOAD {
            return None;
        }
        let mut device_id = [0u8; DEVICE_ID_SIZE];
        device_id.copy_from_slice(&buf[7..15]);
        Some(Self { msg_type: buf[2], payload_len, device_id })
    }

    /// 是否为 v3 已定义的消息类型（1..=4）。`0` 与 `5..=255` 均返回 false。
    pub fn is_known_type(&self) -> bool {
        self.msg_type >= TYPE_CONNECT && self.msg_type <= TYPE_MAX_KNOWN
    }
}

/// 解析一个完整控制数据报，返回（头, 负载）。
///
/// 依次执行规范 5.7 的前 4 步校验：长度 ≥15、版本 = 3、`payload_len <= 512`、
/// 数据报长度精确等于 `15 + payload_len`。消息类型与负载布局由调用方继续校验。
pub fn parse_control(buf: &[u8]) -> Option<(ControlHeader, &[u8])> {
    let hdr = ControlHeader::decode(buf)?;
    if buf.len() != CONTROL_HEADER_SIZE + hdr.payload_len as usize {
        return None;
    }
    Some((hdr, &buf[CONTROL_HEADER_SIZE..]))
}

/// 构造成完整控制包（分配新 Vec）
///
/// 负载上限 [`CONTROL_MAX_PAYLOAD`]：超限时截断到上限（保证不会构造出非法
/// 数据报——对端会因 `payload_len` 与布局不符而整包丢弃）。这是**兜底**而非
/// 正常路径：所有调用方的负载都是定长且远小于 512。开发期用 `debug_assert`
/// 把误用暴露出来；需要"超限即失败"的语义请改用 [`build_control_to`]。
pub fn build_control(msg_type: u8, device_id: &[u8; DEVICE_ID_SIZE], payload: &[u8]) -> Vec<u8> {
    debug_assert!(
        payload.len() <= CONTROL_MAX_PAYLOAD,
        "控制负载 {} 字节超过上限 {}，将被截断",
        payload.len(),
        CONTROL_MAX_PAYLOAD
    );
    let len = payload.len().min(CONTROL_MAX_PAYLOAD);
    let mut out = Vec::with_capacity(CONTROL_HEADER_SIZE + len);
    let mut hdr = [0u8; CONTROL_HEADER_SIZE];
    ControlHeader::new(msg_type, len as u16, *device_id).encode(&mut hdr);
    out.extend_from_slice(&hdr);
    out.extend_from_slice(&payload[..len]);
    out
}

/// 构造成完整控制包（写入预分配缓冲区，零分配）。
/// 返回写入的字节数；缓冲区不足或负载超限返回 `None`。
pub fn build_control_to(
    dest: &mut [u8],
    offset: usize,
    msg_type: u8,
    device_id: &[u8; DEVICE_ID_SIZE],
    payload: &[u8],
) -> Option<usize> {
    if payload.len() > CONTROL_MAX_PAYLOAD {
        return None;
    }
    let total = CONTROL_HEADER_SIZE + payload.len();
    if offset + total > dest.len() {
        return None;
    }
    dest[offset] = CONTROL_VERSION;
    dest[offset + 1] = 0;
    dest[offset + 2] = msg_type;
    dest[offset + 3] = (payload.len() >> 8) as u8;
    dest[offset + 4] = payload.len() as u8;
    dest[offset + 5] = 0;
    dest[offset + 6] = 0;
    dest[offset + 7..offset + 15].copy_from_slice(device_id);
    dest[offset + CONTROL_HEADER_SIZE..offset + total].copy_from_slice(payload);
    Some(total)
}

// ═══════════════════════ 同 socket 解复用 ═══════════════════════

/// 一个数据报的归属。`Drop` 表示首字节非法或长度不足，调用方只计数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Datagram<'a> {
    Rtp(&'a [u8]),
    Control(&'a [u8]),
    Drop,
}

/// 按首字节分流（规范 3.2）。**不得**用长度是否 ≥15 判断控制包：
/// DTX 期间的 RTP 包可能只有 13 字节。
pub fn demux(buf: &[u8]) -> Datagram<'_> {
    let Some(&b0) = buf.first() else {
        return Datagram::Drop;
    };
    match b0 {
        0x80..=0xBF => {
            if buf.len() < rtp::RTP_HEADER_SIZE + 1 || buf.len() > rtp::RTP_MAX_PACKET {
                Datagram::Drop
            } else {
                Datagram::Rtp(buf)
            }
        }
        CONTROL_VERSION => {
            if buf.len() < CONTROL_HEADER_SIZE || buf.len() > CONTROL_HEADER_SIZE + CONTROL_MAX_PAYLOAD
            {
                Datagram::Drop
            } else {
                Datagram::Control(buf)
            }
        }
        _ => Datagram::Drop,
    }
}

// ═══════════════════════ CONNECT（类型 1） ═══════════════════════

/// CONNECT 负载，固定 15 字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectPayload {
    /// Android 反向接收端口；`0` 表示不接收反向音频
    pub reverse_port: u16,
    /// Android 正向 RTP 的 SSRC；必须非零
    pub forward_ssrc: u32,
    pub payload_type: u8,
    pub frame_ms: u8,
    /// `1..=10`；`0` 表示使用默认值
    pub complexity: u8,
    /// `0`=auto, `1`=voice, `2`=music
    pub signal: u8,
    /// `0`=NB, `1`=MB, `2`=WB, `3`=SWB, `4`=FB
    pub bandwidth: u8,
    /// bit0=VBR, bit1=DTX, bit2=VBR 约束, bit3=FEC
    pub opus_flags: u8,
    /// 反向 Opus 目标码率；`0`=自动（512 kbps）
    pub bitrate_kbps: u16,
    /// 预期丢包率 0..=100
    pub packet_loss: u8,
}

impl Default for ConnectPayload {
    fn default() -> Self {
        Self {
            reverse_port: 0,
            forward_ssrc: 1,
            payload_type: OPUS_PAYLOAD_TYPE,
            frame_ms: OPUS_FRAME_MS,
            complexity: 0,
            signal: 0,
            bandwidth: 4,
            opus_flags: OPUS_FLAG_VBR,
            bitrate_kbps: 0,
            packet_loss: 0,
        }
    }
}

impl ConnectPayload {
    pub fn encode(&self) -> [u8; CONNECT_PAYLOAD_SIZE] {
        let mut b = [0u8; CONNECT_PAYLOAD_SIZE];
        b[0..2].copy_from_slice(&self.reverse_port.to_be_bytes());
        b[2..6].copy_from_slice(&self.forward_ssrc.to_be_bytes());
        b[6] = self.payload_type;
        b[7] = self.frame_ms;
        b[8] = self.complexity;
        b[9] = self.signal;
        b[10] = self.bandwidth;
        b[11] = self.opus_flags;
        b[12..14].copy_from_slice(&self.bitrate_kbps.to_be_bytes());
        b[14] = self.packet_loss;
        b
    }

    /// 解析负载；长度必须恰好 15 字节。
    pub fn decode(p: &[u8]) -> Option<Self> {
        if p.len() != CONNECT_PAYLOAD_SIZE {
            return None;
        }
        Some(Self {
            reverse_port: u16::from_be_bytes([p[0], p[1]]),
            forward_ssrc: u32::from_be_bytes([p[2], p[3], p[4], p[5]]),
            payload_type: p[6],
            frame_ms: p[7],
            complexity: p[8],
            signal: p[9],
            bandwidth: p[10],
            opus_flags: p[11],
            bitrate_kbps: u16::from_be_bytes([p[12], p[13]]),
            packet_loss: p[14],
        })
    }

    /// 校验能否接受。`Err` 携带原因（用于日志，不影响丢弃语义）。
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.payload_type != OPUS_PAYLOAD_TYPE {
            return Err("payload_type != 111");
        }
        if self.frame_ms != OPUS_FRAME_MS {
            return Err("frame_ms != 10");
        }
        if self.forward_ssrc == 0 {
            return Err("forward_ssrc == 0");
        }
        Ok(())
    }

    pub fn vbr(&self) -> bool {
        self.opus_flags & OPUS_FLAG_VBR != 0
    }
    pub fn dtx(&self) -> bool {
        self.opus_flags & OPUS_FLAG_DTX != 0
    }
    pub fn vbr_constraint(&self) -> bool {
        self.opus_flags & OPUS_FLAG_VBR_CONSTRAINT != 0
    }
    pub fn fec(&self) -> bool {
        self.opus_flags & OPUS_FLAG_FEC != 0
    }

    /// 生效码率：`0` → 512 kbps；超过 1000 钳制到 1000。
    pub fn effective_bitrate_kbps(&self) -> u16 {
        clamp_bitrate_kbps(self.bitrate_kbps)
    }
}

/// 码率归一：`0`=自动→512，上限 1000。
pub fn clamp_bitrate_kbps(kbps: u16) -> u16 {
    match kbps {
        0 => BITRATE_AUTO_KBPS,
        k if k > BITRATE_MAX_KBPS => BITRATE_MAX_KBPS,
        k => k,
    }
}

// ═══════════════════════ CONNECT_ACK（类型 4） ═══════════════════════

/// ACK 负载，固定 6 字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectAckPayload {
    /// Windows 反向 RTP 的 SSRC；`0` 表示反向未启用
    pub reverse_ssrc: u32,
    pub payload_type: u8,
    pub frame_ms: u8,
}

impl Default for ConnectAckPayload {
    fn default() -> Self {
        Self { reverse_ssrc: 0, payload_type: OPUS_PAYLOAD_TYPE, frame_ms: OPUS_FRAME_MS }
    }
}

impl ConnectAckPayload {
    pub fn encode(&self) -> [u8; CONNECT_ACK_PAYLOAD_SIZE] {
        let mut b = [0u8; CONNECT_ACK_PAYLOAD_SIZE];
        b[0..4].copy_from_slice(&self.reverse_ssrc.to_be_bytes());
        b[4] = self.payload_type;
        b[5] = self.frame_ms;
        b
    }

    /// 解析负载；长度必须恰好 6 字节，PT/帧长必须合法。
    pub fn decode(p: &[u8]) -> Option<Self> {
        if p.len() != CONNECT_ACK_PAYLOAD_SIZE {
            return None;
        }
        if p[4] != OPUS_PAYLOAD_TYPE || p[5] != OPUS_FRAME_MS {
            return None;
        }
        Some(Self {
            reverse_ssrc: u32::from_be_bytes([p[0], p[1], p[2], p[3]]),
            payload_type: p[4],
            frame_ms: p[5],
        })
    }
}

// ═══════════════════════ DISCOVER ═══════════════════════

/// DISCOVER_REQ 负载为空；DISCOVER_REPLY 为 `listen_port(2) + device_name(N)`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoverReply {
    pub listen_port: u16,
    pub device_name: String,
}

impl DiscoverReply {
    pub fn encode(&self) -> Vec<u8> {
        let name = truncate_utf8(&self.device_name, DEVICE_NAME_MAX);
        let mut out = Vec::with_capacity(2 + name.len());
        out.extend_from_slice(&self.listen_port.to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out
    }

    pub fn decode(p: &[u8]) -> Option<Self> {
        if p.len() < 2 {
            return None;
        }
        let listen_port = u16::from_be_bytes([p[0], p[1]]);
        let name = std::str::from_utf8(&p[2..]).ok()?;
        Some(Self { listen_port, device_name: name.to_string() })
    }
}

/// 按字节上限截断字符串，**不切断 UTF-8 多字节字符**。
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ═══════════════════════ 设备 ID ═══════════════════════

/// 判断设备 ID 是否有效（全零视为无效）
pub fn is_valid_device_id(id: &[u8; DEVICE_ID_SIZE]) -> bool {
    id.iter().any(|&b| b != 0)
}

/// 生成 8 字节设备 ID
///
/// 设备 ID 只用于控制面绑定与发现，**不是密码学凭据**：它每秒随
/// DISCOVER_REPLY 明文广播，任何同网段主机都可伪造。这里混入
/// `RandomState`（运行期由 OS 熵初始化）与自增序号，把候选面扩大到
/// 进程级随机 + 时间 + 计数，足以避免"两台机器撞 ID"。
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
    if !is_valid_device_id(&id) {
        id[0] = 1; // 全零视为无效 ID
    }
    id
}

// ═══════════════════════ 测试 ═══════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: [u8; DEVICE_ID_SIZE] = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF];

    #[test]
    fn header_matches_spec_vector_b4() {
        // 附录 B.4：版本=0x03，type=1，payload_len=15
        let hdr = ControlHeader::new(TYPE_CONNECT, 15, DEV);
        let mut buf = [0u8; CONTROL_HEADER_SIZE];
        hdr.encode(&mut buf);
        assert_eq!(
            buf,
            [0x03, 0x00, 0x01, 0x00, 0x0F, 0x00, 0x00, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]
        );
        let back = ControlHeader::decode(&buf).unwrap();
        assert_eq!(back.msg_type, TYPE_CONNECT);
        assert_eq!(back.payload_len, 15);
        assert_eq!(back.device_id, DEV);
    }

    #[test]
    fn connect_payload_matches_spec_vector_b5() {
        let p = ConnectPayload {
            reverse_port: 50000,
            forward_ssrc: 0x1122_3344,
            payload_type: 111,
            frame_ms: 10,
            complexity: 3,
            signal: 1,
            bandwidth: 4,
            opus_flags: OPUS_FLAG_VBR | OPUS_FLAG_DTX,
            bitrate_kbps: 0,
            packet_loss: 5,
        };
        assert_eq!(
            p.encode(),
            [0xC3, 0x50, 0x11, 0x22, 0x33, 0x44, 0x6F, 0x0A, 0x03, 0x01, 0x04, 0x03, 0x00, 0x00, 0x05]
        );
        assert_eq!(ConnectPayload::decode(&p.encode()), Some(p));
        assert!(p.validate().is_ok());
        assert!(p.vbr() && p.dtx() && !p.fec() && !p.vbr_constraint());
    }

    #[test]
    fn connect_ack_matches_spec_vector_b6() {
        let a = ConnectAckPayload { reverse_ssrc: 0xAABB_CCDD, payload_type: 111, frame_ms: 10 };
        assert_eq!(a.encode(), [0xAA, 0xBB, 0xCC, 0xDD, 0x6F, 0x0A]);
        assert_eq!(ConnectAckPayload::decode(&a.encode()), Some(a));
    }

    #[test]
    fn connect_full_packet_bytes() {
        let p = ConnectPayload {
            reverse_port: 50000,
            forward_ssrc: 0x1122_3344,
            complexity: 3,
            signal: 1,
            bandwidth: 4,
            opus_flags: OPUS_FLAG_VBR | OPUS_FLAG_DTX,
            packet_loss: 5,
            ..ConnectPayload::default()
        };
        let pkt = build_control(TYPE_CONNECT, &DEV, &p.encode());
        assert_eq!(pkt.len(), CONTROL_HEADER_SIZE + CONNECT_PAYLOAD_SIZE);
        assert_eq!(
            pkt,
            [
                0x03, 0x00, 0x01, 0x00, 0x0F, 0x00, 0x00, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD,
                0xEF, 0xC3, 0x50, 0x11, 0x22, 0x33, 0x44, 0x6F, 0x0A, 0x03, 0x01, 0x04, 0x03, 0x00,
                0x00, 0x05
            ]
        );
        let hdr = ControlHeader::decode(&pkt).unwrap();
        assert_eq!(hdr.msg_type, TYPE_CONNECT);
        assert_eq!(&pkt[CONTROL_HEADER_SIZE..], &p.encode()[..]);
    }

    #[test]
    fn discover_reply_matches_spec_vector() {
        let r = DiscoverReply { listen_port: DEFAULT_LISTEN_PORT, device_name: "UDP2CAL".into() };
        let mut pkt = build_control(TYPE_DISCOVER_REPLY, &DEV, &r.encode());
        assert_eq!(
            pkt,
            [
                0x03, 0x00, 0x03, 0x00, 0x09, 0x00, 0x00, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD,
                0xEF, 0xAC, 0x0C, 0x55, 0x44, 0x50, 0x32, 0x43, 0x41, 0x4C
            ]
        );
        let hdr = ControlHeader::decode(&pkt).unwrap();
        assert_eq!(hdr.msg_type, TYPE_DISCOVER_REPLY);
        let reply = DiscoverReply::decode(&pkt[CONTROL_HEADER_SIZE..]).unwrap();
        assert_eq!(reply.listen_port, 44044);
        assert_eq!(reply.device_name, "UDP2CAL");
        pkt.clear();
        assert!(pkt.is_empty());
    }

    #[test]
    fn discover_reply_rejects_short_payload() {
        assert!(DiscoverReply::decode(&[0xAC]).is_none());
        assert!(DiscoverReply::decode(&[]).is_none());
    }

    #[test]
    fn truncate_utf8_never_splits_char() {
        // "电" 占 3 字节，max=4 时只能保留 1 个完整字符
        let s = "电音";
        assert_eq!(truncate_utf8(s, 4), "电");
        assert_eq!(truncate_utf8(s, 3), "电");
        assert_eq!(truncate_utf8(s, 2), "");
        assert_eq!(truncate_utf8("abc", 2), "ab");
        // 设备名上限截断后不得出现非法 UTF-8
        let long = "电".repeat(200);
        let enc = DiscoverReply { listen_port: 1, device_name: long }.encode();
        assert!(enc.len() <= 2 + DEVICE_NAME_MAX);
        assert!(std::str::from_utf8(&enc[2..]).is_ok());
    }

    #[test]
    fn rejects_bad_control_header() {
        // 版本错误
        let mut pkt = build_control(TYPE_CONNECT, &DEV, &ConnectPayload::default().encode());
        pkt[0] = 0x02;
        assert!(ControlHeader::decode(&pkt).is_none());

        // 长度与 payload_len 不一致（少一字节）
        let pkt = build_control(TYPE_CONNECT, &DEV, &ConnectPayload::default().encode());
        assert!(parse_control(&pkt[..pkt.len() - 1]).is_none());
        // 尾部多一字节同样拒绝
        let mut longer = pkt.clone();
        longer.push(0);
        assert!(parse_control(&longer).is_none());

        // 负载超限
        let big = vec![0u8; CONTROL_MAX_PAYLOAD + 1];
        let mut pkt = build_control(TYPE_CONNECT, &DEV, &[]);
        pkt[3] = 0x02;
        pkt[4] = 0x01; // 513
        pkt.extend_from_slice(&big);
        assert!(ControlHeader::decode(&pkt).is_none());

        // 负载上限边界 512 合法
        let pkt = build_control(TYPE_CONNECT, &DEV, &vec![0u8; CONTROL_MAX_PAYLOAD]);
        assert_eq!(ControlHeader::decode(&pkt).unwrap().payload_len, 512);
    }

    #[test]
    fn msg_type_zero_is_deprecated() {
        let pkt = build_control(TYPE_DATA, &DEV, &[]);
        let hdr = ControlHeader::decode(&pkt).unwrap();
        assert_eq!(hdr.msg_type, 0);
        assert!(!hdr.is_known_type());
        // 5..=255 也不属于已知类型（应忽略而非报错）
        let pkt = build_control(5, &DEV, &[]);
        assert!(!ControlHeader::decode(&pkt).unwrap().is_known_type());
        let pkt = build_control(255, &DEV, &[]);
        assert!(!ControlHeader::decode(&pkt).unwrap().is_known_type());
    }

    #[test]
    fn connect_rejects_bad_params() {
        let mut p = ConnectPayload::default();
        assert!(p.validate().is_ok());

        p.payload_type = 110;
        assert!(p.validate().is_err());

        let mut p = ConnectPayload::default();
        p.frame_ms = 20;
        assert!(p.validate().is_err());

        let mut p = ConnectPayload::default();
        p.forward_ssrc = 0;
        assert!(p.validate().is_err());

        // 负载长度必须恰好 15
        assert!(ConnectPayload::decode(&[0u8; 14]).is_none());
        assert!(ConnectPayload::decode(&[0u8; 16]).is_none());
    }

    #[test]
    fn ack_rejects_bad_params() {
        assert!(ConnectAckPayload::decode(&[0u8; 5]).is_none());
        assert!(ConnectAckPayload::decode(&[0u8; 7]).is_none());
        let mut raw = ConnectAckPayload::default().encode();
        raw[4] = 0; // PT 错误
        assert!(ConnectAckPayload::decode(&raw).is_none());
        let mut raw = ConnectAckPayload::default().encode();
        raw[5] = 20; // 帧长错误
        assert!(ConnectAckPayload::decode(&raw).is_none());
    }

    #[test]
    fn bitrate_clamped_to_1000() {
        assert_eq!(clamp_bitrate_kbps(0), 512);
        assert_eq!(clamp_bitrate_kbps(512), 512);
        assert_eq!(clamp_bitrate_kbps(960), 960);
        assert_eq!(clamp_bitrate_kbps(1000), 1000);
        assert_eq!(clamp_bitrate_kbps(1024), 1000);
        assert_eq!(clamp_bitrate_kbps(65535), 1000);
        let p = ConnectPayload { bitrate_kbps: 1024, ..ConnectPayload::default() };
        assert_eq!(p.effective_bitrate_kbps(), 1000);
    }

    #[test]
    fn demux_classifies_by_first_byte() {
        // RTP
        let rtp = [0x80u8, 0xEF, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 1, 0xF8];
        assert!(matches!(demux(&rtp), Datagram::Rtp(_)));

        // 控制
        let ctl = build_control(TYPE_CONNECT, &DEV, &ConnectPayload::default().encode());
        assert!(matches!(demux(&ctl), Datagram::Control(_)));

        // 保留位非零的 RTP（0x81 带填充）仍归为 RTP，由 rtp::parse 拒绝
        let padded = [0x81u8, 0x6F, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0x00];
        assert!(matches!(demux(&padded), Datagram::Rtp(_)));

        // 其他首字节
        assert!(matches!(demux(&[0x00; 20]), Datagram::Drop));
        assert!(matches!(demux(&[0xC0; 20]), Datagram::Drop));
        assert!(matches!(demux(&[]), Datagram::Drop));

        // 控制包长度不足 15
        assert!(matches!(demux(&[0x03; 14]), Datagram::Drop));
        // RTP 长度不足 13（DTX 最小包为 13 字节，必须是 12+1）
        assert!(matches!(demux(&[0x80; 12]), Datagram::Drop));
        // RTP 超过 1287
        let mut big = vec![0u8; crate::rtp::RTP_MAX_PACKET + 1];
        big[0] = 0x80;
        assert!(matches!(demux(&big), Datagram::Drop));
    }

    #[test]
    fn build_control_to_zero_alloc() {
        let mut buf = [0u8; 64];
        let payload = ConnectPayload::default().encode();
        let n = build_control_to(&mut buf, 3, TYPE_CONNECT, &DEV, &payload).unwrap();
        assert_eq!(n, CONTROL_HEADER_SIZE + CONNECT_PAYLOAD_SIZE);
        assert_eq!(buf[3], CONTROL_VERSION);
        assert_eq!(buf[5], TYPE_CONNECT);
        assert_eq!(&buf[10..18], &DEV);
        assert_eq!(&buf[18..18 + payload.len()], &payload);

        // 缓冲区不足
        assert!(build_control_to(&mut buf, 60, TYPE_CONNECT, &DEV, &payload).is_none());
        // 负载超限
        assert!(build_control_to(&mut buf, 0, TYPE_CONNECT, &DEV, &vec![0u8; 513]).is_none());
    }

    #[test]
    fn device_id_validity() {
        let id = generate_device_id();
        assert!(is_valid_device_id(&id));
        assert!(!is_valid_device_id(&[0u8; DEVICE_ID_SIZE]));
        assert_ne!(generate_device_id(), id);
    }

    #[test]
    fn header_is_not_ambiguous_with_short_rtp() {
        // 13 字节的 DTX RTP 包不得被当成控制包（长度 <15 但首字节为 0x80）
        let dtx = [0x80u8, 0x6F, 0x00, 0x02, 0, 0, 0, 0, 0, 0, 0, 1, 0x00];
        assert_eq!(dtx.len(), 13);
        assert!(matches!(demux(&dtx), Datagram::Rtp(_)));
    }
}
