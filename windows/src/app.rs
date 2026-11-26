//! 应用层：iced 状态、消息处理与启动编排。

use crate::audio;
use crate::config;
use crate::diag;
use crate::firewall;
use crate::i18n;
use crate::logf;
use crate::logger;
use crate::net;
use crate::power;
use crate::protocol;
use crate::session;
use crate::tray;
use crate::ui;
use iced::{Subscription, Task, Theme};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::PCWSTR;

#[derive(Debug, Clone)]
pub(crate) enum Message {
    IpChanged(String),
    PortChanged(String),
    ToggleRunning,
    ToggleReverse(bool),
    ToggleAutoStart(bool),
    /// 托盘切换接收目标水位（ms）
    SetWaterTarget(u32),
    /// 托盘切换界面语言（空串 = 跟随系统）
    SetLanguage(String),
    StatusUpdate(StatusInfo),
    Disconnected,
    /// 引擎级失败（监听失败 / 音频初始化失败）：必须回到停止态并把原因显示出来
    EngineError(String),
    /// P2P 绑定/重连状态变化：只触发一次重绘，不携带也不修改任何遥测数据。
    /// （历史实现用 bitrate=0 的 StatusUpdate 充当重绘信号，导致码率显示每秒闪一次 0）
    PeerStateChanged,
    /// 未配对设备请求连接：必须由本机用户显式允许后才建立绑定。
    PairingRequest(PairingRequest),
    AllowPairing,
    DenyPairing,
    HideWindow,
    ShowWindow,
    Quit,
}

#[derive(Debug, Clone)]
pub(crate) struct StatusInfo {
    pub(crate) bitrate_kbps: f32,
    pub(crate) level_db: f32,
}

/// 未配对设备的连接请求。只包含本机需要展示、其余流程不需要的字段。
#[derive(Debug, Clone, Copy)]
pub(crate) struct PairingRequest {
    pub(crate) device_id: u64,
    pub(crate) addr: SocketAddr,
}

/// 提示行的语义级别。
/// 历史实现靠中文子串（"失败/无效/不能/断开/不是合法"）猜测配色，
/// 任何文案调整都会让颜色错位；改为在写入处显式声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoticeLevel {
    Info,
    Error,
}

pub(crate) struct AppState {
    /// 会话级共享状态：绑定、抖动缓冲、反向发送、配对与音频统计。
    pub(crate) session: Arc<session::Session>,
    pub(crate) config: config::Config,
    pub(crate) ip_input: String,
    pub(crate) port_input: String,
    pub(crate) port_valid: bool,
    pub(crate) is_running: bool,
    pub(crate) connected: bool,
    /// 是否处于“已断开”而非“等待首次连接”（用于状态点文案）
    pub(crate) disconnected: bool,
    pub(crate) last_bitrate: f32,
    pub(crate) last_level_db: f32,
    /// 需要直接展示给用户的提示（错误 / 警告 / 断开原因）
    pub(crate) notice: String,
    /// 提示级别，决定配色
    pub(crate) notice_level: NoticeLevel,
    pub(crate) vb_cable_installed: bool,
    /// 底部广播地址文案：随当前语言生成一次，菜单切语言时更新。
    /// 重绘频率远高于语言切换，没必要每帧重新查表 + 格式化。
    pub(crate) footer_broadcast: String,
    pub(crate) last_toggle_instant: Instant,
    /// 上次写音频诊断日志的时间（30s 一次，避免刷盘）
    pub(crate) last_stats_log: Instant,
    pub(crate) reverse_enabled: bool,
    /// 待用户确认的配对请求（有值时显示模态框）
    pub(crate) pending_pair: Option<PairingRequest>,
}

