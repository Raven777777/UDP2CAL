//! 网络层：发现/广播状态机与 UDP 接收流。

use crate::app::{Message, StatusInfo};
use crate::diag::{AUDIO_LEVEL_DB, RTP_RX_DROPPED, RTP_RX_PACKETS};
use crate::logf;
use crate::protocol;
use crate::session::{self, AudioInitState, RTP_IDLE_RESET_MS, Session};
use iced::futures::SinkExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

// ══════════════════════════════════════════════
// 广播状态机 (独立线程)
// ══════════════════════════════════════════════
// Ready 模式: 每 1 秒广播 TYPE_DISCOVER_REPLY 到 LAN
// Silent 模式: APP 停止 / DEVICE_BUSY 时停止广播

pub(crate) fn start_broadcast_state_machine(session: Arc<Session>) {
    std::thread::spawn(move || {
        let socket = match std::net::UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = socket.set_broadcast(true);
        let device_id = session.device_id();
        let mut buf = [0u8; protocol::CONTROL_HEADER_SIZE + protocol::CONTROL_MAX_PAYLOAD];
        loop {
            if session.running() && !session.p2p().is_busy() {
                let reply = protocol::DiscoverReply {
                    listen_port: session.listen_port() as u16,
                    device_name: session.device_name().to_string(),
                };
                let payload = reply.encode();
                if let Some(total) = protocol::build_control_to(
                    &mut buf,
                    0,
                    protocol::TYPE_DISCOVER_REPLY,
                    &device_id,
                    &payload,
                ) {
                    let _ =
                        socket.send_to(&buf[..total], ("255.255.255.255", protocol::DISCOVER_PORT));
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

// ══════════════════════════════════════════════
// 发现服务 (独立线程)
// ══════════════════════════════════════════════
pub(crate) fn start_broadcast_listener(session: Arc<Session>) {
    std::thread::spawn(move || {
        // 发现端口可能被短暂占用（旧实例退出、系统服务重启）。绑定失败
        // 就返回会让 Android 永远发现不了本机，而 UI 只会显示“等待连接”。
        // 每 5 s 重试，直到成功；原有限流逻辑不变。
        let socket = loop {
            match std::net::UdpSocket::bind(("0.0.0.0", protocol::DISCOVER_PORT)) {
                Ok(s) => break s,
                Err(e) => {
                    logf!("[Discovery] 绑定广播端口失败: {}（5 s 后重试）", e);
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        };
        let mut buf = [0u8; 1500];
        let my_id = session.device_id();
        // 简单令牌桶：同一源 IP 每秒最多应答 MAX_PER_SEC 次。
        // 发现请求 15B、应答 ~15B+主机名，无限制时可被用于 UDP 反射放大。
        let mut bucket: std::collections::HashMap<std::net::IpAddr, (u32, Instant)> =
            std::collections::HashMap::new();
        let mut last_cleanup = Instant::now();
        loop {
            let (amt, src) = match socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if last_cleanup.elapsed() > Duration::from_secs(30) {
                bucket.retain(|_, (_, t)| t.elapsed() < Duration::from_secs(5));
                last_cleanup = Instant::now();
            }
            if !session::allow_discovery_request(&mut bucket, src.ip(), Instant::now()) {
                continue;
            }

            let Some((hdr, payload)) = protocol::parse_control(&buf[..amt]) else {
                continue;
            };
            // 只接受空负载的 DISCOVER_REQ；Busy / 未启动一律不回
            if !session::is_valid_control_header(&hdr)
                || hdr.msg_type != protocol::TYPE_DISCOVER_REQ
                || !payload.is_empty()
            {
                continue;
            }
            if !session.running() || session.p2p().is_busy() {
                continue;
            }
            let reply = protocol::DiscoverReply {
                listen_port: session.listen_port() as u16,
                device_name: session.device_name().to_string(),
            };
            let body = reply.encode();
            if let Some(total) = protocol::build_control_to(
                &mut buf,
                0,
                protocol::TYPE_DISCOVER_REPLY,
                &my_id,
                &body,
            ) {
                let _ = socket.send_to(&buf[..total], src);
            }
        }
    });
}

/// `iced::Subscription::run_with` 要求数据可 `Hash`，但 `Session` 本身不实现
/// `Hash`（它含锁与线程句柄，也没有值语义）。这里用 `Arc` 的指针身份参与
/// 哈希：同一会话 + 同一监听地址构成一个订阅标识，重启 iced 时不会误复用。
pub(crate) struct ReceiverKey {
    pub(crate) session: Arc<Session>,
    pub(crate) listen_ip: String,
    pub(crate) listen_port: u32,
}

impl std::hash::Hash for ReceiverKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.listen_ip.hash(state);
        self.listen_port.hash(state);
        (Arc::as_ptr(&self.session) as usize).hash(state);
    }
}

/// 空闲计时只由**被接受**的 RTP 包刷新。
///
/// 被丢弃的包（如发送端重启后同 SSRC 的向后时间戳）若也刷新，`RTP_IDLE_RESET_MS`
/// 的 1 s 窗口永远不会到期，时间线就无法通过 reset 自愈。
fn rtp_activity_at(last: Option<Instant>, accepted: bool, now: Instant) -> Option<Instant> {
    if accepted {
        Some(now)
    } else {
        last
    }
}

///
/// `+ use<>` 让返回的 opaque 类型不捕获入参生命周期——否则它无法强制转换成
/// `for<'a> fn(&'a ReceiverKey) -> _`（流本身只持有 `Arc` 与 `String` 的所有权）。
pub(crate) fn receiver_stream(
    key: &ReceiverKey,
) -> impl iced::futures::Stream<Item = Message> + use<> {
    udp_receiver_stream(
        Arc::clone(&key.session),
        key.listen_ip.clone(),
        key.listen_port,
    )
}

pub(crate) fn udp_receiver_stream(
    session: Arc<Session>,
    listen_ip: String,
    listen_port: u32,
) -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(
        64,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            let bind_addr = format!("{}:{}", listen_ip, listen_port);
            // 用 `(host, port)` 元组解析，而不是把上面那个字符串拿去做 bind：
            // `format!("{}:{}")` 对 IPv6 会拼出 "::1:44044"（缺方括号，非法
            // socket 地址），而 UI 是接受 IPv6 监听地址的（app.rs 的校验
            // 只拦非法字符）。`ToSocketAddrs` 对 IPv4/IPv6/主机名三种形态
            // 都正确，`bind_addr` 只用于报错展示。
            // 端口已是 1..=65535（config::load 归一 + UI 校验），`as u16` 安全。
            let socket =
                match tokio::net::UdpSocket::bind((listen_ip.as_str(), listen_port as u16)).await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = output
                            .send(Message::EngineError(format!(
                                "监听 {bind_addr} 失败：{}（端口被占用或地址不可用）",
                                e.kind()
                            )))
                            .await;
                        return;
                    }
                };

            // 接收缓冲（借鉴 Sunshine 的 socket 调优）：把内核接收队列放大到
            // 1 MiB。Windows 默认只有 64 KiB 量级，Wi-Fi 一次漫游/重传退避就
            // 可能把突发直接丢掉；这里放大的是**内核队列**而不是应用层积压
            // ——循环每轮立即 recv，队列只用来吸收空口级突发，不引入延迟。
            const RECV_BUFFER_BYTES: usize = 1 << 20;
            if let Err(e) = socket2::SockRef::from(&socket).set_recv_buffer_size(RECV_BUFFER_BYTES)
            {
                logf!("[Net] 接收缓冲设置失败（{e}），继续使用系统默认值");
            }

            // 等待音频初始化完成（最多 3 秒；冷启动时音频服务可能较慢）
            let mut retries = 60;
            while session.audio_state() == AudioInitState::Pending && retries > 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                retries -= 1;
            }

            match session.audio_state() {
                AudioInitState::DecoderFailed => {
                    let _ = output
                        .send(Message::EngineError("Opus 解码器初始化失败".into()))
                        .await;
                    return;
                }
                AudioInitState::DeviceFailed => {
                    let _ = output
                        .send(Message::EngineError(
                            "音频设备初始化失败（请确认 VB-Cable 已安装后重启程序）".into(),
                        ))
                        .await;
                    return;
                }
                AudioInitState::Pending => {
                    // start_audio 已有 2.5 s 上限；3 s 后仍 Pending 说明初始化线程
                    // 卡在设备打开等路径，不能当作已连接放行。
                    let _ = output
                        .send(Message::EngineError(
                            "音频初始化超时（音频设备无响应），请检查音频服务后重新开始".into(),
                        ))
                        .await;
                    return;
                }
                AudioInitState::Ready => {}
            }

            session.reset_to_ready();
            session.wake_decoder();

            let mut buf = vec![0u8; 2048];
            let mut last_stats = Instant::now();
            let mut last_liveness = Instant::now();
            let mut byte_count: u64 = 0;
            // 最近一次被 JitterBuffer 接纳的 RTP 包到达时刻（空闲恢复判据，见 RTP_IDLE_RESET_MS）
            let mut last_rtp_at: Option<Instant> = None;

            loop {
                // 控制面保活检查必须**与流量无关**地周期性执行（规范 6.4：保活只看
                // 控制面，不得用"是否收到 RTP"判断连接存活）。历史实现把它放在
                // recv 超时分支里：只要 RTP 持续到达，检查就永不执行，绑定永不释放。
                if last_liveness.elapsed() >= Duration::from_millis(100) {
                    last_liveness = Instant::now();
                    if session.check_peer_timeout() {
                        // Disconnected 每次断连只发一次（check_peer_timeout 触发
                        // reset_to_ready 后续 tick 直接 false），不能无脑丢：先试
                        // 非阻塞投递，通道真满（UI 落后 ≥64 条）才阻塞等待。
                        if let Err(e) = output.try_send(Message::Disconnected) {
                            // 该消息一次性不能丢：满了才阻塞等待；UI 已退出时
                            // send 立刻报错被忽略
                            let _ = output.send(e.into_inner()).await;
                        }
                    }
                }

                let recv =
                    tokio::time::timeout(Duration::from_millis(200), socket.recv_from(&mut buf))
                        .await;

                let (len, src) = match recv {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => {
                        // Windows 上 UDP socket 会把上一次 send_to 收到的 ICMP 端口不可达
                        // 回灌成一次 WSAECONNRESET。这类错误不是致命的，但绝对不能
                        // 立刻重试——那会变成 100% CPU 的忙等。睡 10 ms 退避。
                        if session.should_log_control_reject() {
                            logf!("[UDP] recv_from 失败：{e}");
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    }
                    // 超时：下一轮继续做保活检查
                    Err(_) => continue,
                };

                match protocol::demux(&buf[..len]) {
                    protocol::Datagram::Control(data) => {
                        if let Some((hdr, payload)) = protocol::parse_control(data) {
                            session
                                .handle_control(&hdr, payload, &src, &socket, &mut output)
                                .await;
                        }
                    }
                    protocol::Datagram::Rtp(data) => {
                        let peer = session.p2p();
                        // 未绑定 / 源地址不符：一律丢弃（含绑定前后的迟到包）
                        if !peer.is_busy() || peer.addr != Some(src) {
                            RTP_RX_DROPPED.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let Ok(pkt) = protocol::rtp::parse(data) else {
                            RTP_RX_DROPPED.fetch_add(1, Ordering::Relaxed);
                            continue;
                        };
                        if pkt.ssrc != peer.forward_ssrc {
                            RTP_RX_DROPPED.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        // 网络线程只插入，不解码、不阻塞等待播放
                        // 空闲恢复：长时间无包后，本地已把播放头推到远超前于发送端，
                        // 必须先 reset 再按首包重新起播（否则恢复包全被当 late 丢弃）。
                        let idle_too_long = last_rtp_at
                            .map(|at| at.elapsed().as_millis() > RTP_IDLE_RESET_MS as u128)
                            .unwrap_or(false);
                        let accepted = {
                            let mut jb = session.jitter();
                            if idle_too_long {
                                jb.reset();
                            }
                            jb.insert(&pkt) != protocol::rtp::InsertResult::Dropped
                        };
                        if idle_too_long {
                            logf!(
                                "[RTP] 空闲超过 {}ms，已重置抖动缓冲并按首包重新起播",
                                RTP_IDLE_RESET_MS
                            );
                        }
                        last_rtp_at = rtp_activity_at(last_rtp_at, accepted, Instant::now());
                        if accepted {
                            byte_count += len as u64;
                            RTP_RX_PACKETS.fetch_add(1, Ordering::Relaxed);
                        }
                        session.wake_decoder();
                    }
                    protocol::Datagram::Drop => {
                        RTP_RX_DROPPED.fetch_add(1, Ordering::Relaxed);
                    }
                }

                if last_stats.elapsed() > Duration::from_millis(500) {
                    let elapsed = last_stats.elapsed().as_secs_f32();
                    let kbps = if elapsed > 0.0 {
                        (byte_count * 8) as f32 / elapsed / 1000.0
                    } else {
                        0.0
                    };
                    let level_db = f32::from_bits(AUDIO_LEVEL_DB.load(Ordering::Relaxed));
                    // 半秒统计窗口结束：窗口字节数无条件归零（没有 RTP 时下个窗口
                    // 从 0 重新累计，DTX 静音也走这里；包计数不需要单独保存，
                    // 全局的 RTP_RX_PACKETS 已经在统计同一件事）。
                    byte_count = 0;
                    last_stats = Instant::now();
                    // 每 500 ms 重发：满了丢本条，下一条自然补上，不倒灌接收任务
                    let _ = output.try_send(Message::StatusUpdate(StatusInfo {
                        bitrate_kbps: kbps,
                        level_db,
                    }));
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::rtp_activity_at;
    use std::time::{Duration, Instant};

    #[test]
    fn only_accepted_rtp_updates_idle_timestamp() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_millis(500);

        assert_eq!(rtp_activity_at(None, false, t0), None);
        assert_eq!(rtp_activity_at(Some(t0), false, later), Some(t0));
        assert_eq!(rtp_activity_at(None, true, t0), Some(t0));
        assert_eq!(rtp_activity_at(Some(t0), true, later), Some(later));
    }
}
