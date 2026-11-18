use super::*;
use std::time::{Duration, Instant};

#[test]
fn tail_hold_plc_must_not_peek_fec() {
    assert!(!audio::fec_peek_allowed(0, 0));
    assert!(audio::fec_peek_allowed(
        0,
        protocol::rtp::FRAME_SAMPLES as u64
    ));
}

#[test]
fn discovery_limiter_is_bounded_and_resets_per_second() {
    let mut bucket = std::collections::HashMap::new();
    let source = "192.0.2.1".parse().unwrap();
    let t0 = Instant::now();
    for _ in 0..DISCOVERY_RATE_PER_SOURCE {
        assert!(allow_discovery_request(&mut bucket, source, t0));
    }
    assert!(!allow_discovery_request(&mut bucket, source, t0));
    assert!(allow_discovery_request(
        &mut bucket,
        source,
        t0 + Duration::from_secs(1)
    ));

    let mut full = std::collections::HashMap::new();
    for i in 0..DISCOVERY_MAX_TRACKED_SOURCES {
        let ip = std::net::IpAddr::V4(std::net::Ipv4Addr::from(i as u32 + 1));
        assert!(allow_discovery_request(&mut full, ip, t0));
    }
    let new_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 1));
    assert!(!allow_discovery_request(&mut full, new_ip, t0));
    assert_eq!(full.len(), DISCOVERY_MAX_TRACKED_SOURCES);
    // 已在表中的源不受新源上限影响，避免一次伪造洪泛把正常设备也锁死。
    assert!(allow_discovery_request(
        &mut full,
        std::net::IpAddr::V4(std::net::Ipv4Addr::from(1)),
        t0 + Duration::from_secs(1)
    ));
}

#[test]
fn control_header_rejects_all_zero_device_id() {
    let valid_id = [1u8; protocol::DEVICE_ID_SIZE];
    let valid = protocol::ControlHeader::new(protocol::TYPE_CONNECT, 15, valid_id);
    assert!(is_valid_control_header(&valid));

    let zero_id =
        protocol::ControlHeader::new(protocol::TYPE_CONNECT, 15, [0u8; protocol::DEVICE_ID_SIZE]);
    assert!(!is_valid_control_header(&zero_id));

    let unknown_type = protocol::ControlHeader::new(protocol::TYPE_DATA, 0, valid_id);
    assert!(!is_valid_control_header(&unknown_type));
}

#[test]
fn v3_binding_requires_exact_source_endpoint() {
    let id = [7u8; protocol::DEVICE_ID_SIZE];
    let original: SocketAddr = "192.0.2.10:50000".parse().unwrap();
    let mut peer = P2pState::new();
    peer.state = DEVICE_BUSY;
    peer.device_id = id;
    peer.addr = Some(original);

    assert!(is_bound_peer_source(&peer, id, original));
    assert!(!is_bound_peer_source(
        &peer,
        id,
        "192.0.2.11:50000".parse().unwrap()
    ));
    assert!(!is_bound_peer_source(
        &peer,
        id,
        "192.0.2.10:50001".parse().unwrap()
    ));
    assert!(!is_bound_peer_source(
        &peer,
        [8u8; protocol::DEVICE_ID_SIZE],
        original
    ));
}

#[test]
fn reverse_restart_is_immediate_when_stale_but_dead_retries_are_limited() {
    let t0 = Instant::now();
    let min = Duration::from_secs(2);

    // 地址/SSRC/端口真实变化：不受限流影响，立即重启。
    assert!(should_restart_reverse(
        true,
        true,
        Some(t0 + Duration::from_millis(100)),
        t0 + Duration::from_millis(200),
        min
    ));
    // 发送线程健康：无需重启。
    assert!(!should_restart_reverse(false, false, None, t0, min));
    // 死线程首次发现：立即重启。
    assert!(should_restart_reverse(false, true, None, t0, min));
    // 距上次重启不足 2 s：拒绝。
    assert!(!should_restart_reverse(
        false,
        true,
        Some(t0),
        t0 + Duration::from_millis(1999),
        min
    ));
    // 满 2 s：允许重试。
    assert!(should_restart_reverse(
        false,
        true,
        Some(t0),
        t0 + Duration::from_secs(2),
        min
    ));
}

