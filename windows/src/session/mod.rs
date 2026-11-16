//! 会话控制面：P2P 独占绑定、配对、反向发送器生命周期与 CONNECT 处理。

use crate::app::{Message, PairingRequest};
use crate::capture;
use crate::config;
use crate::logf;
use crate::protocol;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

mod audio;
pub(crate) use audio::{AudioInitState, init_audio_worker, jitter, wake_decoder};

#[cfg(test)]
mod tests;

// ══════════════════════════════════════════════
// 控制面状态（P2P 独占绑定）
// ══════════════════════════════════════════════

const DEVICE_READY: u8 = 0;
const DEVICE_BUSY: u8 = 1;

/// 绑定关系。
///
/// 历史实现把"状态"和"绑定 ID"放在两个独立全局里分别用 Relaxed 访问，
/// 二者构成的复合不变式没有任何顺序保证，可能读到 "BUSY + 旧 ID"。
/// 现在合并进一把锁，状态与全部绑定字段永远同时可见。
///
/// 正向 RTP 的准入条件是 (源地址, SSRC, PT)，三者在绑定建立时一次性确定；
/// 反向 RTP 由 Windows 发出，只需要 `reverse_ssrc` 与目标端口。
#[derive(Clone, Copy)]
pub(crate) struct P2pState {
    state: u8,
    device_id: [u8; protocol::DEVICE_ID_SIZE],
    /// CONNECT 源地址（Android 主 socket）：正向 RTP 必须来自同一 IP + 端口
    pub(crate) addr: Option<SocketAddr>,
    pub(crate) forward_ssrc: u32,
    reverse_ssrc: u32,
    reverse_port: u16,
    last_connect: Option<Instant>,
}

impl P2pState {
    const fn new() -> Self {
        Self {
            state: DEVICE_READY,
            device_id: [0u8; protocol::DEVICE_ID_SIZE],
            addr: None,
            forward_ssrc: 0,
            reverse_ssrc: 0,
            reverse_port: 0,
            last_connect: None,
        }
    }
    pub(crate) fn is_busy(&self) -> bool {
        self.state == DEVICE_BUSY
    }
}

/// A v3 device ID is public, so an established binding is the exact peer endpoint,
/// not merely the ID. Endpoint migration is reserved for an authenticated protocol.
fn is_bound_peer_source(
    peer: &P2pState,
    device_id: [u8; protocol::DEVICE_ID_SIZE],
    src: SocketAddr,
) -> bool {
    peer.is_busy() && peer.device_id == device_id && peer.addr == Some(src)
}

/// 控制包只有消息类型已知且设备 ID 非全零时才允许进入状态机。
///
/// 设备 ID 不是认证凭据，但协议规范明确把全零定义为无效值；若放行，
/// Busy 状态会建立一个以后永远无法被同一 ID 正常识别的绑定。
pub(crate) fn is_valid_control_header(header: &protocol::ControlHeader) -> bool {
    header.is_known_type() && protocol::is_valid_device_id(&header.device_id)
}

const DISCOVERY_RATE_PER_SOURCE: u32 = 5;
const DISCOVERY_MAX_TRACKED_SOURCES: usize = 1024;

/// RTP 连续空闲超过该值：下一个通过校验的包到达时先 reset 抖动缓冲。
///
/// 与 Android 端 `REVERSE_IDLE_RESET_MS`、`docs/PROTOCOL_V3_RTP.md` §7.12 一致。
/// 不做这一步时，本地 PLC/静音分支会按 10 ms/帧把 `next_ts` 一路推过新包的
/// 时间戳，发送端时间线一旦回退（编码器/采集重启），恢复包永远被判「迟到」，
/// 结果是整条反向音频永久静音、只留下单调增长的 `stats.late`。
pub(crate) const RTP_IDLE_RESET_MS: u64 = 1_000;

