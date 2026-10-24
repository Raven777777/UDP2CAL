// Opus解码器封装 - audiopus 0.3.0-rc.0
// 支持所有 Opus 标准采样率: 8k, 12k, 16k, 24k, 48k
use audiopus::coder::Decoder;
use audiopus::{Channels, SampleRate};
use audiopus::packet::Packet;
use audiopus::MutSignals;
use std::convert::TryFrom;
use crate::protocol;

/// 48000 Hz * 120ms (Opus最大帧长) = 5760 样本/声道
const MAX_FRAME_SAMPLES: usize = 5760;

pub struct OpusDecoder {
    decoder_8k: Decoder,
    decoder_12k: Decoder,
    decoder_16k: Decoder,
    decoder_24k: Decoder,
    decoder_48k: Decoder,
    pcm_buf: Vec<f32>,
    last_n: usize,
    /// 最近一次解码使用的采样率档位，PLC 沿用它（保持解码器状态连续）
    active_sr: u8,
}

/// Deep PLC 运行时开关：解码复杂度 ≥5 且库以 OPUS_DEEP_PLC 构建时启用
const DEEP_PLC_COMPLEXITY: u8 = 5;

impl OpusDecoder {
    pub fn new() -> Result<Self, audiopus::Error> {
        let decoder_8k = Decoder::new(SampleRate::Hz8000, Channels::Mono)?;
        let decoder_12k = Decoder::new(SampleRate::Hz12000, Channels::Mono)?;
        let decoder_16k = Decoder::new(SampleRate::Hz16000, Channels::Mono)?;
        let decoder_24k = Decoder::new(SampleRate::Hz24000, Channels::Mono)?;
        let decoder_48k = Decoder::new(SampleRate::Hz48000, Channels::Mono)?;
        // Opus 1.5+：解码器复杂度 ≥5 时启用 Deep PLC（库需 OPUS_DEEP_PLC 构建），
        // 代价约为高丢包率下 1% CPU；未编译该特性时此调用无效，经典 PLC 照常工作。
        for d in [&decoder_8k, &decoder_12k, &decoder_16k, &decoder_24k, &decoder_48k] {
            let _ = d.set_complexity(DEEP_PLC_COMPLEXITY);
        }
        Ok(Self {
            decoder_8k,
            decoder_12k,
            decoder_16k,
            decoder_24k,
            decoder_48k,
            pcm_buf: vec![0.0f32; MAX_FRAME_SAMPLES],
            last_n: 0,
            active_sr: protocol::SAMPLE_RATE_48K,
        })
    }

    /// 解码Opus包，返回(样本数, 采样率Hz)。失败返回None。
    pub fn decode(&mut self, sample_rate: u8, data: &[u8]) -> Option<(usize, u32)> {
        if data.is_empty() {
            return None;
        }
        let decoder = match sample_rate {
            protocol::SAMPLE_RATE_8K => &mut self.decoder_8k,
            protocol::SAMPLE_RATE_12K => &mut self.decoder_12k,
            protocol::SAMPLE_RATE_16K => &mut self.decoder_16k,
            protocol::SAMPLE_RATE_24K => &mut self.decoder_24k,
            _ => &mut self.decoder_48k,
        };
        self.active_sr = sample_rate;

        let packet = Packet::try_from(data).ok()?;
        let signals = MutSignals::try_from(self.pcm_buf.as_mut_slice()).ok()?;
        match decoder.decode_float(Some(packet), signals, false) {
            Ok(n) => {
                self.last_n = n;
                let sr_hz = protocol::sample_rate_to_hz(sample_rate);
                Some((n, sr_hz))
            }
            Err(_) => None,
        }
    }

    /// 丢包隐藏（PLC）：对当前活跃解码器请求一帧补偿。
    ///
    /// 库启用 Deep PLC 时由神经网络生成，否则走经典 PLC 外推。
    /// 返回(样本数, 采样率Hz)，无可隐藏状态时返回 None。
    pub fn conceal(&mut self) -> Option<(usize, u32)> {
        let sample_rate = self.active_sr;
        // 字段级离散借用：解码器与 pcm_buf 互不重叠，避免 &mut self 整体借用冲突
        let decoder = match sample_rate {
            protocol::SAMPLE_RATE_8K => &mut self.decoder_8k,
            protocol::SAMPLE_RATE_12K => &mut self.decoder_12k,
            protocol::SAMPLE_RATE_16K => &mut self.decoder_16k,
            protocol::SAMPLE_RATE_24K => &mut self.decoder_24k,
            _ => &mut self.decoder_48k,
        };
        let signals = MutSignals::try_from(self.pcm_buf.as_mut_slice()).ok()?;
        match decoder.decode_float(None, signals, false) {
            Ok(n) if n > 0 => {
                self.last_n = n;
                Some((n, protocol::sample_rate_to_hz(sample_rate)))
            }
            _ => None,
        }
    }

    /// 获取最近一次解码的PCM数据。返回(&[f32], 源采样率Hz)
    pub fn pcm_data(&self) -> &[f32] {
        &self.pcm_buf[..self.last_n]
    }
}