pub(crate) fn run() -> Result<(), iced::Error> {
    // 尽早初始化落盘日志：后续所有失败路径都要能留下证据
    logger::init();
    // 任何线程 panic 都要落盘：GUI 子系统下 stderr 会被丢弃
    logger::install_panic_hook();
    i18n::init();
    logf!("[App] 启动 v{}", env!("CARGO_PKG_VERSION"));
    if let Some(p) = logger::path() {
        logf!("[App] 日志文件: {}", p.display());
    }

    // 配置先于单实例提示加载：重复启动的提示也要用用户已选的语言。
    // 只读语言，不触发设备 ID 的生成/落盘。
    i18n::set_active(&config::load_language());

    // SAFETY: 三处 `PCWSTR::from_raw` 的缓冲都是本作用域内的 `Vec<u16>`，
    // 以 `\0` 结尾且生命周期覆盖调用；`CreateMutexW` 句柄故意泄漏到进程结束
    // （单实例锁整个进程生命周期有效），不对其调用 `CloseHandle`。
    unsafe {
        let name: Vec<u16> = "UDP2CAL_SingleInstance_Mutex\0".encode_utf16().collect();
        let _ = CreateMutexW(None, true, PCWSTR::from_raw(name.as_ptr()));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // 已有实例在运行：不弹任何提示框，直接请求它自己弹出窗口。
            // 信号通道不可用（运行中的是旧版本、没有唤醒事件）时才回退
            // 到旧的裸激活——该路径会让 winit 可见性状态失同步，导致
            // 弹出后的窗口点"X"失效，仅作为兼容兜底。
            if !tray::signal_show_request() {
                logf!("[App] 唤醒事件不可用，回退为直接激活已有窗口");
                tray::focus_existing_window("UDP2CAL");
            }
            return Ok(());
        }
    }

    // 单实例检查通过后再接管电源：运行期保持系统唤醒、关闭 EcoQoS 降频，
    // 守卫持有到 main 结束（iced 事件循环退出）才恢复。
    let _power_guard = power::ProcessPowerGuard::acquire();

    // 唤醒事件要在任何耗时初始化之前建好：第二个实例可能在配置加载/
    // 音频初始化期间就启动并期望能通知到本实例。
    tray::create_show_request_event();

    let startup_cfg = config::Config::load();
    let session = session::Session::from_config(&startup_cfg);

    // 防火墙放行：限定到本程序 + 具体端口。失败必须可被用户看到，
    // 因此同步执行并把错误作为首条提示传入界面。
    let firewall_warning = firewall::add_firewall_rule(startup_cfg.listen_port as u16).err();
    if let Some(err) = &firewall_warning {
        logf!("[Firewall] {err}");
    }

    // P2P 系统初始化：广播线程与接收流共享同一个会话实例
    net::start_broadcast_listener(Arc::clone(&session));
    net::start_broadcast_state_machine(Arc::clone(&session));
    session::init_audio_worker(Arc::clone(&session));

    // 初始化图标
    let (icon_rgba, w, h) = tray::load_icon_rgba();

    // 系统托盘（失败不致命：降级为"关窗即退出"）
    let tray_icon = tray::install_tray(
        icon_rgba.clone(),
        w,
        h,
        startup_cfg.resolved_water_target_ms(),
        &startup_cfg.language,
    );
    let tray_ok = tray_icon.is_some();
    if !tray_ok {
        logf!("[Tray] 托盘不可用，降级为关闭窗口即退出");
    }

    // 首条提示：防火墙失败 / 托盘降级，二者可能同时出现
    let startup_notice: String = match (&firewall_warning, tray_ok) {
        (Some(err), false) => i18n::tf("notice.firewall_and_tray", &[("err", err)]),
        (Some(err), true) => err.clone(),
        (None, false) => i18n::t("notice.tray_unavailable"),
        (None, true) => String::new(),
    };

    // 主窗体
    let ui_session = Arc::clone(&session);
    let result = iced::application(
        move || {
            // 复用启动时已读出的配置：boot 是 `Fn`，克隆一份即可，
            // 避免再读一次注册表（两次读取之间配置理论上可能被外部改写）。
            let cfg = startup_cfg.clone();
            let now = Instant::now();
            let state = AppState {
                footer_broadcast: i18n::tf(
                    "footer.broadcast_addr",
                    &[("port", &protocol::DISCOVER_PORT)],
                ),
                session: Arc::clone(&ui_session),
                ip_input: cfg.listen_ip.clone(),
                port_input: cfg.listen_port.to_string(),
                port_valid: true,
                is_running: false,
                connected: false,
                disconnected: false,
                last_bitrate: 0.0,
                last_level_db: -60.0,
                notice: startup_notice.clone(),
                notice_level: if firewall_warning.is_some() || !tray_ok {
                    NoticeLevel::Error
                } else {
                    NoticeLevel::Info
                },
                vb_cable_installed: audio::detect_vb_cable(),
                reverse_enabled: cfg.reverse_enabled != 0,
                config: cfg,
                last_toggle_instant: now,
                last_stats_log: now,
                pending_pair: None,
            };
            (state, Task::none())
        },
        AppState::update,
        AppState::view,
    )
    .title("UDP2CAL")
    .subscription(AppState::subscription)
    .default_font(iced::Font::with_name("Microsoft YaHei"))
    .theme(Theme::Dark)
    .window(iced::window::Settings {
        // ⚠ 此尺寸是精心设计的视觉尺寸，请勿修改。
        // 320×290 客户区 + 1px 边框 + 标题栏，成品窗口恰好是 322×322 的完美正方形；
        // 任何改动都会破坏这个正方形比例。
        size: iced::Size::new(320.0, 290.0),
        position: iced::window::Position::Centered,
        resizable: false,
        // 托盘不可用时必须让 X 真正退出：否则窗口隐藏后再也唤不回来，
        // 进程会变成只能从任务管理器结束的幽灵进程。
        exit_on_close_request: !tray_ok,
        icon: iced::window::icon::from_rgba(icon_rgba, w, h).ok(),
        ..Default::default()
    })
    .run();

    drop(tray_icon);
    result
}

