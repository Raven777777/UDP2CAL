//! 反向编码器码率回归测试（真机问题存档）：
//!
//! 自动码率原映射 `Bitrate::Max`，真机上反向串流无声（600/700/1024 kbps 正常，
//! 2048k 与 Max 无声）。定版：自动码率映射 1024 kbps（1280 B/10ms 帧，1472
//! 协议上限内留余量）。Max 在离线环境无法复现问题（恒定 638B），疑似真机
//! 链路对码率/包型有额外约束，详见 docs/CHANGELOG.md 2026-10-24 条目。
//!
//! 本测试固化两个不变量：
//! 1. Max / 2048k / 1024k / 960k 下编码不报错、包不超 1472 协议上限；
//! 2. 手机端等效解码器（2ch/48k/20ms 缓冲）可解码全部包。

use audiopus::{coder::Encoder, Application, Bandwidth, Bitrate, Channels, SampleRate};

fn run(bitrate: Bitrate, label: &str) {
    let mut enc = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
        .expect("create encoder");
    // 与 capture.rs apply_config 一致的 ctl 序列
    enc.set_bitrate(bitrate).unwrap();
    enc.set_bandwidth(Bandwidth::Fullband).unwrap();
    enc.set_vbr(true).unwrap();
    enc.set_dtx(false).unwrap();
    enc.set_inband_fec(false).unwrap();
    enc.set_packet_loss_perc(0).unwrap();
    enc.set_vbr_constraint(false).unwrap();
    enc.set_complexity(10).unwrap(); // 手机端默认复杂度
    let _ = enc.set_dred_duration(0);

    // 宽带音乐式内容（比纯正弦更能逼出 VBR 峰值）
    let frame: Vec<i16> = (0..960)
        .map(|i| {
            let t = i as f32 / 48000.0;
            let mut s = 0.0f32;
            for (f, a) in [(110.0, 0.35), (220.0, 0.25), (880.0, 0.2), (3_500.0, 0.15),
                           (9_000.0, 0.1), (15_000.0, 0.06)] {
                s += (t * f * std::f32::consts::TAU).sin() * a;
            }
            s += (i as f32 * 0.37).fract() * 0.12 - 0.06;
            (s * 32767.0) as i16
        })
        .collect();

    // 与 capture.rs 完全一致的 1500 字节编码缓冲
    let mut out = [0u8; 1500];
    let (mut over_1472, mut errors, mut total) = (0usize, 0usize, 0usize);
    for _ in 0..600 {
        match enc.encode(&frame, &mut out) {
            Ok(n) => {
                total += 1;
                if n > 1472 { over_1472 += 1; }
            }
            Err(e) => { errors += 1; eprintln!("[{label}] encode error: {e:?}"); }
        }
    }

    // 手机端等效解码验证（包长逐帧可能不同，边编码边解码）
    let mut dec = audiopus::coder::Decoder::new(SampleRate::Hz48000, Channels::Stereo)
        .expect("create decoder");
    let mut pcm = vec![0.0f32; 960 * 2]; // 20ms 立体声（opus_jni_decoder 语义）
    let mut decode_errors = 0;
    for _ in 0..600 {
        let n = enc.encode(&frame, &mut out).expect("encode");
        if n == 0 { continue; }
        let packet = audiopus::packet::Packet::try_from(&out[..n]).expect("packet");
        let sig = audiopus::MutSignals::try_from(&mut pcm[..]).expect("signals");
        if dec.decode_float(Some(packet), sig, false).is_err() {
            decode_errors += 1;
        }
    }

    println!("[{label}] encoded={total} errors={errors} over1472={over_1472} decode_errors={decode_errors}");
    assert_eq!(errors, 0, "[{label}] 编码错误");
    assert_eq!(over_1472, 0, "[{label}] 存在超 1472 协议上限的包，会被 capture.rs 整帧丢弃");
    assert_eq!(decode_errors, 0, "[{label}] 手机端等效解码失败");
}

#[test]
fn reverse_bitrate_sweep() {
    run(Bitrate::BitsPerSecond(1_024_000), "1024k(定版自动)");
    run(Bitrate::BitsPerSecond(960_000), "960k");
    run(Bitrate::BitsPerSecond(2_048_000), "2048k(超限对照)");
    run(Bitrate::Max, "Max(历史默认,存档)");
}
