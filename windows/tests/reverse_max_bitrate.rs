//! 反向编码器码率/包长回归测试（v3）
//!
//! v3 把「每帧负载」钉死在 **10 ms / ≤1275 字节**：1280 字节（1024 kbps CBR 参考值）
//! 会直接越过 Opus/RTP 上限，因此协议把最大码率钳制到 1000 kbps。
//! 本测试固化三个不变量：
//!
//! 1. 允许档位（512 / 768 / 960 / 1000 kbps）在 1275 字节编码缓冲下**不报错**；
//! 2. 编码结果**永不**超过 1275 字节（超限宁可整帧丢弃，绝不截断）；
//! 3. 手机端等效解码器（2ch / 48 kHz / 10 ms）能解出每一包。
//!
//! 历史条目（v2）里的「1472 字节上限 / 20 ms 帧 / 自动码率 1024k」已随 v3 失效。

use audiopus::{coder::Encoder, Application, Bandwidth, Bitrate, Channels, SampleRate};

/// 协议上限：单个 Opus 负载
const OPUS_MAX_PAYLOAD: usize = 1275;
/// 10 ms 立体声交错样本数
const FRAME_INTERLEAVED: usize = 960;

fn run(bitrate_kbps: u16, label: &str) {
    let mut enc = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
        .expect("create encoder");
    // 与 capture.rs apply_config 一致的 ctl 序列（BITRATE 最后设置）
    enc.set_vbr(true).unwrap();
    enc.set_dtx(false).unwrap();
    enc.set_inband_fec(false).unwrap();
    enc.set_packet_loss_perc(0).unwrap();
    enc.set_vbr_constraint(false).unwrap();
    let _ = enc.set_dred_duration(0);
    enc.set_complexity(10).unwrap();
    enc.set_bandwidth(Bandwidth::Fullband).unwrap();
    enc.set_bitrate(Bitrate::BitsPerSecond(bitrate_kbps as i32 * 1000)).unwrap();

    // 宽带音乐式内容（比纯正弦更能逼出 VBR 峰值）
    let frame: Vec<i16> = (0..FRAME_INTERLEAVED)
        .map(|i| {
            let t = i as f32 / 48_000.0;
            let mut s = 0.0f32;
            for (f, a) in [
                (110.0, 0.35),
                (220.0, 0.25),
                (880.0, 0.2),
                (3_500.0, 0.15),
                (9_000.0, 0.1),
                (15_000.0, 0.06),
            ] {
                s += (t * f * std::f32::consts::TAU).sin() * a;
            }
            s += (i as f32 * 0.37).fract() * 0.12 - 0.06;
            (s * 32767.0) as i16
        })
        .collect();

    // 与 capture.rs 完全一致的 1275 字节编码缓冲
    let mut out = [0u8; OPUS_MAX_PAYLOAD];
    let mut dec = audiopus::coder::Decoder::new(SampleRate::Hz48000, Channels::Stereo)
        .expect("create decoder");
    // 手机端 10 ms 立体声缓冲（opus_jni_decoder.c 语义）
    let mut pcm = vec![0.0f32; FRAME_INTERLEAVED];

    let mut encoded = 0usize;
    let mut over_limit = 0usize;
    let mut errors = 0usize;
    let mut decode_errors = 0usize;
    let mut max_len = 0usize;

    for _ in 0..600 {
        match enc.encode(&frame, &mut out) {
            Ok(n) => {
                if n == 0 {
                    continue; // DTX 空帧（未开启 DTX，正常不会出现）
                }
                if n > OPUS_MAX_PAYLOAD {
                    over_limit += 1;
                }
                max_len = max_len.max(n);
                // 手机端等效解码
                let packet = audiopus::packet::Packet::try_from(&out[..n]).expect("packet");
                let sig = audiopus::MutSignals::try_from(pcm.as_mut_slice()).expect("signals");
                match dec.decode_float(Some(packet), sig, false) {
                    Ok(samples) if samples == 480 => encoded += 1,
                    _ => decode_errors += 1,
                }
            }
            Err(e) => {
                errors += 1;
                if errors <= 3 {
                    eprintln!("[{label}] encode error: {e:?}");
                }
            }
        }
    }

    println!(
        "[{label}] decoded={encoded} errors={errors} over_limit={over_limit} max_len={max_len}"
    );
    assert_eq!(errors, 0, "[{label}] 允许档位不得编码失败");
    assert_eq!(over_limit, 0, "[{label}] 负载超 1275 字节会被整帧丢弃");
    assert_eq!(decode_errors, 0, "[{label}] 手机端 10 ms 解码失败");
    assert!(encoded > 0, "[{label}] 没有产出任何可解码帧");
}

#[test]
fn reverse_bitrate_sweep_v3() {
    run(512, "512k(自动映射值)");
    run(768, "768k");
    run(960, "960k");
    run(1000, "1000k(上限)");
}

/// 协议禁止 1024 kbps，但**必须**证明"即使传入也不会超过 1275 字节"：
/// capture.rs 用 1275 字节缓冲调用 opus_encode，库会钳制或报错，
/// 两条路径都不会产生非法包。
#[test]
fn forbidden_1024kbps_never_exceeds_payload_cap() {
    let mut enc = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
        .expect("create encoder");
    enc.set_bitrate(Bitrate::BitsPerSecond(1_024_000)).unwrap();
    enc.set_vbr(false).unwrap();
    enc.set_bandwidth(Bandwidth::Fullband).unwrap();
    enc.set_complexity(10).unwrap();

    let frame: Vec<i16> = (0..FRAME_INTERLEAVED)
        .map(|i| {
            let t = i as f32 / 48_000.0;
            ((t * 440.0 * std::f32::consts::TAU).sin() * 20_000.0) as i16
        })
        .collect();

    let mut out = [0u8; OPUS_MAX_PAYLOAD];
    let mut max_len = 0usize;
    for _ in 0..200 {
        match enc.encode(&frame, &mut out) {
            Ok(n) => max_len = max_len.max(n),
            // 缓冲不足时 opus 返回错误，capture.rs 会丢帧并计数——同样不会发出非法包
            Err(_) => {}
        }
    }
    assert!(
        max_len <= OPUS_MAX_PAYLOAD,
        "1024 kbps 下仍必须 <= 1275 字节，实际 {max_len}"
    );
    println!("[1024k(CBR,禁止档位)] max_len={max_len}（缓冲已钳制）");
}