impl AppState {
    /// 保存配置，失败必须留证据。
    ///
    /// 注册表写入可能被杀毒软件/组策略拦截。用 `let _ =` 吞掉错误会让用户
    /// 以为设置已持久化：重启后配置静默回退，而日志里没有任何线索。为此把
    /// 错误写进日志并提示用户（回滚语义由各调用点自己决定，这里不改配置值）。
    ///
    /// 返回是否保存成功。调用方**只在成功时**才能清空 `notice`：否则刚写的
    /// “配置保存失败”提示会被紧接着的 `notice.clear()` 抹掉，用户什么也看不到。
    fn persist_config(&mut self) -> bool {
        match self.config.save() {
            Ok(()) => true,
            Err(e) => {
                logf!("[Config] 保存配置失败: {e}");
                self.notice = i18n::tf("notice.config_save_failed", &[("err", &e)]);
                self.notice_level = NoticeLevel::Error;
                false
            }
        }
    }

    /// 配置保存成功后清理提示行：只清掉 Info 级提示。
    ///
    /// 防火墙规则添加失败等 Error 提示不能因为一次成功的配置保存就消失：
    /// 入站仍被拦截，而 release 构建没有控制台，用户将没有任何线索。
    fn clear_info_notice(&mut self) {
        if self.notice_level != NoticeLevel::Error {
            self.notice.clear();
        }
    }

    /// 回到停止态：复位会话与所有随运行态变化的界面字段。
    ///
    /// `disable_reverse` 保留 ToggleRunning 的额外副作用；提示行由调用方
    /// 按各自语义处理（停止时清空、引擎错误时写入错误文本）。
    fn reset_to_stopped(&mut self, disable_reverse: bool) {
        self.session.set_running(false);
        if disable_reverse {
            self.session.set_reverse_enabled(false);
        }
        self.session.reset_to_ready();
        self.is_running = false;
        self.pending_pair = None;
        self.session.clear_pairing_prompt();
        self.reset_connection_stats();
    }

