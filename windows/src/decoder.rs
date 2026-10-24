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

    /// 丢包隐藏：请求恰好一帧 10 ms。
    ///
    /// 返回实际生成的采样数；不足 480 时由调用方补静音。**不得**一次请求 120 ms
    /// 后当一帧播放。
    pub fn conceal(&mut self) -> Option<usize> {
        let signals = MutSignals::try_from(self.pcm_buf.as_mut_slice()).ok()?;
        match self.decoder.decode_float(None, signals, false) {
            Ok(n) if n > 0 => {
                self.last_n = n.min(MAX_FRAME_SAMPLES);
                Some(self.last_n)
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