/// 发现请求的按源限流，并为流量状态表设置硬上限。
///
/// UDP 源地址可被伪造；只按 30 s 周期清理而不限制表大小，会让持续到达的
/// 不同源地址在清理前无界占用内存。达到上限后拒绝新源，已有的合法源仍按
/// 原令牌桶规则工作，直到下一轮清理回收过期项。
pub(crate) fn allow_discovery_request(
    bucket: &mut std::collections::HashMap<std::net::IpAddr, (u32, Instant)>,
    source: std::net::IpAddr,
    now: Instant,
) -> bool {
    if let Some((count, window_start)) = bucket.get_mut(&source) {
        if now.duration_since(*window_start) >= Duration::from_secs(1) {
            *count = 0;
            *window_start = now;
        }
        if *count >= DISCOVERY_RATE_PER_SOURCE {
            return false;
        }
        *count += 1;
        return true;
    }
    if bucket.len() >= DISCOVERY_MAX_TRACKED_SOURCES {
        return false;
    }
    bucket.insert(source, (1, now));
    true
}

static P2P_STATE: OnceLock<Mutex<P2pState>> = OnceLock::new();

/// 非法控制包的日志闸门：控制面与 RTP 共用一个接收任务，攻击者/故障设备
/// 刷非法 CONNECT 时不能让它用文件 I/O 拖慢实时音频接收。每秒最多一条。
pub(crate) fn should_log_control_reject() -> bool {
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    let gate = LAST.get_or_init(|| Mutex::new(None));
    // 中毒恢复与 p2p_lock 一致：闸门里只有 Instant，没有需要保持的不变量；
    // 因中毒而静默不记日志反而会丢掉现场证据。
    let mut last = gate.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if last
        .map(|t| now.duration_since(t) >= Duration::from_secs(1))
        .unwrap_or(true)
    {
        *last = Some(now);
        return true;
    }
    false
}

/// 合法控制事件的日志闸门：同样与实时接收共线程，重连/SSRC 抖动时
/// 不能让同步文件 I/O 每秒刷屏。
fn should_log_control_event() -> bool {
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    let gate = LAST.get_or_init(|| Mutex::new(None));
    let mut last = gate.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if last
        .map(|t| now.duration_since(t) >= Duration::from_secs(1))
        .unwrap_or(true)
    {
        *last = Some(now);
        return true;
    }
    false
}

fn p2p_lock() -> std::sync::MutexGuard<'static, P2pState> {
    P2P_STATE
        .get_or_init(|| Mutex::new(P2pState::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn p2p_snapshot() -> P2pState {
    *p2p_lock()
}

/// UI 是否已启动（停止时禁止广播、禁止接收 RTP）
pub(crate) static APP_RUNNING: AtomicU8 = AtomicU8::new(0);

/// 反向串流用户开关（全局，供控制面读取）
pub(crate) static REVERSE_ENABLED: AtomicU8 = AtomicU8::new(0);

/// 反向发送器重启的**串行化锁**。
///
/// 槽位锁（`reverse_sender()`）只用于短平快地 take/store；`stop()` 里的
/// `join` 必须放在槽位锁之外（否则 keepalive 路径的 `reverse_sender_alive()`
/// 以及 UI 线程会被一串到底）。但“同一时刻只有一个发送线程持有环回端点”这个
/// 不变量需要跨线程串行化（控制面 async 任务 + UI 线程都会重启），
/// 所以单独用一把锁。
static REVERSE_RESTART: Mutex<()> = Mutex::new(());

/// 反向音频发送器句柄
static REVERSE_SENDER: OnceLock<Mutex<Option<capture::ReverseSender>>> = OnceLock::new();
fn reverse_sender() -> &'static Mutex<Option<capture::ReverseSender>> {
    REVERSE_SENDER.get_or_init(|| Mutex::new(None))
}

/// 反向发送线程是否仍存活。槽位为 `Some` 不代表线程还在跑：
/// 环回初始化失败、编码器创建失败、线程 panic 都会让线程退出而槽位不动。
fn reverse_sender_alive() -> bool {
    reverse_sender()
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(|s| s.is_alive()))
        .unwrap_or(false)
}