#[test]
fn pairing_prompts_only_until_approved_or_denied() {
    let ip: std::net::IpAddr = "192.0.2.10".parse().unwrap();
    let other: std::net::IpAddr = "192.0.2.11".parse().unwrap();
    assert_eq!(
        pairing_action(0, None, 0xAA, ip, false, 0, false),
        PairingAction::Prompt
    );
    assert_eq!(
        pairing_action(0xAA, Some(ip), 0xAA, ip, false, 0, false),
        PairingAction::Allow
    );
    assert_eq!(
        pairing_action(0xAA, Some(ip), 0xAA, other, false, 0, false),
        PairingAction::Prompt
    );
    assert_eq!(
        pairing_action(0xAA, None, 0xAA, ip, false, 0, false),
        PairingAction::Prompt,
        "旧配置缺少配对 IP 时必须重新确认"
    );
    assert_eq!(
        pairing_action(0xAA, Some(ip), 0xBB, ip, false, 0, false),
        PairingAction::Prompt
    );
    assert_eq!(
        pairing_action(0, None, 0xAA, ip, true, 0, false),
        PairingAction::Ignore
    );
    assert_eq!(
        pairing_action(0, None, 0xAA, ip, false, 0xAA, false),
        PairingAction::Ignore
    );
    assert_eq!(
        pairing_action(0, None, 0xAA, ip, false, 0xBB, false),
        PairingAction::Ignore
    );
    // 已绑定的设备已由精确 endpoint 校验，拒绝表/旧弹窗不能将其踢回 Prompt。
    assert_eq!(
        pairing_action(0xAA, Some(ip), 0xBB, other, true, 0xCC, true),
        PairingAction::Allow
    );
}

#[test]
fn session_reset_clears_binding_and_bumps_jitter_epoch() {
    let session = Session::for_test();
    let src: SocketAddr = "192.0.2.10:50000".parse().unwrap();
    session.with_p2p(|st| {
        st.state = DEVICE_BUSY;
        st.device_id = [7u8; protocol::DEVICE_ID_SIZE];
        st.addr = Some(src);
        st.forward_ssrc = 0x1234;
        st.reverse_ssrc = 0x5678;
        st.reverse_port = 50001;
        st.last_connect = Some(Instant::now());
    });
    let epoch = session.jitter().epoch;
    assert!(session.p2p().is_busy());

    session.reset_to_ready();

    let peer = session.p2p();
    assert!(!peer.is_busy());
    assert_eq!(peer.addr, None);
    assert_eq!(peer.forward_ssrc, 0);
    assert_eq!(
        session.jitter().epoch,
        epoch + 1,
        "复位必须让解码线程重建状态"
    );
}

#[test]
fn session_pairing_state_is_instance_scoped_and_bounded() {
    let a = Session::for_test();
    let b = Session::for_test();
    let ip: std::net::IpAddr = "192.0.2.10".parse().unwrap();
    let other: std::net::IpAddr = "192.0.2.11".parse().unwrap();
    assert_eq!(a.paired_device(), 0);

    a.set_paired_device(0xAA, ip);
    assert_eq!(a.paired_device(), 0xAA);
    assert_eq!(a.pairing_action_for(0xAA, ip, false), PairingAction::Allow);
    assert_eq!(
        a.pairing_action_for(0xAA, other, false),
        PairingAction::Prompt,
        "已配对 ID 从新 IP 来时必须重新确认"
    );
    a.set_paired_device(0xAA, other);
    assert_eq!(a.pairing_action_for(0xAA, other, false), PairingAction::Allow);
    assert_eq!(a.pairing_action_for(0xAA, ip, false), PairingAction::Prompt);

    // 同一 ID 在另一个会话里仍是未配对状态：不再有进程级串扰。
    assert_eq!(b.pairing_action_for(0xAA, ip, false), PairingAction::Prompt);

    assert!(a.claim_pairing_prompt(0xBB));
    assert!(!a.claim_pairing_prompt(0xCC), "单槽只允许一个待确认设备");
    assert_eq!(
        a.pairing_action_for(0xCC, ip, false),
        PairingAction::Ignore
    );
    a.clear_pairing_prompt();
    assert_eq!(
        a.pairing_action_for(0xCC, ip, false),
        PairingAction::Prompt
    );

    // 拒绝表有上限，避免故障设备反复重连把内存撑大；满时淘汰最早一条，
    // 否则 UI 已提示“已拒绝”的第 17 个设备下次 CONNECT 会再次弹窗。
    for id in 0..20u64 {
        a.deny_pairing(id);
    }
    let denied_len = a
        .pairing_denied
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();
    assert_eq!(denied_len, 16);
    assert!(!a.is_denied(0), "最早的拒绝记录必须被淘汰");
    assert!(a.is_denied(19), "最新的拒绝记录必须保留");
    assert_eq!(
        a.pairing_action_for(3, ip, false),
        PairingAction::Prompt,
        "已被淘汰的 ID 应重新允许弹窗"
    );
    assert_eq!(
        a.pairing_action_for(19, ip, false),
        PairingAction::Ignore,
        "仍在拒绝表中的 ID 必须被忽略"
    );
    assert_eq!(
        b.pairing_action_for(3, ip, false),
        PairingAction::Prompt
    );
}

