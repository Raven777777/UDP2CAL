//! 诊断：反向编码器 Bitrate::Max（自动码率）+ complexity 10 的包尺寸
//! 复刻 capture.rs apply_config 真实参数（手机端 complexity 默认 10）

use audiopus::{coder::Encoder, Application, Bandwidth, Bitrate, Channels, SampleRate};

fn run(complexity: u8, label: &str) {
    let mut enc = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
        .expect("create encoder");
    enc.set_bitrate(Bitrate::Max).unwrap();
    enc.set_bandwidth(Bandwidth::Fullband).unwrap();
    enc.set_vbr(true).unwrap();
    enc.set_dtx(false).unwrap();
    enc.set_inband_fec(false).unwrap();
    enc.set_packet_loss_perc(0).unwrap();
    enc.set_vbr_constraint(false).unwrap();
    enc.set_complexity(complexity).unwrap();
    let _ = enc.set_dred_duration(0);

    // 音乐式内容：多谐波 + 噪声底，比正弦更接近真实环回音频
    let frame: Vec<i16> = (0..960)
        .map(|i| {
            let t = i as f32 / 48000.0;
            let mut s = 0.0f32;
            for (f, a) in [(220.0, 0.3), (440.0, 0.2), (1_000.0, 0.15), (4_000.0, 0.1), (12_000.0, 0.05)] {
                s += (t * f * std::f32::consts::TAU).sin() * a;
            }
            s += (i as f32 * 0.37).fract() * 0.1 - 0.05; // 伪噪声底
            (s * 32767.0) as i16
        })
        .collect();

    let mut out = vec![0u8; 4000];
    let mut max_sz = 0usize;
    let mut over_1472 = 0usize;
    let mut first = 0usize;
    for _ in 0..300 {
        match enc.encode(&frame, &mut out) {
            Ok(n) => {
                if first == 0 { first = n; }
                max_sz = max_sz.max(n);
                if n > 1472 { over_1472 += 1; }
            }
            Err(e) => { println!("[{label}] encode error: {e:?}"); return; }
        }
    }
    println!("[{label}] first={first} max={max_sz} over1472={over_1472}/300");
}

#[test]
fn reverse_max_bitrate_sweep() {
    for c in [3u8, 6, 8, 10] {
        run(c, &format!("cplx{c}"));
    }
}