/// 获取本机设备 ID 字节数组（进程内只解析一次，避免每包读注册表）
pub(crate) fn my_device_id() -> [u8; protocol::DEVICE_ID_SIZE] {
    static ID: OnceLock<[u8; protocol::DEVICE_ID_SIZE]> = OnceLock::new();
    *ID.get_or_init(|| config::Config::load().get_device_id_bytes())
}

/// 获取本机设备名（系统主机名，进程内只读一次）
pub(crate) fn my_device_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| std::env::var("COMPUTERNAME").unwrap_or_else(|_| "UDP2CAL".into()))
        .as_str()
}

/// 生成非零随机 32 位值（反向 SSRC 用）
fn random_u32_nonzero() -> u32 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    loop {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        let v = hasher.finish() as u32;
        if v != 0 {
            return v;
        }
    }
}

/// 释放绑定：回到 READY，停反向发送，清空抖动缓冲。
/// 控制面唯一的状态回退路径（超时 / 停止 / 引擎错误都走这里）。
pub(crate) fn reset_to_ready() {
    {
        let mut st = p2p_lock();
        *st = P2pState::new();
    }
    stop_reverse_sender();
    jitter().lock().unwrap_or_else(|e| e.into_inner()).reset();
    wake_decoder();
}

/// 已批准的手机设备 ID。0 = 未配对：所有 CONNECT 都必须先经过本机确认。
pub(crate) static PAIRED_DEVICE: AtomicU64 = AtomicU64::new(0);
/// 当前已提示、等待用户处理的设备 ID（0 = 无）。同一时间只弹一个请求。
pub(crate) static PAIRING_PROMPTED: AtomicU64 = AtomicU64::new(0);
/// 本次运行内被拒绝的设备 ID（不落盘；重启后重新征求确认）。
pub(crate) static PAIRING_DENIED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairingAction {
    Allow,
    Prompt,
    Ignore,
}

/// 纯决策：已批准 → 放行；已拒绝/已有弹窗 → 忽略；否则请求本机确认。
fn pairing_action(paired: u64, id: u64, denied: bool, prompted: u64) -> PairingAction {
    if paired == id {
        PairingAction::Allow
    } else if denied || prompted != 0 {
        PairingAction::Ignore
    } else {
        PairingAction::Prompt
    }
}

/// 发送 CONNECT_ACK（10 字节负载：reverse_ssrc + PT + frame_ms + forward_ssrc）。
/// 复用监听 socket 发送：ACK 源端口恒为服务端口，便于对端归属判断。
async fn send_connect_ack(
    socket: &tokio::net::UdpSocket,
    src_addr: &SocketAddr,
    reverse_ssrc: u32,
    forward_ssrc: u32,
) {
    let payload = protocol::ConnectAckPayload {
        reverse_ssrc,
        forward_ssrc,
        ..protocol::ConnectAckPayload::default()
    }
    .encode();
    let packet = protocol::build_control(protocol::TYPE_CONNECT_ACK, &my_device_id(), &payload);
    let _ = socket.send_to(&packet, src_addr).await;
}

/// 本次绑定应使用的反向 SSRC。
/// 反向关闭（用户开关关闭或 `reverse_port == 0`）时返回 0，表示不启动反向发送。
fn desired_reverse_ssrc(reverse_port: u16, previous: u32, regenerate: bool) -> u32 {
    if reverse_port == 0 || REVERSE_ENABLED.load(Ordering::Acquire) == 0 {
        return 0;
    }
    if previous != 0 && !regenerate {
        return previous;
    }
    random_u32_nonzero()
}

