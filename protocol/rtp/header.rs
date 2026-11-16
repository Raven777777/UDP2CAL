//! RTP 线格式：固定头编解码、包校验与序号/时间戳扩展。

pub const RTP_VERSION: u8 = 2;
pub const RTP_HEADER_SIZE: usize = 12;
pub const OPUS_PAYLOAD_TYPE: u8 = 111;

/// Opus 单包负载上限（RFC 7587 + 1275 字节实用上限）
pub const OPUS_MAX_PAYLOAD: usize = 1275;
/// 单个 RTP 包上限：12 字节头 + 1275 字节负载
pub const RTP_MAX_PACKET: usize = RTP_HEADER_SIZE + OPUS_MAX_PAYLOAD;
/// 加上 UDP(8) + IPv4(20) 后的上限，必须小于典型以太网 MTU
pub const UDP_IP_MAX_PACKET: usize = RTP_MAX_PACKET + 8 + 20;

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
}
