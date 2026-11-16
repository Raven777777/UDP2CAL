//! 会话控制面：P2P 独占绑定、配对、反向发送器生命周期与 CONNECT 处理。
//!
//! ## 状态与锁顺序
//!
//! 进程内所有**会话级**可变状态都收敛在 [`Session`] 上，由 `app::run` 创建
//! 一次并以 `Arc<Session>` 注入广播线程、UDP 接收流、解码线程与 UI。这样：
//!
//! * 测试可以创建独立的 `Session` 实例，不必共享进程静态；
//! * 每个字段的锁/原子语义集中在一处，不再出现“绑定 ID 和 BUSY 状态分属
//!   两个全局变量”这类复合不变式；
//! * 跨模块依赖是显式参数，而不是隐式全局。
//!
//! 锁顺序（小号先拿，**禁止反向获取**）：
//!
//! 1. `reverse.restart` —— 串行化反向发送器重启；
//! 2. `reverse.sender` —— 槽位，只做 take/store；
//! 3. `p2p` —— 绑定关系；
//! 4. `jitter` —— 正向抖动缓冲；
//! 5. `reverse_shared.config` —— 反向 Opus 配置；
//! 6. `pairing_denied` —— 本次运行内被拒设备表；
//! 7. `decode_thread` / `retry_at` / `reject_log` / `event_log` —— 叶子锁，
//!    不与任何其它锁嵌套。
//!
//! 实践上绝大多数路径只取一把锁：需要多步时先拿快照、放锁、再操作。
//! 唯一有意的嵌套是 `reverse.restart → reverse.sender`（以及 take 之后在
//! `restart` 保护下、**槽位锁已释放**时执行 `ReverseSender::stop()` 的 join）。
//! 任何路径都不允许在持有 `reverse.sender` 时 join：发送线程启动可能要几百
//! 毫秒，而 keepalive 的 `reverse_sender_alive()` 与 UI 线程都要读这个槽位。

use crate::app::{Message, PairingRequest};
use crate::audio::AudioStats;
use crate::capture;
use crate::config;
use crate::logf;
use crate::protocol;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::Thread;
use std::time::{Duration, Instant};

mod audio;
pub(crate) use audio::{AudioInitState, init_audio_worker};

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

/// 会话级状态容器。见模块头部的「状态与锁顺序」。
pub(crate) struct Session {
    // ── 控制面 ──
    p2p: Mutex<P2pState>,
    /// 非法控制包的日志闸门：控制面与 RTP 共用一个接收任务，攻击者/故障设备
    /// 刷非法 CONNECT 时不能让它用文件 I/O 拖慢实时音频接收。每秒最多一条。
    reject_log: Mutex<Option<Instant>>,
    /// 合法控制事件的日志闸门（同上，每秒最多一条）。
    event_log: Mutex<Option<Instant>>,

    // ── 接收侧 ──
    /// 正向媒体流的抖动缓冲。网络线程只插入，解码线程只按播放头拉取。
    jitter: Mutex<protocol::rtp::JitterBuffer>,
    /// 解码线程句柄：网络线程与复位路径据此唤醒它（unpark 为原子置位，无分配）。
    ///
    /// 必须**可替换**：音频初始化失败后重试会创建新的解码线程，而 `OnceLock`
    /// 只能写入一次——重试后 `wake_decoder()` 会一直唤醒已退出的旧线程，新线程
    /// 只能靠 4 ms 轮询兜底（表现为卡顿与延迟上升）。
    decode_thread: Mutex<Option<Thread>>,
    /// 音频初始化状态。
    audio_state: AtomicU8,
    audio_stats: Arc<AudioStats>,

    // ── 反向发送 ──
    reverse: ReverseState,
    /// 反向 Opus 配置 + 发送统计；发送线程持有同一份 `Arc`。
    reverse_shared: Arc<capture::ReverseShared>,

