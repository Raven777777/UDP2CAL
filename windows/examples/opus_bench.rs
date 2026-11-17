//! Opus 解码/PLC/FEC 单帧成本基准（hyperfine 用）。
//!
//! 自包含：直接用 audiopus，不复用应用里的 OpusDecoder 封装，
//! 但保持与生产一致的口径——48 kHz / Mono / 固定 10 ms / 480 采样。
//! 编码只作为生成真实码流的手段，**在计时区之外**完成。
//!
//! 用法：`opus_bench --scenario <enc|dec|plc|fec> [--frames N] [--bitrate BPS]`
//! 输出（stdout）一行总耗时与每帧均值；hyperfine 比较各 scenario 的 wall time。
use std::time::Instant;

use audiopus::coder::{Decoder, Encoder};
use audiopus::packet::Packet;
use audiopus::{Application, Bitrate, Channels, MutSignals, SampleRate};

const FRAME: usize = 480; // 10 ms @ 48 kHz

fn load_packets(path: &str) -> Result<Vec<Vec<u8>>, std::io::Error> {
    let raw = std::fs::read(path)?;
    let mut at = 0usize;
    let read_u32 = |raw: &[u8], at: &mut usize| -> u32 {
        let v = u32::from_le_bytes([raw[*at], raw[*at + 1], raw[*at + 2], raw[*at + 3]]);
        *at += 4;
        v
    };
    let count = read_u32(&raw, &mut at) as usize;
    let mut pkts = Vec::with_capacity(count);
    for _ in 0..count {
        let len = read_u32(&raw, &mut at) as usize;
        pkts.push(raw[at..at + len].to_vec());
        at += len;
    }
    Ok(pkts)
}

fn save_packets(path: &str, pkts: &[Vec<u8>]) -> Result<(), std::io::Error> {
    let mut out = Vec::new();
    out.extend_from_slice(&(pkts.len() as u32).to_le_bytes());
    for p in pkts {
        out.extend_from_slice(&(p.len() as u32).to_le_bytes());
        out.extend_from_slice(p);
    }
    std::fs::write(path, out)
}

