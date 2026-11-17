//! JitterBuffer 策略基准（hyperfine 用）。
//!
//! 模拟接收端循环：按 100 pps 设备节拍消费，每 tick 先插入到点的 RTP 包，
//! 再跑解码循环（ring_ms < DECODE_LIMIT_MS 就 pull，Wait 即停），设备每 tick
//! 消费一帧。衡量 JB 策略（插入/查找/PLC 决策/hold 状态机）的纯 CPU 开销，
//! 与真实 Opus 解码、网络无关。
//!
//! 用法：
//! ```text
//! jb_bench --scenario <inorder|reorder|loss1|loss5|burst> [--frames N] [--seed S]
//! ```
//! `UDP2CAL_PLC_HOLD_MS` 环境变量对 hold 路径生效（与生产一致）。
use std::time::Instant;

use udp2cal_protocol::rtp::{JitterBuffer, Pull, RtpPacket, DECODE_LIMIT_MS, FRAME_SAMPLES};

fn main() {
    let mut scenario = String::from("inorder");
    let mut frames: usize = 3000; // 30 s
    let mut seed: u64 = 0x243F6A8885A308D3;
    let mut debug = false;
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
            "--seed" => {
                i += 1;
                seed = argv[i].parse().expect("--seed <N>");
            }
            "--hold" => {
                i += 1;
                let ms: u32 = argv[i].parse().expect("--hold <ms>");
                // plc_hold_ms() 用 OnceLock 缓存，首次 pull 前设置即生效
                // SAFETY: 单线程启动阶段，且 OnceLock 尚未初始化
                unsafe { std::env::set_var("UDP2CAL_PLC_HOLD_MS", ms.to_string()) };
            }
            "--debug" => debug = true,
            other => panic!("unknown arg {other}"),
        }
        i += 1;
    }

    // 到达调度：frame i 在 tick arrival[i] 到点（默认 i）；dropped[i] = 该包被丢
    let mut arrival: Vec<usize> = (0..frames).collect();
    let mut dropped = vec![false; frames];
    let mut rng = XorShift::new(seed);
    let lens: Vec<usize> = (0..frames).map(|_| 60 + (rng.next() % 80) as usize).collect(); // 32 kbps 级别的载荷尺寸

    match scenario.as_str() {
        "inorder" => {}
        "reorder" => {
            for pair in 0..(frames / 2) {
                let a = pair * 2;
                arrival[a] = a + 1;
                arrival[a + 1] = a;
            }
        }
        "loss1" => {
            for d in dropped.iter_mut() {
                *d = rng.next().is_multiple_of(100);
            }
        }
        "loss5" => {
            for d in dropped.iter_mut() {
                *d = rng.next().is_multiple_of(20);
            }
        }
        "burst" => {
            // 每 5 s 一次 300 ms 突发丢（对齐 PLC_HOLE_MAX_MS）
            let mut base = 500;
            while base + 30 < frames {
                for d in &mut dropped[base..base + 30] {
                    *d = true;
                }
                base += 1500;
            }
        }
        other => panic!("unknown scenario {other}"),
    }

    let mut jb = JitterBuffer::new();
    let mut out = vec![0u8; 1500];
    let mut seq: u16 = 100;
    let mut ts: u32 = 4800;
    let mut ring_ms: u32 = 0;
    let mut inserted = 0usize;
    let mut consumed: u64 = 0;
    let mut plc_frames: u64 = 0;
    let mut silence_frames: u64 = 0;

    let t0 = Instant::now();
    for tick in 0..frames {
        while inserted < frames && arrival[inserted] <= tick {
            // 真实发送端时间戳/序号连续推进，与丢包无关
            let seq_i = seq;
            let ts_i = ts;
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(FRAME_SAMPLES);
            if !dropped[inserted] {
                let payload = vec![0u8; lens[inserted]];
                let pkt = RtpPacket {
                    marker: false,
                    payload_type: 96,
                    seq: seq_i,
                    timestamp: ts_i,
                    ssrc: 1,
                    payload: &payload,
                };
                let _ = jb.insert(&pkt);
            }
            inserted += 1;
        }
        // 解码循环：ring 未到 DECODE_LIMIT 就一直拉
        loop {
            if ring_ms >= DECODE_LIMIT_MS {
                break;
            }
            match jb.pull(ring_ms, 0, &mut out) {
                Pull::Packet { .. } => {
                    ring_ms += 10;
                    consumed += 1;
                }
                Pull::Plc { .. } => {
                    ring_ms += 10;
                    consumed += 1;
                    plc_frames += 1;
                }
                Pull::Silence { .. } => {
                    ring_ms += 10;
                    consumed += 1;
                    silence_frames += 1;
                }
                Pull::Wait => break,
            }
            if debug {
                eprintln!(
                    "tick={} pull={:?} ring={} next_ts={} stats: resync={} inserted={} late={} dup={} overflow={}",
                    tick, "?", ring_ms, jb.next_ts(), jb.stats.resyncs, jb.stats.inserted, jb.stats.late, jb.stats.duplicate, jb.stats.overflow
                );
            }
        }
        // 设备消费一帧
        ring_ms = ring_ms.saturating_sub(10);
    }
    let dt = t0.elapsed();

    let st = jb.stats;
    eprintln!(
        "scenario={} frames={} wall={:.4?} ms consumed={} plc={} silence={} inserted={} late={} dup={} overflow={} resync={} hold={}",
        scenario,
        frames,
        dt.as_secs_f64() * 1000.0,
        consumed,
        plc_frames,
        silence_frames,
        st.inserted,
        st.late,
        st.duplicate,
        st.overflow,
        st.resyncs,
        st.hold_frames,
    );
}

struct XorShift {
    s: u64,
}

impl XorShift {
    fn new(seed: u64) -> Self {
        Self {
            s: seed | 1,
        }
    }
    fn next(&mut self) -> u64 {
        let mut x = self.s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.s = x;
        x
    }
}