    // ── 配对 ──
    /// 已批准的手机设备 ID。0 = 未配对：所有 CONNECT 都必须先经过本机确认。
    paired_device: AtomicU64,
    /// 当前已提示、等待用户处理的设备 ID（0 = 无）。同一时间只弹一个请求。
    pairing_prompted: AtomicU64,
    /// 本次运行内被拒绝的设备 ID（不落盘；重启后重新征求确认）。
    pairing_denied: Mutex<Vec<u64>>,

    // ── 运行开关 ──
    /// UI 是否已启动（停止时禁止广播、禁止接收 RTP）
    running: AtomicBool,
    /// 反向串流用户开关
    reverse_enabled: AtomicBool,
    /// 当前监听端口（广播线程与接收流读取）
    listen_port: AtomicU32,

    // ── 身份 ──
    /// 本机设备 ID（启动时从配置解析一次，避免每包读注册表）
    device_id: [u8; protocol::DEVICE_ID_SIZE],
    /// 本机设备名（系统主机名，进程内只读一次）
    device_name: String,
}

struct ReverseState {
    /// 反向发送器重启的**串行化锁**。
    ///
    /// 槽位锁只用于短平快地 take/store；`stop()` 里的 `join` 必须放在槽位锁
    /// 之外（否则 keepalive 路径的 `reverse_sender_alive()` 以及 UI 线程会被
    /// 一串到底）。但“同一时刻只有一个发送线程持有环回端点”这个不变量需要
    /// 跨线程串行化（控制面 async 任务 + UI 线程都会重启），所以单独用一把锁。
    restart: Mutex<()>,
    /// 反向音频发送器句柄
    sender: Mutex<Option<capture::ReverseSender>>,
    /// 死发送线程的最小重试间隔状态：1 Hz CONNECT 若每次都完整 stop/join +
    /// 重建设备/Opus，会持续阻塞 RTP 接收。地址/SSRC/端口真实变化仍立即重启。
    retry_at: Mutex<Option<Instant>>,
}

