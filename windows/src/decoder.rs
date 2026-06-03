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
}

impl OpusDecoder {
    pub fn new() -> Result<Self, audiopus::Error> {
        let decoder_8k = Decoder::new(SampleRate::Hz8000, Channels::Mono)?;
        let decoder_12k = Decoder::new(SampleRate::Hz12000, Channels::Mono)?;
        let decoder_16k = Decoder::new(SampleRate::Hz16000, Channels::Mono)?;
        let decoder_24k = Decoder::new(SampleRate::Hz24000, Channels::Mono)?;
        let decoder_48k = Decoder::new(SampleRate::Hz48000, Channels::Mono)?;
        Ok(Self {
            decoder_8k,
            decoder_12k,
            decoder_16k,
            decoder_24k,
            decoder_48k,
            pcm_buf: vec![0.0f32; MAX_FRAME_SAMPLES],
            last_n: 0,
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
            protocol::SAMPLE_RATE_48K => &mut self.decoder_48k,
            _ => &mut self.decoder_48k,
        };

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

    /// 获取最近一次解码的PCM数据。返回(&[f32], 源采样率Hz)
    pub fn pcm_data(&self) -> &[f32] {
        &self.pcm_buf[..self.last_n]
    }
}
