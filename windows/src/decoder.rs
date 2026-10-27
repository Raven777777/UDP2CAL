// Opus 解码器封装 - audiopus 0.3.0-rc.0
//
// v3 只使用一种解码配置：**48 kHz / 单声道 / 固定 10 ms 帧**。
// 声道数由方向决定（正向 Windows 收单声道，反向 Android 收立体声），
// 不得从 RTP 头或控制包推断；帧长固定 480 采样，任何其它长度都视为异常。
use audiopus::coder::Decoder;
use audiopus::packet::Packet;
use audiopus::{Channels, MutSignals, SampleRate};
use std::convert::TryFrom;

use crate::protocol::rtp::OPUS_FRAME_SAMPLES;

/// Opus 单声道最大帧长 120 ms = 5760 采样；解码缓冲按上限分配，
/// 实际只接受 480 采样（10 ms）的结果。
const MAX_FRAME_SAMPLES: usize = 5760;

/// Deep PLC 运行时开关：解码复杂度 ≥5 且库以 OPUS_DEEP_PLC 构建时启用
const DEEP_PLC_COMPLEXITY: u8 = 5;

pub struct OpusDecoder {
    decoder: Decoder,
    pcm_buf: Vec<f32>,
    last_n: usize,
}

impl OpusDecoder {
    pub fn new() -> Result<Self, audiopus::Error> {
        let decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono)?;
        // Opus 1.5+：复杂度 ≥5 时启用 Deep PLC（库需 OPUS_DEEP_PLC 构建），
        // 未编译该特性时此调用无效，经典 PLC 照常工作。
        let _ = decoder.set_complexity(DEEP_PLC_COMPLEXITY);
        Ok(Self {
            decoder,
            pcm_buf: vec![0.0f32; MAX_FRAME_SAMPLES],
            last_n: 0,
        })
    }

    /// 重建解码器状态（重同步 / 新会话）。
    /// Opus 没有"清空"接口，直接换一个新实例最可靠。
    pub fn reset(&mut self) {
        if let Ok(dec) = Decoder::new(SampleRate::Hz48000, Channels::Mono) {
            let _ = dec.set_complexity(DEEP_PLC_COMPLEXITY);
            self.decoder = dec;
        }
        self.last_n = 0;
    }

    /// 解码一个 10 ms Opus 包。返回 480（成功）或 `None`（解码失败，按丢包处理）。
    pub fn decode(&mut self, data: &[u8]) -> Option<usize> {
        if data.is_empty() {
            return None;
        }
        let packet = Packet::try_from(data).ok()?;
        let signals = MutSignals::try_from(self.pcm_buf.as_mut_slice()).ok()?;
        match self.decoder.decode_float(Some(packet), signals, false) {
            Ok(n) if n == OPUS_FRAME_SAMPLES as usize => {
                self.last_n = n;
                Some(n)
            }
            // 帧长不符（非法包 / 120 ms 包）一律按失败处理：
            // 时间线固定 10 ms，多出来的采样没有地方放
            _ => None,
        }
    }

    /// 用**下一个包**里的 LBRR 冗余重建刚刚丢失的那一帧（Opus 带内 FEC）。
    ///
    /// libopus 语义（RFC 6716 §4.4）：`opus_decode(..., decode_fec = 1)` 不解析
    /// `pkt` 自己的音频，而是用它携带的 LBRR 冗余还原**上一帧**。因此调用方
    /// 必须保证同一个包随后还会用 `fec = false` 正常解码一次，解码器状态才连续。
    ///
    /// 包内没有 LBRR（发送端没开 in-band FEC）时 libopus 自动退化成 PLC，
    /// 所以这个调用不会比 [`conceal`](Self::conceal) 更差——但只有发送端开了
    /// FEC 才有真实收益。返回 `None` 表示连退化 PLC 都失败，调用方走静音。
    pub fn decode_fec(&mut self, data: &[u8]) -> Option<usize> {
        if data.is_empty() {
            return None;
        }
        let packet = Packet::try_from(data).ok()?;
        // 只交出**恰好一帧**的输出缓冲：`opus_decode(fec = 1)` 会把调用方给的
        // `frame_size` 填满（第一帧是 LBRR 重建，后面都是补的 PLC），
        // 实测传 5760 就返回 5760。我们的时间线固定 10 ms，多出来的帧会被
        // 当成"这一帧"写进播放环 → 延迟棘轮，所以这里必须自己掐住长度。
        let want = OPUS_FRAME_SAMPLES as usize;
        let signals = MutSignals::try_from(&mut self.pcm_buf[..want]).ok()?;
        match self.decoder.decode_float(Some(packet), signals, true) {
            Ok(n) if n == want => {
                self.last_n = n;
                Some(n)
            }
            _ => None,
        }
    }

    /// 丢包隐藏：恒定产出一帧 10 ms（480 采样）。
    ///
    /// libopus 的 PLC 按解码器内部的 `frame_size` 合成，而该值来自**上一次被解析**
    /// （不是被接受）的包：一个 20/40/60 ms 包被 `decode` 拒绝后，`frame_size`
    /// 已经被改成 960/1920/2880。如果原样返回，调用方会把 2~6 帧当成一帧 10 ms
    /// 写进播放环（`pull` 只推进 480）→ 环水位每丢包一次涨 2~6 倍（延迟棘轮），
    /// RMS 统计也随之失真。
    ///
    /// 因此这里把结果钉死在 480：超出的部分丢弃，不足的部分补静音。
    pub fn conceal(&mut self) -> Option<usize> {
        let want = OPUS_FRAME_SAMPLES as usize;
        let signals = MutSignals::try_from(self.pcm_buf.as_mut_slice()).ok()?;
        match self.decoder.decode_float(None, signals, false) {
            Ok(n) if n > 0 => {
                let n = n.min(want);
                if n < want {
                    self.pcm_buf[n..want].fill(0.0);
                }
                self.last_n = want;
                Some(want)
            }
            _ => None,
        }
    }

    /// 最近一次解码/补偿得到的 PCM（长度由上一次调用的返回值决定）
    pub fn pcm_mut(&mut self, n: usize) -> &mut [f32] {
        let n = n.min(self.pcm_buf.len());
        &mut self.pcm_buf[..n]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiopus::coder::Encoder;
    use audiopus::{Application, Bitrate};

    fn encode_ms(dec: &mut OpusDecoder, ms: usize) -> usize {
        let mut enc = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Audio)
            .expect("create encoder");
        enc.set_complexity(10).expect("complexity");
        let pcm = vec![0.1f32; 48 * ms]; // 48 kHz 单声道
        let mut buf = vec![0u8; 1500];
        let len = enc.encode_float(&pcm, &mut buf).expect("encode");
        // 把包喂给解码器：20/40/60 ms 会被 decode 拒绝
        assert_eq!(dec.decode(&buf[..len]), None, "{ms} ms 包应被拒绝");
        len
    }

    /// 回归：被拒绝的长帧会污染 libopus 的 `frame_size`，PLC 曾因此返回
    /// 960/1920/2880 而被当作一帧 10 ms 写入播放环（延迟棘轮）。
    #[test]
    fn conceal_is_always_one_10ms_frame_after_rejected_long_packet() {
        let want = OPUS_FRAME_SAMPLES as usize;
        for ms in [20usize, 40, 60] {
            let mut dec = OpusDecoder::new().expect("create decoder");
            encode_ms(&mut dec, ms);
            assert_eq!(
                dec.conceal(),
                Some(want),
                "{ms} ms 包之后 PLC 必须仍然只产出一帧 10 ms"
            );
        }
    }

    // ── 带内 FEC（LBRR）重建 ──

    const FRAME: usize = OPUS_FRAME_SAMPLES as usize;

    /// 相位连续的 440 Hz 正弦，幅度 0.5（写进 Opus 时比噪声更容易看清
    /// "重建出来的是不是原来那一帧"）
    fn tone_frame(first: usize) -> Vec<f32> {
        (0..FRAME)
            .map(|i| {
                let t = (first + i) as f32 / 48_000.0;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5
            })
            .collect()
    }

    /// 低码率 SILK + `packet_loss_perc > 0` + in-band FEC 才会真的塞 LBRR；
    /// 32 kbps 单声道 10 ms 是这条路径最容易命中的配置。
    fn encode_three_frames(inband_fec: bool) -> [Vec<u8>; 3] {
        let mut enc = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
            .expect("create encoder");
        enc.set_bitrate(Bitrate::BitsPerSecond(32_000))
            .expect("bitrate");
        enc.set_packet_loss_perc(20).expect("loss perc");
        enc.set_inband_fec(inband_fec).expect("inband fec");
        std::array::from_fn(|i| {
            let mut buf = vec![0u8; 1275];
            let n = enc
                .encode_float(&tone_frame(i * FRAME), &mut buf)
                .expect("encode");
            buf.truncate(n);
            buf
        })
    }

    fn mean_abs_err(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / a.len() as f32
    }

    /// 核心断言：丢帧后用下一个包的 LBRR 重建，比 PLC 外推更接近真实那一帧。
    #[test]
    fn fec_rebuilds_the_lost_frame_closer_than_plc() {
        let [f1, f2, f3] = encode_three_frames(true);

        let mut reference = OpusDecoder::new().expect("decoder");
        assert!(reference.decode(&f1).is_some());
        let n = reference.decode(&f2).expect("参考帧应能解码");
        let truth = reference.pcm_mut(n).to_vec();

        let mut fec = OpusDecoder::new().expect("decoder");
        assert!(fec.decode(&f1).is_some());
        let n = fec.decode_fec(&f3).expect("带内 FEC 应能重建上一帧");
        assert_eq!(n, FRAME, "FEC 重建也必须是固定 10 ms");
        let fec_pcm = fec.pcm_mut(n).to_vec();

        let mut plc = OpusDecoder::new().expect("decoder");
        assert!(plc.decode(&f1).is_some());
        let n = plc.conceal().expect("PLC 应产出一帧");
        let plc_pcm = plc.pcm_mut(n).to_vec();

        let fec_err = mean_abs_err(&fec_pcm, &truth);
        let plc_err = mean_abs_err(&plc_pcm, &truth);
        assert!(
            fec_err < plc_err,
            "FEC 应比 PLC 更接近原始帧：fec={fec_err:.4} plc={plc_err:.4}"
        );
    }

    /// FEC 只是"重建",不能吃掉那个包：之后它还要被正常解码一次，否则
    /// 解码器状态与发送端错位（后续每一帧都会失真）。
    #[test]
    fn decode_fec_keeps_the_decoder_in_step_with_the_next_packet() {
        let [f1, f2, f3] = encode_three_frames(true);
        let mut dec = OpusDecoder::new().expect("decoder");
        assert!(dec.decode(&f1).is_some());
        assert_eq!(dec.decode_fec(&f3), Some(FRAME));
        assert_eq!(dec.decode(&f3), Some(FRAME), "同一个包之后必须还能正常解码");
        // 再往后解一帧也正常（真包 / 无 LBRR 都不会让状态坏掉）
        assert_eq!(dec.decode_fec(&f2), Some(FRAME));
    }

    /// 发送端没开 in-band FEC 时包里没有 LBRR，libopus 自动退化成 PLC：
    /// 这条路径不会比 `conceal()` 更差，所以调用方不需要先知道对端有没有开。
    #[test]
    fn decode_fec_without_lbrr_falls_back_to_plc() {
        let [f1, f2, _f3] = encode_three_frames(false);
        let mut dec = OpusDecoder::new().expect("decoder");
        assert!(dec.decode(&f1).is_some());
        assert_eq!(dec.decode_fec(&f2), Some(FRAME));
        // 空包是"连兜底都没有"的情形：调用方据此走静音
        assert_eq!(dec.decode_fec(&[]), None);
    }

    #[test]
    fn conceal_on_fresh_decoder_is_one_frame() {
        let mut dec = OpusDecoder::new().expect("create decoder");
        assert_eq!(dec.conceal(), Some(OPUS_FRAME_SAMPLES as usize));
    }
}
