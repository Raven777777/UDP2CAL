//! 诊断：Windows 反向编码器在 Bitrate::Max（自动码率）下的包尺寸行为
//! 手机自动码率 → CONNECT 携带 bitrate=0 → 反向编码器 Bitrate::Max
//! 编码输出 > protocol::MAX_PAYLOAD(1472) 的帧会被 capture.rs 静默丢弃 → 手机端无声

use audiopus::{coder::Encoder, Application, Bandwidth, Bitrate, Channels, SampleRate};

#[test]
fn reverse_encoder_max_bitrate_packet_sizes() {
    let mut enc = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
        .expect("create encoder");

    // 与 capture.rs apply_config 自动码率路径完全一致
    enc.set_bitrate(Bitrate::Max).unwrap();
    enc.set_bandwidth(Bandwidth::Fullband).unwrap();
    enc.set_vbr(true).unwrap();
    enc.set_dtx(false).unwrap();
    enc.set_inband_fec(false).unwrap();
    enc.set_packet_loss_perc(0).unwrap();
    enc.set_vbr_constraint(false).unwrap();
    enc.set_complexity(3).unwrap();
    let _ = enc.set_dred_duration(0);

    // 10ms 立体声帧：480 帧/声道
    let frame: Vec<i16> = (0..960)
        .map(|i| {
            let t = i as f32 / 48000.0;
            // 语音+音乐混合能量，逼出 VBR 峰值
            let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.6
                + (t * 3000.0 * std::f32::consts::TAU).sin() * 0.3
                + (t * 33.0 * std::f32::consts::TAU).sin() * 0.4;
            (s * 32767.0) as i16
        })
        .collect();

    let mut out = vec![0u8; 4000];
    let mut sizes = Vec::new();
    let mut errors = 0;
    let mut over_1472 = 0usize;
    let mut over_1500 = 0usize;

    for n in 0..600 {
        match enc.encode(&frame, &mut out) {
            Ok(sz) => {
                sizes.push(sz);
                if sz > 1472 {
                    over_1472 += 1;
                }
                if sz > 1500 {
                    over_1500 += 1;
                }
            }
            Err(e) => {
                errors += 1;
                if errors <= 3 {
                    eprintln!("frame {n}: encode error: {e:?}");
                }
            }
        }
    }

    sizes.sort_unstable();
    let total = sizes.len();
    println!("encoded={total} errors={errors}");
    if total > 0 {
        println!(
            "packet size: min={} p50={} p90={} p99={} max={}",
            sizes[0],
            sizes[total / 2],
            sizes[total * 9 / 10],
            sizes[total * 99 / 100],
            sizes[total - 1]
        );
    }
    println!("over 1472 (协议丢弃): {over_1472}, over 1500 (编码缓冲不足): {over_1500}");

    // ── 手机端等效解码验证：2ch/48k 解码器 + 20ms PCM 缓冲（opus_jni_decoder 语义）──
    let mut dec = audiopus::coder::Decoder::new(SampleRate::Hz48000, Channels::Stereo)
        .expect("create decoder");
    let mut pcm = vec![0.0f32; 960 * 2]; // 20ms 立体声
    let mut decode_errors = 0;
    for n in 0..sizes.len() {
        let packet = audiopus::packet::Packet::try_from(&out[..sizes[n]]).expect("packet");
        let sig = audiopus::MutSignals::try_from(&mut pcm[..]).expect("signals");
        if dec.decode_float(Some(packet), sig, false).is_err() {
            decode_errors += 1;
            if decode_errors <= 3 {
                eprintln!("frame {n}: decode error, size={}", sizes[n]);
            }
        }
    }
    println!("decode_errors={decode_errors}/{total}");

    // 断言：Max 模式下不应有超过协议上限的包（若有，反向串流就会丢帧）
    assert_eq!(errors, 0, "Bitrate::Max 下出现编码错误");
    assert_eq!(over_1500, 0, "存在 >1500 字节包：encode 缓冲不足会整帧失败");
    assert_eq!(over_1472, 0, "存在 >1472 字节包：会被协议层静默丢弃");
    assert_eq!(decode_errors, 0, "手机端等效解码器无法解码 Max 码率包");
}