/// 启动/重启反向发送器：先停旧的，保证同一时刻只有一个线程持有环回端点。
fn restart_reverse_sender(addr: SocketAddr, ssrc: u32) {
    // 中毒恢复：槽里只有 Option<ReverseSender>，取回内部值继续操作；
    // 若因中毒直接跳过，旧发送线程会继续跑而日志一无所知。
    let serial = REVERSE_RESTART.lock().unwrap_or_else(|e| e.into_inner());
    stop_reverse_sender_under(&serial);
    if ssrc != 0 {
        let sender = capture::ReverseSender::start(addr, ssrc);
        *reverse_sender().lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
    }
}

/// 停止反向发送器（`stop()` 的 `join` 在槽位锁之外完成）。
fn stop_reverse_sender() {
    let serial = REVERSE_RESTART.lock().unwrap_or_else(|e| e.into_inner());
    stop_reverse_sender_under(&serial);
}

/// 取出槽位并 `stop()`。调用方必须已持有 `REVERSE_RESTART`
/// （维持“同一时刻只有一个发送线程”的不变量），但**不得**同时持有槽位锁：
/// `stop()` 会 `join` 发送线程，而该线程的启动阶段（`CoInitializeEx` +
/// 新建环回端点 + 新建 Opus 编码器）可长达数百毫秒。持有槽位锁做 join 会：
/// - 阻塞持有唯一 UDP socket 的 async 接收任务（期间不读数据报）；
/// - 阻塞 UI 线程（`reset_to_ready` 路径），整个窗口冻结；
/// - 把每次 keepalive 都要抢同一把锁的 `reverse_sender_alive()` 一起串住。
fn stop_reverse_sender_under(_serial: &std::sync::MutexGuard<'static, ()>) {
    // 临时守卫在语句结束即释放，`stop()` 在锁外执行。
    let old = reverse_sender()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(old) = old {
        old.stop();
    }
}

/// 死发送线程的最小重试间隔：1 Hz CONNECT 若每次都完整 stop/join + 重建
/// 设备/Opus，会持续阻塞 RTP 接收。地址/SSRC/端口真实变化仍立即重启。
static REVERSE_RETRY_AT: Mutex<Option<Instant>> = Mutex::new(None);
const REVERSE_RETRY_MIN: Duration = Duration::from_secs(2);

fn should_restart_reverse(
    reverse_stale: bool,
    sender_dead: bool,
    last_retry: Option<Instant>,
    now: Instant,
    min_interval: Duration,
) -> bool {
    if reverse_stale {
        true
    } else if !sender_dead {
        false
    } else {
        last_retry.is_none_or(|t| now.duration_since(t) >= min_interval)
    }
}