#[test]
fn session_flags_and_ports_are_instance_scoped() {
    let a = Session::for_test();
    let b = Session::for_test();
    a.set_running(true);
    a.set_reverse_enabled(true);
    a.set_listen_port(51000);
    a.set_audio_state(AudioInitState::Ready);

    assert!(a.running());
    assert!(a.reverse_enabled());
    assert_eq!(a.listen_port(), 51000);
    assert_eq!(a.audio_state(), AudioInitState::Ready);

    assert!(!b.running());
    assert!(!b.reverse_enabled());
    assert_eq!(b.listen_port(), protocol::DEFAULT_LISTEN_PORT as u32);
    assert_eq!(b.audio_state(), AudioInitState::Pending);
}

#[test]
fn reverse_retry_gate_is_stored_per_session() {
    let a = Session::for_test();
    let b = Session::for_test();
    let now = Instant::now();
    a.set_reverse_retry_at(Some(now));
    assert_eq!(a.reverse_retry_at(), Some(now));
    assert_eq!(b.reverse_retry_at(), None);
}

#[test]
fn reverse_restart_is_rejected_after_stop_or_parameter_change() {
    // 回归：restart 在 spawn_blocking 里排队，期间可能已停止/释放绑定/换参数。
    // 旧任务拿到 restart 锁后不得再安装 ReverseSender。
    let addr: SocketAddr = "192.0.2.10:50001".parse().unwrap();
    let mut peer = P2pState::new();
    peer.state = DEVICE_BUSY;
    peer.addr = Some("192.0.2.10:50000".parse().unwrap());
    peer.reverse_port = 50001;
    peer.reverse_ssrc = 0x5678;

    assert!(should_start_reverse(true, &peer, addr, 0x5678));
    // 已停止（reset_to_ready 后 running=false）：参数一致也不能启动
    assert!(!should_start_reverse(false, &peer, addr, 0x5678));
    // 旧任务：SSRC / 端口 / IP 任一不匹配都不得覆盖当前绑定
    assert!(!should_start_reverse(true, &peer, addr, 0x9999));
    assert!(!should_start_reverse(
        true,
        &peer,
        "192.0.2.10:50002".parse().unwrap(),
        0x5678
    ));
    assert!(!should_start_reverse(
        true,
        &peer,
        "192.0.2.11:50001".parse().unwrap(),
        0x5678
    ));
    // 反向关闭（ssrc=0）不启动
    assert!(!should_start_reverse(true, &peer, addr, 0));
    // 未绑定
    let idle: SocketAddr = "192.0.2.10:50000".parse().unwrap();
    let mut unbound = P2pState::new();
    unbound.addr = Some(idle);
    unbound.reverse_port = 50001;
    unbound.reverse_ssrc = 0x5678;
    assert!(!should_start_reverse(true, &unbound, addr, 0x5678));
}

#[test]
fn reverse_target_preserves_ipv6_scope_id() {
    let src = SocketAddr::V6(std::net::SocketAddrV6::new(
        "fe80::1".parse().unwrap(),
        50000,
        0,
        7,
    ));
    match reverse_target(&src, 50001) {
        SocketAddr::V6(v6) => {
            assert_eq!(v6.port(), 50001);
            assert_eq!(v6.scope_id(), 7, "link-local 目标必须保留 scope_id");
        }
        SocketAddr::V4(_) => panic!("IPv6 源必须生成 IPv6 目标"),
    }
    let v4: SocketAddr = "192.0.2.10:50000".parse().unwrap();
    assert_eq!(
        reverse_target(&v4, 50001),
        "192.0.2.10:50001".parse().unwrap()
    );
}