    /// 连接状态与瞬时统计归零（启动/停止/引擎错误三条路径共用）。
    fn reset_connection_stats(&mut self) {
        self.connected = false;
        self.disconnected = false;
        self.last_bitrate = 0.0;
        self.last_level_db = -60.0;
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::IpChanged(s) => {
                self.ip_input = s;
                Task::none()
            }
            Message::PortChanged(s) => {
                self.port_input = s;
                self.port_valid =
                    self.port_input.parse::<u16>().is_ok() || self.port_input.is_empty();
                Task::none()
            }
            Message::ToggleAutoStart(enabled) => {
                // 写入失败必须回滚开关并告知用户，避免"开关假亮"
                match self.config.set_auto_start(enabled) {
                    Ok(()) => {
                        if self.persist_config() {
                            self.clear_info_notice();
                        }
                    }
                    Err(e) => {
                        self.notice = e;
                        self.notice_level = NoticeLevel::Error;
                    }
                }
                Task::none()
            }
            Message::ToggleReverse(enabled) => {
                // 运行中不可更改
                if self.is_running {
                    return Task::none();
                }
                self.reverse_enabled = enabled;
                self.session.set_reverse_enabled(enabled);
                self.config.set_reverse_enabled(enabled);
                self.persist_config();
                Task::none()
            }
            Message::SetWaterTarget(ms) => {
                self.config.water_target_ms = ms;
                if self.persist_config() {
                    self.clear_info_notice();
                }
                // 解码线程每轮读取该原子值，运行中也能立即换目标
                self.session.set_water_target_ms(ms);
                Task::none()
            }
            Message::SetLanguage(code) => {
                self.config.language = code;
                i18n::set_active(&self.config.language);
                // 语言变了，缓存的底部文案必须同步刷新（构造时已设置过语言）。
                self.footer_broadcast = i18n::tf(
                    "footer.broadcast_addr",
                    &[("port", &protocol::DISCOVER_PORT)],
                );
                if self.persist_config() {
                    self.clear_info_notice();
                }
                tray::refresh_menu(
                    self.config.resolved_water_target_ms(),
                    &self.config.language,
                );
                Task::none()
            }
            Message::ToggleRunning => {
                if self.last_toggle_instant.elapsed() < Duration::from_millis(200) {
                    return Task::none();
                }
                self.last_toggle_instant = Instant::now();

                if self.is_running {
                    self.reset_to_stopped(true);
                    self.notice.clear();
                } else {
                    let port: u16 = match self.port_input.parse() {
                        Ok(p) => p,
                        Err(_) => {
                            self.notice = i18n::t("notice.port_invalid");
                            self.notice_level = NoticeLevel::Error;
                            return Task::none();
                        }
                    };
                    if port == 0 {
                        self.notice = i18n::t("notice.port_zero");
                        self.notice_level = NoticeLevel::Error;
                        return Task::none();
                    }
                    // 44043 是常驻发现服务的绑定端口：监听它必定冲突，
                    // 而且坏端口一旦落盘，之后每次启动都会失败。
                    if port == protocol::DISCOVER_PORT {
                        self.notice = i18n::tf("notice.port_reserved", &[("port", &port)]);
                        self.notice_level = NoticeLevel::Error;
                        return Task::none();
                    }
                    // 监听地址必须是本机地址；空/非法值会让 bind 直接失败
                    let ip = self.ip_input.trim().to_string();
                    if ip.is_empty() {
                        self.notice = i18n::t("notice.ip_empty");
                        self.notice_level = NoticeLevel::Error;
                        return Task::none();
                    }
                    if ip.parse::<std::net::IpAddr>().is_err() && ip != "localhost" {
                        self.notice = i18n::tf("notice.ip_invalid", &[("ip", &ip)]);
                        self.notice_level = NoticeLevel::Error;
                        return Task::none();
                    }

                    // 端口变化必须先落实防火墙规则，再提交配置/启动监听。
                    // 旧实现异步执行并丢弃错误：非管理员时新端口被永久静默阻断，
                    // 而界面显示启动成功。失败则保留旧端口并明确提示。
                    let old_port = self.config.listen_port;
                    // 本次启动实际遇到的防火墙错误。只有它需要压过“启动成功
                    // 清空提示”的旧逻辑：入站仍被拦截，release 无日志。
                    let mut firewall_error: Option<String> = None;
                    if port as u32 != old_port {
                        if let Err(err) = firewall::add_firewall_rule(port) {
                            logf!("[Firewall] 端口 {port} 规则添加失败: {err}");
                            // add 会先删旧规则；失败时尽量把旧端口的规则补回去。
                            let restore = u16::try_from(old_port)
                                .ok()
                                .filter(|p| *p != 0)
                                .map(firewall::add_firewall_rule)
                                .transpose()
                                .err();
                            self.notice = match restore {
                                Some(re) => i18n::tf(
                                    "notice.firewall_restore_failed",
                                    &[("port", &port), ("old_port", &old_port), ("re", &re)],
                                ),
                                None => i18n::tf(
                                    "notice.firewall_failed",
                                    &[("port", &port), ("old_port", &old_port), ("err", &err)],
                                ),
                            };
                            self.notice_level = NoticeLevel::Error;
                            self.port_input = old_port.to_string();
                            return Task::none();
                        }
                    } else if let Err(err) = firewall::add_firewall_rule(port) {
                        // 同端口也要重试：上次启动/添加可能失败，或规则被组策略
                        // 清理。add_firewall_rule 先删后加，重复调用是幂等的。
                        // 失败不阻止启动（与启动路径一致），但提示必须保留到用户
                        // 看到——不能被随后的 persist 成功清掉。
                        logf!("[Firewall] 端口 {port} 规则重试失败: {err}");
                        firewall_error = Some(err);
                    }

                    self.config.listen_ip = ip;
                    self.config.listen_port = port as u32;
                    let saved = self.persist_config();
                    self.session.set_listen_port(port as u32);

                    // 上次音频初始化失败过则重试一次（例如用户刚装好 VB-Cable）
                    if matches!(
                        self.session.audio_state(),
                        session::AudioInitState::DecoderFailed
                            | session::AudioInitState::DeviceFailed
                    ) {
                        session::init_audio_worker(Arc::clone(&self.session));
                    }

                    self.session.set_running(true);
                    self.session.reset_to_ready();
                    self.is_running = true;
                    self.reset_connection_stats();
                    if saved {
                        match firewall_error {
                            Some(err) => {
                                self.notice = err;
                                self.notice_level = NoticeLevel::Error;
                            }
                            None => self.notice.clear(),
                        }
                    }
                    self.vb_cable_installed = audio::detect_vb_cable();
                    // 应用反向串流设置到全局
                    self.session.set_reverse_enabled(self.reverse_enabled);
                }
                Task::none()
            }
            Message::StatusUpdate(info) => {
                // 停止后接收任务可能还有一条已在途的统计：不能把码率/电平
                // 写回“已停止”的界面。DeviceFailed 自检也只在运行态有意义。
                if !self.is_running {
                    return Task::none();
                }
                // 解码线程重建输出流失败会置 DeviceFailed；此时必须停表并提示，
                // 不能维持“运行中”假象（played/设备时钟已冻结，永远无声）。
                if self.session.audio_state() == session::AudioInitState::DeviceFailed {
                    return self.update(Message::EngineError(
                        i18n::t("engine.audio_stream_failed"),
                    ));
                }
                // 「已连接」= 控制面绑定存在。规范 6.4 明确保活只看控制面，
                // 不得用"是否收到 RTP"判断存活；历史实现只在 bitrate>0 时置真、
                // 从不置假，导致 RTP 停止后码率归零而界面仍显示已连接。
                let bound = self.session.p2p().is_busy();
                if bound != self.connected {
                    self.connected = bound;
                    if bound {
                        // 音频真正流动起来后清掉历史错误与断开标记。
                        // 只清 Info：配置保存失败等 Error 提示必须保留，
                        // 否则用户重启后发现端口/开关静默回退且无线索。
                        if self.notice_level == NoticeLevel::Info {
                            self.notice.clear();
                        }
                        self.disconnected = false;
                    } else if self.is_running {
                        self.last_bitrate = 0.0;
                        self.last_level_db = -60.0;
                        self.disconnected = true;
                    }
                }
                self.last_bitrate = info.bitrate_kbps;
                self.last_level_db = info.level_db;
                // 诊断：每 30s 落一次盘（音频计数器 + RTP/抖动缓冲计数器）
                if self.last_stats_log.elapsed() >= Duration::from_secs(30) {
                    self.last_stats_log = Instant::now();
                    diag::log_periodic_stats(&self.session);
                }
                Task::none()
            }
            Message::Disconnected => {
                if self.is_running && self.connected {
                    self.connected = false;
                    self.last_bitrate = 0.0;
                    self.last_level_db = -60.0;
                    // 只更新顶部状态点（● 已断开），底部提示行不再重复同一信息
                    self.disconnected = true;
                }
                Task::none()
            }
            Message::EngineError(msg) => {
                // 引擎级失败：必须回到停止态，否则界面会一直"假运行"
                self.reset_to_stopped(false);
                self.notice = msg;
                self.notice_level = NoticeLevel::Error;
                Task::none()
            }
            Message::PeerStateChanged => Task::none(),
            Message::PairingRequest(req) => {
                // 停止后迟到的配对请求：不弹窗、不写配对。同时释放控制面的
                // 提示槽，避免下次启动时该设备因 prompted != 0 永远被 Ignore。
                if !self.is_running {
                    self.pending_pair = None;
                    self.session.clear_pairing_prompt();
                    return Task::none();
                }
                if self.pending_pair.is_none() {
                    logf!(
                        "[P2P] 未配对设备 {:016x}（{}）请求连接，等待本机确认",
                        req.device_id,
                        req.addr.ip()
                    );
                    self.pending_pair = Some(req);
                } else {
                    // UI 已有待确认弹窗时丢弃新请求，但必须释放控制面的提示槽：
                    // 该槽已属于这个被丢弃的请求，不清会让后续设备永远 Ignore。
                    self.session.clear_pairing_prompt();
                }
                Task::none()
            }
            Message::AllowPairing => {
                if let Some(req) = self.pending_pair.take() {
                    self.config.paired_device = req.device_id;
                    self.config.paired_device_ip = Some(req.addr.ip());
                    self.session.set_paired_device(req.device_id, req.addr.ip());
                    self.session.clear_pairing_prompt();
                    // 保存失败时 persist_config 已写入错误提示，不能覆盖掉。
                    if self.persist_config() {
                        self.notice = i18n::tf(
                            "notice.pair_allowed",
                            &[
                                ("ip", &req.addr.ip()),
                                ("id", &format!("{:016x}", req.device_id)),
                            ],
                        );
                        self.notice_level = NoticeLevel::Info;
                    }
                    logf!(
                        "[P2P] 用户已允许设备 {:016x}（{}）",
                        req.device_id,
                        req.addr.ip()
                    );
                }
                Task::none()
            }
            Message::DenyPairing => {
                if let Some(req) = self.pending_pair.take() {
                    self.session.deny_pairing(req.device_id);
                    self.session.clear_pairing_prompt();
                    self.notice = i18n::tf(
                        "notice.pair_denied",
                        &[
                            ("ip", &req.addr.ip()),
                            ("id", &format!("{:016x}", req.device_id)),
                        ],
                    );
                    self.notice_level = NoticeLevel::Info;
                    logf!(
                        "[P2P] 用户已拒绝设备 {:016x}（{}）",
                        req.device_id,
                        req.addr.ip()
                    );
                }
                Task::none()
            }
            Message::HideWindow => {
                ui::latest_window_task(|id| iced::window::set_mode(id, iced::window::Mode::Hidden))
            }
            Message::ShowWindow => ui::latest_window_task(|id| {
                // 解除最小化必须走 iced（winit）而不是裸 ShowWindow：后者绕过
                // winit 的可见性/最小化标志，可能再度造成状态失同步（"X 失效"）。
                // 顺序：先展开最小化，再确保可见，最后抢前台。
                Task::batch([
                    iced::window::minimize(id, false),
                    iced::window::set_mode(id, iced::window::Mode::Windowed),
                    iced::window::gain_focus(id),
                ])
            }),
            Message::Quit => iced::exit(),
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let close_events = iced::event::listen_with(|event, _status, _id| {
            if let iced::Event::Window(window_event) = event {
                if matches!(window_event, iced::window::Event::CloseRequested) {
                    Some(Message::HideWindow)
                } else {
                    None
                }
            } else {
                None
            }
        });

        let mut subs = vec![
            close_events,
            Subscription::run_with((), |_| tray::tray_event_stream()),
            // id 不能与托盘流相同：run_with 的去重身份只由 id 决定，
            // 两个 `()` 会被 iced 合并成一条流。
            Subscription::run_with("show-request", |_| tray::show_request_stream()),
        ];

        if self.is_running {
            let key = net::ReceiverKey {
                session: Arc::clone(&self.session),
                listen_ip: self.config.listen_ip.clone(),
                listen_port: self.config.listen_port,
            };
            subs.push(Subscription::run_with(key, net::receiver_stream));
        }

        Subscription::batch(subs)
    }
}