impl Session {
    /// 从启动配置构造会话。设备身份在这里解析一次并固化。
    pub(crate) fn from_config(cfg: &config::Config) -> Arc<Self> {
        let device_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "UDP2CAL".into());
        Arc::new(Self::with_parts(
            cfg.get_device_id_bytes(),
            device_name,
            cfg.listen_port,
            cfg.paired_device,
            cfg.reverse_enabled != 0,
        ))
    }

    fn with_parts(
        device_id: [u8; protocol::DEVICE_ID_SIZE],
        device_name: String,
        listen_port: u32,
        paired_device: u64,
        reverse_enabled: bool,
    ) -> Self {
        Self {
            p2p: Mutex::new(P2pState::new()),
            reject_log: Mutex::new(None),
            event_log: Mutex::new(None),
            jitter: Mutex::new(protocol::rtp::JitterBuffer::new()),
            decode_thread: Mutex::new(None),
            audio_state: AtomicU8::new(AudioInitState::Pending as u8),
            audio_stats: Arc::new(AudioStats::default()),
            reverse: ReverseState {
                restart: Mutex::new(()),
                sender: Mutex::new(None),
                retry_at: Mutex::new(None),
            },
            reverse_shared: Arc::new(capture::ReverseShared::new()),
            paired_device: AtomicU64::new(paired_device),
            pairing_prompted: AtomicU64::new(0),
            pairing_denied: Mutex::new(Vec::new()),
            running: AtomicBool::new(false),
            reverse_enabled: AtomicBool::new(reverse_enabled),
            listen_port: AtomicU32::new(listen_port),
            device_id,
            device_name,
        }
    }

    /// 独立实例，供单元测试使用：不读注册表、不生成设备 ID。
    #[cfg(test)]
    pub(crate) fn for_test() -> Arc<Self> {
        Arc::new(Self::with_parts(
            [1u8; protocol::DEVICE_ID_SIZE],
            "test".into(),
            protocol::DEFAULT_LISTEN_PORT as u32,
            0,
            false,
        ))
    }

    // ── 身份与运行开关 ──

    pub(crate) fn device_id(&self) -> [u8; protocol::DEVICE_ID_SIZE] {
        self.device_id
    }

    pub(crate) fn device_name(&self) -> &str {
        &self.device_name
    }

    pub(crate) fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub(crate) fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::Release);
    }

    pub(crate) fn reverse_enabled(&self) -> bool {
        self.reverse_enabled.load(Ordering::Acquire)
    }

    pub(crate) fn set_reverse_enabled(&self, enabled: bool) {
        self.reverse_enabled.store(enabled, Ordering::Release);
    }

    pub(crate) fn listen_port(&self) -> u32 {
        self.listen_port.load(Ordering::Acquire)
    }

    pub(crate) fn set_listen_port(&self, port: u32) {
        self.listen_port.store(port, Ordering::Release);
    }

    // ── 音频初始化状态 ──

    pub(crate) fn audio_state(&self) -> AudioInitState {
        AudioInitState::from_u8(self.audio_state.load(Ordering::Acquire))
    }

    pub(crate) fn set_audio_state(&self, state: AudioInitState) {
        self.audio_state.store(state as u8, Ordering::Release);
    }

    pub(crate) fn audio_stats(&self) -> &Arc<AudioStats> {
        &self.audio_stats
    }

    // ── 抖动缓冲 / 解码线程 ──

    pub(crate) fn jitter(&self) -> MutexGuard<'_, protocol::rtp::JitterBuffer> {
        self.jitter.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn wake_decoder(&self) {
        if let Some(t) = self
            .decode_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            t.unpark();
        }
    }

    pub(crate) fn set_decode_thread(&self, t: Thread) {
        *self.decode_thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
    }

    // ── P2P 绑定 ──

    pub(crate) fn p2p(&self) -> P2pState {
        *self.p2p.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn with_p2p<R>(&self, f: impl FnOnce(&mut P2pState) -> R) -> R {
        let mut st = self.p2p.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut st)
    }

    /// 释放绑定：回到 READY，停反向发送，清空抖动缓冲。
    /// 控制面唯一的状态回退路径（超时 / 停止 / 引擎错误都走这里）。
    pub(crate) fn reset_to_ready(&self) {
        self.with_p2p(|st| *st = P2pState::new());
        self.stop_reverse_sender();
        self.jitter().reset();
        self.wake_decoder();
    }

    /// 控制面超时：绑定设备 4.5 s 没有 CONNECT → 释放绑定、停止反向、回到 Ready。
    /// 只依据控制面保活判断（DTX 静音期间可能长时间没有 RTP）。
    pub(crate) fn check_peer_timeout(&self) -> bool {
        let expired = {
            let st = self.p2p.lock().unwrap_or_else(|e| e.into_inner());
            if !st.is_busy() {
                return false;
            }
            st.last_connect
                .map(|t| t.elapsed() > Duration::from_millis(protocol::PEER_TIMEOUT_MS))
                .unwrap_or(true)
        };
        if expired {
            self.reset_to_ready();
        }
        expired
    }

    // ── 配对 ──

    pub(crate) fn paired_device(&self) -> u64 {
        self.paired_device.load(Ordering::Acquire)
    }

    pub(crate) fn set_paired_device(&self, id: u64) {
        self.paired_device.store(id, Ordering::Release);
    }

    pub(crate) fn clear_pairing_prompt(&self) {
        self.pairing_prompted.store(0, Ordering::Release);
    }

    /// 单槽 CAS：同时只允许一个待确认设备，重复 CONNECT 不重复弹窗。
    fn claim_pairing_prompt(&self, id: u64) -> bool {
        self.pairing_prompted
            .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn is_denied(&self, id: u64) -> bool {
        self.pairing_denied
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&id)
    }

    pub(crate) fn deny_pairing(&self, id: u64) {
        let mut denied = self
            .pairing_denied
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if denied.len() < 16 {
            denied.push(id);
        }
    }

    fn pairing_action_for(&self, id: u64) -> PairingAction {
        pairing_action(
            self.paired_device(),
            id,
            self.is_denied(id),
            self.pairing_prompted.load(Ordering::Acquire),
        )
    }

    // ── 反向配置 / 统计 ──

    /// 由控制面调用；只有内容真正变化才落盘日志。
    fn update_reverse_config(&self, cfg: capture::OpusReverseConfig) {
        self.reverse_shared.update_config(cfg);
    }

    pub(crate) fn take_reverse_stats(&self) -> capture::ReverseSnapshot {
        self.reverse_shared.take_stats()
    }

    // ── 日志闸门 ──

    pub(crate) fn should_log_control_reject(&self) -> bool {
        should_log(&self.reject_log)
    }

    fn should_log_control_event(&self) -> bool {
        should_log(&self.event_log)
    }

    // ── 反向发送器生命周期 ──

    /// 反向发送线程是否仍存活。槽位为 `Some` 不代表线程还在跑：
    /// 环回初始化失败、编码器创建失败、线程 panic 都会让线程退出而槽位不动。
    fn reverse_sender_alive(&self) -> bool {
        self.reverse
            .sender
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|s| s.is_alive()))
            .unwrap_or(false)
    }

    /// 本次绑定应使用的反向 SSRC。
    /// 反向关闭（用户开关关闭或 `reverse_port == 0`）时返回 0，表示不启动反向发送。
    fn desired_reverse_ssrc(&self, reverse_port: u16, previous: u32, regenerate: bool) -> u32 {
        if reverse_port == 0 || !self.reverse_enabled() {
            return 0;
        }
        if previous != 0 && !regenerate {
            return previous;
        }
        random_u32_nonzero()
    }

    /// 启动/重启反向发送器：先停旧的，保证同一时刻只有一个线程持有环回端点。
    fn restart_reverse_sender(&self, addr: SocketAddr, ssrc: u32) {
        // 中毒恢复：槽里只有 Option<ReverseSender>，取回内部值继续操作；
        // 若因中毒直接跳过，旧发送线程会继续跑而日志一无所知。
        let serial = self
            .reverse
            .restart
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.stop_reverse_sender_under(&serial);
        if ssrc != 0 {
            let sender =
                capture::ReverseSender::start(addr, ssrc, Arc::clone(&self.reverse_shared));
            *self
                .reverse
                .sender
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(sender);
        }
    }

    /// 停止反向发送器（`stop()` 的 `join` 在槽位锁之外完成）。
    fn stop_reverse_sender(&self) {
        let serial = self
            .reverse
            .restart
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.stop_reverse_sender_under(&serial);
    }

    /// 取出槽位并 `stop()`。调用方必须已持有 `reverse.restart`
    /// （维持“同一时刻只有一个发送线程”的不变量），但**不得**同时持有槽位锁：
    /// `stop()` 会 `join` 发送线程，而该线程的启动阶段（`CoInitializeEx` +
    /// 新建环回端点 + 新建 Opus 编码器）可长达数百毫秒。持有槽位锁做 join 会：
    /// - 阻塞持有唯一 UDP socket 的 async 接收任务（期间不读数据报）；
    /// - 阻塞 UI 线程（`reset_to_ready` 路径），整个窗口冻结；
    /// - 把每次 keepalive 都要抢同一把锁的 `reverse_sender_alive()` 一起串住。
    fn stop_reverse_sender_under(&self, _serial: &MutexGuard<'_, ()>) {
        // 临时守卫在语句结束即释放，`stop()` 在锁外执行。
        let old = self
            .reverse
            .sender
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(old) = old {
            old.stop();
        }
    }

    fn reverse_retry_at(&self) -> Option<Instant> {
        *self
            .reverse
            .retry_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn set_reverse_retry_at(&self, at: Option<Instant>) {
        *self
            .reverse
            .retry_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = at;
    }

    // ── 控制包处理 ──

    /// 发送 CONNECT_ACK（10 字节负载：reverse_ssrc + PT + frame_ms + forward_ssrc）。
    /// 复用监听 socket 发送：ACK 源端口恒为服务端口，便于对端归属判断。
    async fn send_connect_ack(
        &self,
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
        let packet = protocol::build_control(protocol::TYPE_CONNECT_ACK, &self.device_id, &payload);
        let _ = socket.send_to(&packet, src_addr).await;
    }

    /// 处理一条控制消息。
    ///
    /// 校验顺序（规范 5.7 第 5–7 步）：类型 → 负载布局 → 绑定关系。
    /// 任何一步失败都静默丢弃：控制面不回显错误，也不修改绑定状态。
    pub(crate) async fn handle_control(
        self: &Arc<Self>,
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
            if self.should_log_control_reject() {
                logf!("[P2P] CONNECT 负载长度非法（{} 字节），丢弃", payload.len());
            }
            return;
        };
        if let Err(reason) = cfg.validate() {
            if self.should_log_control_reject() {
                logf!("[P2P] CONNECT 参数非法（{reason}），丢弃");
            }
            return;
        }
        let now = Instant::now();
        let peer = self.p2p();
        let same_device = peer.is_busy() && peer.device_id == header.device_id;

        // v3 device_id 是公开标识，不是认证凭据。绑定期间固定完整源端点；
        // 未认证的地址/端口变化不能迁移绑定或刷新保活。合法网络切换需等待超时后重连。
        if same_device && !is_bound_peer_source(&peer, header.device_id, *src_addr) {
            if self.should_log_control_reject() {
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
        match self.pairing_action_for(device_id) {
            PairingAction::Allow => {}
            PairingAction::Ignore => return,
            PairingAction::Prompt => {
                if !self.claim_pairing_prompt(device_id) {
                    return;
                }
                if output
                    .try_send(Message::PairingRequest(PairingRequest {
                        device_id,
                        addr: *src_addr,
                    }))
                    .is_err()
                {
                    self.clear_pairing_prompt();
                }
                return;
            }
        }

        // 反向编码器配置只描述 Windows → Android；内容相同则不下发（避免每秒重置编码器）
        self.update_reverse_config(capture::OpusReverseConfig::from(&cfg));

        if !same_device {
            // ── 建立绑定 ──
            let reverse_ssrc = self.desired_reverse_ssrc(cfg.reverse_port, 0, true);
            self.with_p2p(|st| {
                st.state = DEVICE_BUSY;
                st.device_id = header.device_id;
                st.addr = Some(*src_addr);
                st.forward_ssrc = cfg.forward_ssrc;
                st.reverse_ssrc = reverse_ssrc;
                st.reverse_port = cfg.reverse_port;
                st.last_connect = Some(now);
            });
            // 新会话：抖动缓冲与解码器状态全部重建
            self.jitter().reset();
            self.wake_decoder();
            // restart 里的 stop 是同步 join，旧线程卡在启动段时可达几百 ms；
            // 移出接收任务（spawn_blocking），避免顶住 RTP/CONNECT 入栈。
            // ACK 不等线程起来：反向包晚 ~百 ms 到达只多喂几帧 PLC。
            let addr = SocketAddr::new(src_addr.ip(), cfg.reverse_port.max(1));
            let this = Arc::clone(self);
            tokio::task::spawn_blocking(move || this.restart_reverse_sender(addr, reverse_ssrc));
            if self.should_log_control_event() {
                logf!(
                    "[P2P] 手机端 {} 已绑定（forward_ssrc={:#010x} reverse_ssrc={:#010x} reverse_port={}）",
                    src_addr.ip(),
                    cfg.forward_ssrc,
                    reverse_ssrc,
                    cfg.reverse_port
                );
            }
            let _ = output.try_send(Message::PeerStateChanged);
            self.send_connect_ack(socket, src_addr, reverse_ssrc, cfg.forward_ssrc)
                .await;
            return;
        }

        // ── 同一设备的周期 CONNECT（保活 / 参数同步）──
        // forward_ssrc 变化表示新会话：必须重置抖动缓冲/解码器并换反向 SSRC
        let new_session = peer.forward_ssrc != cfg.forward_ssrc;
        let want_reverse = cfg.reverse_port != 0 && self.reverse_enabled();
        let reverse_stale = new_session
            || peer.reverse_port != cfg.reverse_port
            || (peer.reverse_ssrc != 0) != want_reverse;
        // 槽位里还有 ReverseSender 但线程已经结束：必须重启，否则保活一直显示健康。
        // 这种情况不换 SSRC（换号会让 Android 无谓地重建解码器/播放缓冲）。
        let sender_dead = want_reverse && peer.reverse_ssrc != 0 && !self.reverse_sender_alive();
        let need_restart = should_restart_reverse(
            reverse_stale,
            sender_dead,
            self.reverse_retry_at(),
            now,
            REVERSE_RETRY_MIN,
        );

        let reverse_ssrc = if reverse_stale {
            self.desired_reverse_ssrc(cfg.reverse_port, peer.reverse_ssrc, true)
        } else {
            peer.reverse_ssrc
        };

        self.with_p2p(|st| {
            st.addr = Some(*src_addr);
            st.forward_ssrc = cfg.forward_ssrc;
            st.reverse_ssrc = reverse_ssrc;
            st.reverse_port = cfg.reverse_port;
            st.last_connect = Some(now);
        });

        if new_session {
            self.jitter().reset();
            self.wake_decoder();
            if self.should_log_control_event() {
                logf!("[P2P] forward_ssrc 变化 → 新会话，已重置抖动缓冲");
            }
        }
        if need_restart {
            if sender_dead {
                self.set_reverse_retry_at(Some(now));
            }
            // 同上：restart 含同步 join，移出接收任务；串行锁保证与后续 restart
            // 互斥，限流清除（依赖 restart 完成）也一并移入。
            let addr = SocketAddr::new(src_addr.ip(), cfg.reverse_port.max(1));
            let dead = sender_dead;
            let stale = reverse_stale;
            let port = cfg.reverse_port;
            let this = Arc::clone(self);
            tokio::task::spawn_blocking(move || {
                this.restart_reverse_sender(addr, reverse_ssrc);
                if this.should_log_control_event() {
                    if stale {
                        logf!("[P2P] 反向发送更新：ssrc={reverse_ssrc:#010x} port={port}");
                    } else {
                        logf!(
                            "[P2P] 反向发送线程已退出，已重启：ssrc={reverse_ssrc:#010x} port={port}"
                        );
                    }
                }
                if !dead && this.reverse_sender_alive() {
                    this.set_reverse_retry_at(None);
                }
            });
        } else if !sender_dead && self.reverse_sender_alive() {
            // 只在线程活过一个完整 CONNECT 周期后清除限流，避免“刚重启就退出”
            // 的线程把 1 Hz 重试重新放开。
            self.set_reverse_retry_at(None);
        }

        self.send_connect_ack(socket, src_addr, reverse_ssrc, cfg.forward_ssrc)
            .await;
    }
}

fn should_log(gate: &Mutex<Option<Instant>>) -> bool {
    // 中毒恢复与 p2p 锁一致：闸门里只有 Instant，没有需要保持的不变量；
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

/// 生成非零随机 32 位值（反向 SSRC 用）
fn random_u32_nonzero() -> u32 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    static SEQ: AtomicU64 = AtomicU64::new(0);
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

/// 死发送线程的最小重试间隔。
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
