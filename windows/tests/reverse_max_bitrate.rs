//! 复刻真实应用条件验证：encode_out=1500 字节（capture.rs 实际缓冲）
//! Max 模式是否产包超 1472 → 被 `n <= MAX_PAYLOAD` 过滤器整帧丢弃 → 无声

use audiopus::{coder::Encoder, Application, Bandwidth, Bitrate, Channels, SampleRate};

fn run(bitrate: Bitrate, label: &str) {
    let mut enc = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
        .expect("create encoder");
    enc.set_bitrate(bitrate).unwrap();
    enc.set_bandwidth(Bandwidth::Fullband).unwrap();
    enc.set_vbr(true).unwrap();
    enc.set_dtx(false).unwrap();
    enc.set_inband_fec(false).unwrap();
    enc.set_packet_loss_perc(0).unwrap();
    enc.set_vbr_constraint(false).unwrap();
    enc.set_complexity(10).unwrap();
    let _ = enc.set_dred_duration(0);

    // 真实环回音频特征：宽带音乐内容
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

    // 关键：与 capture.rs 完全一致的 1500 字节缓冲
    let mut out = [0u8; 1500];
    let (mut over_1472, mut errors, mut total, mut sum) = (0usize, 0usize, 0usize, 0u64);
    for _ in 0..600 {
        match enc.encode(&frame, &mut out) {
            Ok(n) => {
                total += 1;
                sum += n as u64;
                if n > 1472 { over_1472 += 1; }
            }
            Err(_) => errors += 1,
        }
    }
    let avg = if total > 0 { sum / total as u64 } else { 0 };
    println!("[{label}] 编码 {total} 帧, 错误 {errors}, 平均 {avg} B/帧 ({} kbps), 超1472: {over_1472}",
        avg * 8 * 100 / 1000);
}

#[test]
fn reverse_real_buffer_sweep() {
    run(Bitrate::Max, "Max(默认自动)");
    run(Bitrate::BitsPerSecond(2_048_000), "2048k");
    run(Bitrate::BitsPerSecond(1_024_000), "1024k");
    run(Bitrate::BitsPerSecond(960_000), "960k");
}