/// 处理一条控制消息。
///
/// 校验顺序（规范 5.7 第 5–7 步）：类型 → 负载布局 → 绑定关系。
/// 任何一步失败都静默丢弃：控制面不回显错误，也不修改绑定状态。
pub(crate) async fn handle_control(
    header: &protocol::ControlHeader,
    payload: &[u8],
    src_addr: &SocketAddr,
    socket: &tokio::net::UdpSocket,
    output: &mut iced::futures::channel::mpsc::Sender<Message>,
) {
    // 0（已废弃）与 5..=255（保留）都不是本端要处理的消息
    if !is_valid_control_header(header) || header.msg_type != protocol::TYPE_CONNECT {
        return;
    }

    let Some(cfg) = protocol::ConnectPayload::decode(payload) else {
        if should_log_control_reject() {
            logf!("[P2P] CONNECT 负载长度非法（{} 字节），丢弃", payload.len());
        }
        return;
    };
    if let Err(reason) = cfg.validate() {
        if should_log_control_reject() {
            logf!("[P2P] CONNECT 参数非法（{reason}），丢弃");
        }
        return;
    }
    let now = Instant::now();
    let peer = p2p_snapshot();
    let same_device = peer.is_busy() && peer.device_id == header.device_id;

    // v3 device_id 是公开标识，不是认证凭据。绑定期间固定完整源端点；
    // 未认证的地址/端口变化不能迁移绑定或刷新保活。合法网络切换需等待超时后重连。
    if same_device && !is_bound_peer_source(&peer, header.device_id, *src_addr) {
        if should_log_control_reject() {
            logf!("[P2P] 已绑定设备从新端点发起 CONNECT，拒绝未认证的端点迁移");
        }
        return;
    }

    // ── 已被其它设备占用：静默忽略，不回 ACK ──
    // 注意：反向编码器配置的更新必须放在这道门之后。否则同网段任意主机
    // 发一个参数合法的 CONNECT 就能改写当前会话的反向编码参数（码率/DTX/FEC），
    // 属于最小权限缺陷（规范 5.7 第 7 步要求先校验绑定关系）。
    if peer.is_busy() && !same_device {
        return;
    }

    // ── 配对闸门：绑定前必须由本机用户显式批准 ──
    // device_id 只是公开标识，不是凭据；未配对的 CONNECT 一律不 ACK、不建立
    // 绑定、不回调反向编码器，只弹一次确认。用户允许后手机的下一次 1 Hz
    // CONNECT 才真正建立会话。
    let device_id = u64::from_be_bytes(header.device_id);
    let denied = PAIRING_DENIED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&device_id);
    let action = pairing_action(
        PAIRED_DEVICE.load(Ordering::Acquire),
        device_id,
        denied,
        PAIRING_PROMPTED.load(Ordering::Acquire),
    );
    match action {
        PairingAction::Allow => {}
        PairingAction::Ignore => return,
        PairingAction::Prompt => {
            // 单槽 CAS：同时只允许一个待确认设备，重复 CONNECT 不重复弹窗。
            if PAIRING_PROMPTED
                .compare_exchange(0, device_id, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }
            if output
                .try_send(Message::PairingRequest(PairingRequest {
                    device_id,
                    addr: *src_addr,
                }))
                .is_err()
            {
                PAIRING_PROMPTED.store(0, Ordering::Release);
            }
            return;
        }
    }

    // 反向编码器配置只描述 Windows → Android；内容相同则不下发（避免每秒重置编码器）
    capture::update_reverse_config(capture::OpusReverseConfig::from(&cfg));

    if !same_device {
        // ── 建立绑定 ──
        let reverse_ssrc = desired_reverse_ssrc(cfg.reverse_port, 0, true);
        {
            let mut st = p2p_lock();
            st.state = DEVICE_BUSY;
            st.device_id = header.device_id;
            st.addr = Some(*src_addr);
            st.forward_ssrc = cfg.forward_ssrc;
            st.reverse_ssrc = reverse_ssrc;
            st.reverse_port = cfg.reverse_port;
            st.last_connect = Some(now);
        }
        // 新会话：抖动缓冲与解码器状态全部重建
        jitter().lock().unwrap_or_else(|e| e.into_inner()).reset();
        wake_decoder();
        // restart 里的 stop 是同步 join，旧线程卡在启动段时可达几百 ms；
        // 移出接收任务（spawn_blocking），避免顶住 RTP/CONNECT 入栈。
        // ACK 不等线程起来：反向包晚 ~百 ms 到达只多喂几帧 PLC。
        let addr = SocketAddr::new(src_addr.ip(), cfg.reverse_port.max(1));
        tokio::task::spawn_blocking(move || restart_reverse_sender(addr, reverse_ssrc));
        if should_log_control_event() {
            logf!(
                "[P2P] 手机端 {} 已绑定（forward_ssrc={:#010x} reverse_ssrc={:#010x} reverse_port={}）",
                src_addr.ip(),
                cfg.forward_ssrc,
                reverse_ssrc,
                cfg.reverse_port
            );
        }
        let _ = output.try_send(Message::PeerStateChanged);
        send_connect_ack(socket, src_addr, reverse_ssrc, cfg.forward_ssrc).await;
        return;
    }

    // ── 同一设备的周期 CONNECT（保活 / 参数同步）──
    // forward_ssrc 变化表示新会话：必须重置抖动缓冲/解码器并换反向 SSRC
    let new_session = peer.forward_ssrc != cfg.forward_ssrc;
    let want_reverse = cfg.reverse_port != 0 && REVERSE_ENABLED.load(Ordering::Acquire) != 0;
    let reverse_stale = new_session
        || peer.reverse_port != cfg.reverse_port
        || (peer.reverse_ssrc != 0) != want_reverse;
    // 槽位里还有 ReverseSender 但线程已经结束：必须重启，否则保活一直显示健康。
    // 这种情况不换 SSRC（换号会让 Android 无谓地重建解码器/播放缓冲）。
    let sender_dead = want_reverse && peer.reverse_ssrc != 0 && !reverse_sender_alive();
    let last_retry = *REVERSE_RETRY_AT.lock().unwrap_or_else(|e| e.into_inner());
    let need_restart = should_restart_reverse(
        reverse_stale,
        sender_dead,
        last_retry,
        now,
        REVERSE_RETRY_MIN,
    );

    let reverse_ssrc = if reverse_stale {
        desired_reverse_ssrc(cfg.reverse_port, peer.reverse_ssrc, true)
    } else {
        peer.reverse_ssrc
    };

    {
        let mut st = p2p_lock();
        st.addr = Some(*src_addr);
        st.forward_ssrc = cfg.forward_ssrc;
        st.reverse_ssrc = reverse_ssrc;
        st.reverse_port = cfg.reverse_port;
        st.last_connect = Some(now);
    }

    if new_session {
        jitter().lock().unwrap_or_else(|e| e.into_inner()).reset();
        wake_decoder();
        if should_log_control_event() {
            logf!("[P2P] forward_ssrc 变化 → 新会话，已重置抖动缓冲");
        }
    }
    if need_restart {
        if sender_dead {
            *REVERSE_RETRY_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
        }
        // 同上：restart 含同步 join，移出接收任务；串行锁保证与后续 restart
        // 互斥，限流清除（依赖 restart 完成）也一并移入。
        let addr = SocketAddr::new(src_addr.ip(), cfg.reverse_port.max(1));
        let dead = sender_dead;
        let stale = reverse_stale;
        let port = cfg.reverse_port;
        tokio::task::spawn_blocking(move || {
            restart_reverse_sender(addr, reverse_ssrc);
            if should_log_control_event() {
                if stale {
                    logf!("[P2P] 反向发送更新：ssrc={reverse_ssrc:#010x} port={port}");
                } else {
                    logf!("[P2P] 反向发送线程已退出，已重启：ssrc={reverse_ssrc:#010x} port={port}");
                }
            }
            if !dead && reverse_sender_alive() {
                *REVERSE_RETRY_AT.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        });
    } else if !sender_dead && reverse_sender_alive() {
        // 只在线程活过一个完整 CONNECT 周期后清除限流，避免“刚重启就退出”
        // 的线程把 1 Hz 重试重新放开。
        *REVERSE_RETRY_AT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    send_connect_ack(socket, src_addr, reverse_ssrc, cfg.forward_ssrc).await;
}

/// 控制面超时：绑定设备 4.5 s 没有 CONNECT → 释放绑定、停止反向、回到 Ready。
/// 只依据控制面保活判断（DTX 静音期间可能长时间没有 RTP）。
pub(crate) fn check_peer_timeout() -> bool {
    let st = p2p_lock();
    if !st.is_busy() {
        return false;
    }
    let expired = st
        .last_connect
        .map(|t| t.elapsed() > Duration::from_millis(protocol::PEER_TIMEOUT_MS))
        .unwrap_or(true);
    if expired {
        drop(st);
        reset_to_ready();
        true
    } else {
        false
    }
}