fn main() {
    let mut scenario = String::from("dec");
    let mut frames: usize = 2000;
    let mut bitrate: i32 = 32_000;
    let mut complexity: u8 = 5; // ≥5 且库带 OPUS_DEEP_PLC 时启用 Deep PLC
    let mut packets_path: Option<String> = None;
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--scenario" => {
                i += 1;
                scenario = argv[i].clone();
            }
            "--frames" => {
                i += 1;
                frames = argv[i].parse().expect("--frames <N>");
            }
            "--bitrate" => {
                i += 1;
                bitrate = argv[i].parse().expect("--bitrate <bps>");
            }
            "--complexity" => {
                i += 1;
                complexity = argv[i].parse().expect("--complexity <0-10>");
            }
            "--packets" => {
                i += 1;
                packets_path = Some(argv[i].clone());
            }
            other => panic!("unknown arg {other}"),
        }
        i += 1;
    }

    let dec = || {
        let d = Decoder::new(SampleRate::Hz48000, Channels::Mono).expect("decoder");
        let _ = d.set_complexity(complexity); // ≥5 且 OPUS_DEEP_PLC → Deep PLC
        d
    };
    // 一口气的编码器状态按帧连续编码才有可比的码流：编码批在计时区之外。
    // 传 --packets 时首次生成后存盘，后续运行直接加载（编码 359 µs/帧，
    // 不缓存会把 hyperfine 的 wall time 淹没在准备成本里）。
    let encode_batch = |count: usize| -> Vec<Vec<u8>> {
        if let Some(path) = &packets_path
            && let Ok(pkts) = load_packets(path) {
                return pkts;
            }
        let mut e =
            Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).expect("encoder");
        e.set_bitrate(Bitrate::BitsPerSecond(bitrate)).expect("bitrate");
        e.set_packet_loss_perc(20).expect("loss perc");
        e.set_inband_fec(true).expect("inband fec");
        let mut buf = vec![0u8; 1275];
        let pkts: Vec<Vec<u8>> = (0..count)
            .map(|f| {
                let pcm: Vec<f32> = (0..FRAME)
                    .map(|k| {
                        let t = (f * FRAME + k) as f32 / 48_000.0;
                        (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5
                    })
                    .collect();
                let n = e.encode_float(&pcm, &mut buf).expect("encode");
                buf[..n].to_vec()
            })
            .collect();
        if let Some(path) = &packets_path {
            let _ = save_packets(path, &pkts);
        }
        pkts
    };

    let (dt, sink) = match scenario.as_str() {
        "enc" => {
            let t0 = Instant::now();
            let mut e = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
                .expect("encoder");
            e.set_bitrate(Bitrate::BitsPerSecond(bitrate)).expect("bitrate");
            e.set_packet_loss_perc(20).expect("loss perc");
            e.set_inband_fec(true).expect("inband fec");
            let mut buf = vec![0u8; 1275];
            let mut sink = 0.0f32;
            for f in 0..frames {
                let pcm: Vec<f32> = (0..FRAME)
                    .map(|k| {
                        let t = (f * FRAME + k) as f32 / 48_000.0;
                        (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5
                    })
                    .collect();
                let n = e.encode_float(&pcm, &mut buf).expect("encode");
                sink += buf[n - 1] as f32;
            }
            let dt = t0.elapsed();
            (dt, sink)
        }
        "dec" => {
            let packets = encode_batch(frames);
            let mut dec = dec();
            let mut pcm_out = vec![0.0f32; FRAME];
            let t0 = Instant::now();
            let mut sink = 0.0f32;
            for p in &packets {
                let n = dec
                    .decode_float(
                        Some(Packet::try_from(p.as_slice()).expect("pkt")),
                        MutSignals::try_from(pcm_out.as_mut_slice()).expect("sig"),
                        false,
                    )
                    .expect("decode");
                sink += pcm_out[n - 1];
            }
            (t0.elapsed(), sink)
        }
        "plc" => {
            // 1 帧真实解码打底（计时区外），之后连续 N 帧 conceal（模拟长洞 PLC）
            let packets = encode_batch(1);
            let mut dec = dec();
            let mut pcm_out = vec![0.0f32; FRAME];
            dec.decode_float(
                Some(Packet::try_from(packets[0].as_slice()).expect("pkt")),
                MutSignals::try_from(pcm_out.as_mut_slice()).expect("sig"),
                false,
            )
            .expect("prime decode");
            let t0 = Instant::now();
            let mut sink = 0.0f32;
            for _ in 0..frames {
                let n = dec
                    .decode_float(
                        None,
                        MutSignals::try_from(pcm_out.as_mut_slice()).expect("sig"),
                        false,
                    )
                    .expect("conceal");
                sink += pcm_out[(n - 1).min(FRAME - 1)];
            }
            (t0.elapsed(), sink)
        }
        "fec" => {
            // 逐帧 decode_fec（包内 LBRR 重建上一帧），模拟常态丢包的重建成本
            let packets = encode_batch(frames + 1);
            let mut dec = dec();
            let mut pcm_out = vec![0.0f32; FRAME];
            dec.decode_float(
                Some(Packet::try_from(packets[0].as_slice()).expect("pkt")),
                MutSignals::try_from(pcm_out.as_mut_slice()).expect("sig"),
                false,
            )
            .expect("prime");
            let t0 = Instant::now();
            let mut sink = 0.0f32;
            for p in &packets[1..] {
                let n = dec
                    .decode_float(
                        Some(Packet::try_from(p.as_slice()).expect("pkt")),
                        MutSignals::try_from(pcm_out.as_mut_slice()).expect("sig"),
                        true,
                    )
                    .expect("fec");
                sink += pcm_out[n - 1];
            }
            (t0.elapsed(), sink)
        }
        other => panic!("unknown scenario {other}"),
    };

    println!(
        "scenario={scenario} frames={frames} bitrate={bitrate} complexity={complexity} total_us={} us_per_frame={:.2} (sink={})",
        dt.as_micros(),
        dt.as_micros() as f64 / frames as f64,
        sink.is_finite(),
    );
}
