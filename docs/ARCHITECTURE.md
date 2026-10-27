# 系统架构与实现细节

> 相关文档：[协议规范](PROTOCOL_V3_RTP.md) · [控制面审查归档](archive/CONTROL_PLANE_REMEDIATION.md) · [构建说明](BUILD.md) · [功能特性](FEATURES.md) · [延迟校准](LATENCY_CALIBRATION.md)

---

## 一、项目结构

```
udp2cal/
├─ android/        # Android 发送端（Kotlin + JNI Opus，Android 10+）
│  └─ app/src/main/
│     ├─ java/com/udp2cal/app/
│     │  ├─ MainActivity.kt          # Compose UI + 运行时权限申请
│     │  ├─ DiscoveryManager.kt      # 局域网广播发现
│     │  ├─ UdpSender.kt             # 主 socket：CONNECT + 正向 RTP；独立 ACK 线程；反向 socket 只收 RTP
│     │  ├─ Udp2CalProtocol.kt       # 协议常量与 DTO 胶水（行为全部由 Rust 提供，见「协议单一实现」）
│     │  ├─ AudioPlayer.kt           # 反向播放（48kHz 立体声，最小缓冲 + low-latency）
│     │  ├─ StreamResampler.kt       # 反向流的受控重采样（32 相 × 48 抽头多相 FIR）
│     │  ├─ Prefs.kt                 # SharedPreferences 配置
│     │  ├─ service/CaptureService.kt# 前台采集服务（采集/控制/反向接收/反向播放 四路并发，无 PCM 队列）
│     │  └─ native/Opus*.kt          # Opus 编码器/解码器 JNI 封装
│     │     native/ProtocolNative.kt # 协议层的 external fun 声明（加载 libudp2cal_protocol.so）
│     └─ cpp/                        # opus_jni.c / opus_jni_decoder.c（JNI 封装）
├─ opus-src/       # libopus 1.6.1 源码树（Android 与 Windows 共用的唯一副本）
├─ windows/        # Windows 接收端（Rust + iced + cpal）
│  ├─ src/
│  │  ├─ main.rs       # iced UI、托盘、解复用、控制状态机、解码线程（含两级漂移校正）
│  │  ├─ audio.rs      # cpal 输出 + 无锁 PCM 环 + played_frames + 可微调比率重采样
│  │  ├─ capture.rs    # WASAPI Loopback（事件驱动）+ Opus 10 ms + RTP 发送
│  │  ├─ config.rs     # 注册表配置读写
│  │  ├─ decoder.rs    # Opus 解码器封装（48 kHz 单声道，480 采样/帧）
│  │  ├─ firewall.rs   # 防火墙规则
│  │  └─ protocol.rs   # 协议重新导出
│  └─ Cargo.toml
├─ protocol/       # 协议真源（Rust crate）：protocol.rs 控制面 + rtp.rs 媒体面 + jni.rs JNI 表面
├─ docs/           # 项目文档（本目录）
├─ build.bat       # 统一构建入口（Windows / Android / ADB / 体检）
├─ scripts/        # 构建脚本实现（Python 3.8+）
│  ├─ build.py
│  └─ udp2cal/     # 环境探测、依赖补装、Gradle/Cargo 封装
├─ icon.png        # 图标源文件（构建期嵌入 EXE，不可移动）
├─ LICENSE
└─ README.md
```

---

### 协议单一实现：Rust + JNI（v1.5.0）

v1.4.0 及之前的 `Udp2CalProtocol.kt` 是 `protocol/rtp.rs` + `protocol/protocol.rs` 的手工移植副本
（1059 行）。两份实现靠人眼对照，改一处忘一处就会以"一端能连、另一端静音"的形式暴露。v1.5.0 起
协议行为只有一份：

| 位置 | 角色 |
| --- | --- |
| `protocol/protocol.rs`、`protocol/rtp.rs` | **唯一实现**（控制面 / RTP / 抖动缓冲 / 水位与比率控制） |
| `protocol/jni.rs` | `#[cfg(feature = "jni")]` 的 JNI 表面，60 个 `Java_com_udp2cal_app_native_ProtocolNative_*` 符号 |
| `android/.../native/ProtocolNative.kt` | `external fun` 声明 + `System.loadLibrary("udp2cal_protocol")` |
| `android/.../Udp2CalProtocol.kt` | 只剩常量、DTO 打包/解包与 `get()` 转发（492 行） |

三个接线点：

1. **句柄**：`JitterBuffer` / `StepAbsorber` / `RateEstimator` / `DriftController` 在 Kotlin 侧只是一个
   `Long`；Rust 用 `Box::into_raw` 造对象、`*Destroy` 收回。`JitterBuffer` 会被接收线程
   （`insert`）与播放线程（`pull`）同时使用，故内部包一层 `Mutex`——纯 Kotlin 版本在这里是
   数据竞争。Kotlin 侧 `AutoCloseable.close()`（句柄清零）+ `finalize()` 兜底
   （minSdk 29 没有 `java.lang.ref.Cleaner`），`CaptureService` 在停机时主动 `close()`。
2. **时钟桥**：Rust 的到达时刻用 `Instant`，Kotlin 用 `System.nanoTime()`。`clockSync(System.nanoTime())`
   在库加载时记下 `(Instant, java_nanos)` 基准，`arrivalNanosAt` 与 `RateEstimator::observe` 双向换算——
   两端都是单调时钟，不需要 `GetJavaVM`。
3. **常量**：Kotlin 仍需 `const val`（编译期），但值由测试看守：`nativeConstantsMatchKotlinConstants`
   与 `nativeRealConstantsMatchKotlinConstants` 把 Rust 导出的 43 个整数 + 8 个浮点常量逐项比对。

构建接线（`android/app/build.gradle.kts`）：

- `cargoBuildAndroidProtocol`（Exec）：用 NDK 的 `clang.exe` 交叉编译
  `protocol/ --features jni --target aarch64-linux-android`，链接器经
  `CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER` + `CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS`
  （`-C link-arg=--target=aarch64-linux-android29 -C link-arg=--sysroot=…`）指定；用 per-target 变量
  是为了不覆盖用户自己的 `RUSTFLAGS`；
- `syncRustJniLibs`（Sync）：把 `libudp2cal_protocol.so` 放到 `build/rustJniLibs/<abi>/`，`preBuild` 依赖它；
- `cargoBuildHostProtocol`（Exec）：宿主 `cargo build --release --features jni` 产出
  `udp2cal_protocol.dll`，单元测试用 `java.library.path` 指向 `protocol/target/release`——
  **同一套符号直接跑在 JVM 上**。

好处不只是"少写代码"，而是**测试也统一了**：`Udp2CalProtocolTest` 的用例验证的是 Rust 实现，
桌面 JVM 与 Android 真机跑的是同一份机器码，协议回归不再需要"两端各跑一遍再人工比对"。

**边界：共享的是线格式与协议算法，不是连接状态机或身份安全。** Android 的 ACK 生命周期在
`UdpSender.kt` / `CaptureService.kt`，Windows 的会话绑定在 `windows/src/main.rs`；设备选择、重连、超时、
端点更新和安全决策仍可能出现平台差异。v3 的 `device_id` 是公开标识而非凭证；更详细的差异和整改状态见
[控制面审查归档](archive/CONTROL_PLANE_REMEDIATION.md)。

---

## 二、技术栈

| 层级 | 选型 | 关键实现要点 |
| --- | --- | --- |
| UI（Android） | Jetpack Compose + Material 3 | 全参数设置面板 + 状态卡片；卡片显示**已生效**值（编码器上报），不显示"期望值" |
| UI（Windows） | iced 0.14（tiny-skia 软件渲染，禁用 wgpu） | 固定窗口、深色卡片、实时电平；单行状态栏按连接状态切换 |
| 网络 | Kotlin Coroutines / Rust tokio | 控制包与正向 RTP **共用一个 socket**，靠首字节解复用；Windows 防火墙仅对 Private profile 放行 |
| 采集（Android） | `AudioRecord` 48 kHz 单声道 | `READ_BLOCKING` 累积到整帧再编码；语音模式决定音源与 AEC（创建时定型） |
| 采集（Windows） | WASAPI Loopback，事件驱动 | 10 ms 组帧 + 1 帧节拍整形，不做应用层积压 |
| 编码 | libopus 1.6.1（JNI / audiopus，双端同一份 `opus-src`） | Deep PLC；DRED 不启用（接收端无解析/恢复路径）；负载上限 1275 字节，超限丢帧不截断 |
| 时钟 | 三个时间参考 | RTP 媒体时间、接收端 QPC/`Instant`、声卡设备进度（Windows `IAudioClock::GetPosition`，Android 播放头）；映射规则见协议 7.9/7.10 |
| 输出（Windows） | cpal 0.15 / WASAPI | 无锁 SPSC 环（341 ms）+ 显式 10 ms 设备缓冲 + 事件驱动回调 |

> 这里刻意不列"版本号矩阵"之外的运行时策略：那些属于会随实现漂移的细节，
> 写在这里必然过时。行为以代码与协议规范为准。

---

## 三、Windows 端依赖

### 构建依赖（`[build-dependencies]`）

| Crate | 用途 |
|-------|------|
| `winres` | 将 ICO 资源嵌入 EXE 文件 |
| `image` | 构建时将 `icon.png` 转为 256×256 ICO |

`build.rs` 流程：`icon.png` → Lanczos3 缩放 256×256 → `icon.ico` → `winres::WindowsResource` 嵌入 EXE PE 资源。

### 运行时依赖（`[dependencies]`）

| Crate | 版本 | 用途 |
|-------|------|------|
| `udp2cal-protocol` | local | 局域网 UDP 协议（包头编解码、重排序缓冲区） |
| `iced` | 0.14 | GUI 框架（tiny-skia 软件渲染，tokio 运行时） |
| `tokio` | 1.40 | 异步 UDP 收发、超时控制、事件流 |
| `cpal` | 0.15 | WASAPI 音频播放 |
| `audiopus` | 0.3.0-rc.0 | Opus 音频编解码 |
| `windows` | 0.58 | Win32 API（单实例互斥、窗口控制、COM/WASAPI） |
| `winreg` | 0.52 | 注册表配置持久化 |
| `tray-icon` | 0.24 | 系统托盘图标、右键菜单、双击恢复 |
| `image` | 0.24 | 运行时解码内嵌 `icon.png` 为 RGBA |

### 关键依赖配置

```toml
[profile.release]
opt-level = 3         # 实时音频路径优先速度，体积换性能不划算
lto = true            # 链接时优化
codegen-units = 1     # 单代码生成单元
panic = "unwind"      # Panic 展开
strip = "symbols"     # 剥离调试符号

[dependencies]
iced = { version = "0.14", default-features = false, features = ["tokio", "tiny-skia", "image"] }
tokio = { version = "1.40", features = ["net", "sync", "rt", "macros", "time"] }
windows = { version = "0.58", features = [
    "Win32_Security", "Win32_System_Com", "Win32_System_LibraryLoader",
    "Win32_System_Threading", "Win32_System_Registry",
    "Win32_UI_WindowsAndMessaging", "Win32_UI_Shell",
    "Win32_UI_Input_KeyboardAndMouse", "Win32_Graphics_Gdi",
    "Win32_Foundation", "Win32_Media_Audio", "Win32_Media_KernelStreaming",
] }
```

> `iced` 必须禁用默认特性（避免 wgpu 后端引发 `windows` crate 版本冲突），显式启用 `tokio` + `tiny-skia` 渲染器。
>
> `tokio` 必须显式启用 `time` 和 `net` 特性，否则 `tokio::time::sleep` 和 `tokio::net::UdpSocket` 不可用。

---

## 四、v3 的"固定 + 热同步"机制

v3 大幅削减了可协商项：**帧长、采样率、声道数、payload type 全部固定**，
控制面只同步"反向编码器参数"，其余由各自本地决定。

| 项目 | v3 取值 | 同步方式 |
| --- | --- | --- |
| 正向编码（手机麦克风） | 48 kHz / 单声道 / 10 ms | 手机本地决定，PC 不需要知道 |
| 反向编码（PC 环回） | 48 kHz / 立体声 / 10 ms | CONNECT 携带 Opus 参数，PC **仅在变化时**应用 |
| 采样率 | 48 kHz 固定 | 不协商；PC 输出设备非 48 kHz 由 `AudioWriter` 按固定比率重采样 |
| 帧长 | 10 ms 固定（480 采样） | `frame_ms != 10` 直接拒绝连接 |
| 码率 | 五档 64/128/256/384/512 kbps（默认 256，上限 512）；`0` 仅作协议兼容 | `clamp_bitrate_kbps()` / `snap_bitrate_kbps()` 两端一致 |
| 传输 | 服务端口 UDP，控制包 + 正向 RTP 同 socket | 首字节解复用 |
| 接收端 | 单方向一个 `JitterBuffer` + 一个解码实例 | 不需要"5 个采样率解码器" |
| 发现 | Windows 监听 44043，DISCOVER_REPLY 带 `listen_port` | Android 用该端口连接，不再假定 44044；IPv4 广播仅覆盖本地广播域 |
| 保活 | Android 每 1 s CONNECT；PC 4.5 s 无 CONNECT 释放绑定（比 Android 3 s ACK 窗口多一次重传余量）；Android 3 s 无 ACK 断连 | **只看控制面**，不看 RTP |
| 模式切换 | 语音↔音乐重建 AudioRecord/AudioTrack，网络层与 SSRC 一并重建 | 允许短暂中断，不做"保网络层"的特殊分支 |

> 反向编码器"仅在变化时应用"是为了避免不必要的瞬时重配开销；并不是因为每个
> `OPUS_SET_*` 都会重置编码器——真正重置状态的是 `OPUS_RESET_STATE` 与少数隐式模式切换。

---

## 五、运行时行为

| 操作 | 行为 |
|------|------|
| 点击关闭按钮 (X) | 隐藏窗口到系统托盘，任务栏消失（**托盘不可用时改为直接退出**） |
| 点击最小化按钮 (—) | 最小化到任务栏 |
| 左键单击托盘图标 | 无操作（仅右键显示菜单） |
| 双击托盘图标 | 恢复并置顶主窗口 |
| 托盘右键 → 退出 | 退出程序 |

### 日志

发布版是 GUI 子系统，没有控制台，`eprintln!` 输出不可见。所有运行期事件统一写入：

```
%LOCALAPPDATA%\UDP2CAL\udp2cal.log
```

- 单文件上限 2 MiB，超限后轮转为 `udp2cal.1.log`（只保留一轮）
- 记录：启动版本、防火墙结果、托盘降级、音频初始化、P2P 绑定/SSRC/反向端口变化、
  反向编码配置变更、发送错误计数、每 30 s 一次的音频统计（块数/重采样/丢样/欠载、
  总延迟与比率微调）与 RTP 统计（收包/迟到/重复/溢出/重同步/PLC/水位/SSRC）、
  每 10 s 一行的水位分解（抖动缓冲 + 解码环 + 声卡）、比率与反馈/前馈量
- UI 底部为**单行**状态行，按连接状态切换内容（三种状态互斥，永远只占一行）：
  - 未启动：`广播地址: 255.255.255.255:44043`
  - 已启动未连上：`广播地址…` + `● 广播中`（被占用静默时显示 `● 已静音`）
  - 已连接：`延迟 N ms · 丢样 N · 欠载 N` + `● 反向串流已启用`（反向关闭时 `○ 反向串流未启用`）
  - 「延迟」= 正在播放那一帧的"到达本机 → 出声"时长，口径见协议 7.14
- 连接状态（已连接/已断开）**只在顶部状态点显示**，底部提示行仅承载错误与警告，避免同屏重复
- 排查用户问题时优先索取该文件

### UI 状态指示

| 指示器 | 含义 |
|--------|------|
| `● 等待连接 / ● 已断开` | 红色 — 正在监听但无数据流 |
| `● 已连接` | 绿色 — 收到音频数据 |
| `● 已占用` | 橙色 — TYPE_CONNECT 独占绑定，异设备 CONNECT 将被静默拒绝（不显示设备识别码） |
| `● 空闲` | 绿色 — 已启动但无可信连接 |
| `● VB-Cable` / `● VB-Cable 未安装` | 绿色/橙色 — 虚拟声卡检测状态 |
| `● 广播中` / `● 已静音` | 绿色/灰色 — 广播状态机 Ready/Silent |
| `[停止] [开机自启] [反向串流]` | 反向串流按钮 — 停止时可用点击切换，运行时锁定。开启后显示 `● 反向串流已启用` |

### 配置持久化

- 配置存储在注册表 `HKCU\Software\UDP2CAL`
- 监听 IP、端口在点击「启动」时保存；设备 ID 首次运行时自动生成（8 字节 u64）
- 开机自启开关保存到 `HKCU\...\Run` 键 + 配置键
- 反向串流开关在停止时切换并保存到注册表，Win 重启后自动恢复
- 每次启动均从停止状态开始（`is_running` 为运行时状态，不持久化）

### 局域网自动发现

- Windows 端常驻监听 UDP 端口 44043（独立线程 `start_broadcast_listener`）
- **v3 控制包**：手机端 `TYPE_DISCOVER_REQ`（空负载）→ PC 回复 `TYPE_DISCOVER_REPLY`（`listen_port` + 设备名 ≤254 字节，UTF-8 不截断字符）
- **广播状态机**（`start_broadcast_state_machine`）：READY 时每秒向 `255.255.255.255:44043` 广播 `TYPE_DISCOVER_REPLY`；BUSY 时自动静音
- `listen_port` 取自当前监听端口（`LISTEN_PORT` 原子缓存，用户修改后立即生效）

### 图标

- `icon.png` 通过 `include_bytes!` 编译期嵌入 EXE
- 同时用作窗口图标和托盘图标
- `build.rs` 另将 `icon.png` 转为 ICO 嵌入 EXE 文件资源（资源管理器图标）
- 若 `icon.png` 缺失或无法解码，回退到程序绘制的 32×32 绿色圆点

### P2P 防抢占拒绝行为

当 Windows 端处于 `DEVICE_BUSY`（已绑定一台 Android）时，第三方设备手动输入 IP:端口发送 `TYPE_CONNECT`：

| 角色 | 行为 |
|------|------|
| **Windows 端** | `device_id` 不匹配 → 不回复 `CONNECT_ACK`，静默丢弃；`P2P_STATE: Mutex<P2pState>` 记录的绑定关系（设备 ID / 源地址 / 双向 SSRC）**完全不变** |
| **第三方 Android 端** | 收不到新 `CONNECT_ACK` → `isAckFresh()` 恒为 false → **不编码不发送任何正向 RTP**；持续发 CONNECT 等待（由 Win 端静默丢弃，不影响已有连接） |
| **已有 Android 端** | 正常收到 `CONNECT_ACK`（device_id + forward_ssrc 匹配保活分支），RTP 正常解码播放，**完全不受影响** |

> 唯一例外：极低概率的 8 字节 device_id 碰撞会导致 Windows 误判为同设备保活，回复 `CONNECT_ACK` 到第三方。但由于第三方无合法音频采集流，仍无法发送音频数据。
>
> **威胁模型**：`device_id` 随 `DISCOVER_REPLY` 每秒明文广播，本机制只用于"防误连、防抢占"，**不具备抗伪造能力**——同网段的恶意主机嗅探到 ID 后可冒充绑定设备。需要抗伪造时应在 `CONNECT` 中引入一次性 nonce 或预共享密钥 HMAC。

### 释放独占的途径

1. **Windows 端点击「停止」** → `reset_to_ready()`：清空绑定、停止反向发送、复位抖动缓冲
2. **绑定设备 4.5 s 没有 CONNECT**（`PEER_TIMEOUT_MS`，比 Android 的 3 s ACK 窗口多一次重传余量）→ 接收流的超时分支自动释放，回到 Ready
3. **`forward_ssrc` 变化** → 视为新会话：重置抖动缓冲/解码器/播放头，保留绑定但换反向 SSRC
4. **重启 Windows 程序** → 全局状态初始化回到 Ready

> 判活只看控制面：DTX 静音期间可能长时间没有 RTP，用"是否收到音频"判活会误杀连接。

## 六、Windows 端架构精要（v3）

### 数据流

```
[Android] ──RTP/Opus(PT=111, 10 ms)──→ [Windows 主 socket]
                                            │ 首字节解复用
                    ┌───────────────────────┴──────────────────────┐
                    │ 0x80..0xBF → RTP                             │ 0x03 → 控制
                    ▼                                              ▼
          校验 源地址 + SSRC + PT                          CONNECT / DISCOVER
                    │                                              │
                    ▼                                              ▼
          JitterBuffer（网络线程只插入）                    绑定状态机 + ACK
                    │ unpark                                       │
                    ▼                                              │
   解码线程（按播放头拉取 → Opus 解码 / 10 ms PLC / 水位校正）        │
                    │ 写无锁 PCM 环（预解码深度 ≤40 ms）             │
                    ▼                                              │
   cpal 回调（pop PCM、补静音、played_frames += n、unpark）           │
                    │                                              │
                    ▼                                              │
            VB-Cable → 微信 / Zoom / OBS / 游戏                     │
                                                                   │
   [反向] WASAPI Loopback → 10 ms 组帧 → Opus → RTP(SSRC 来自 ACK) ──┘
```

### 线程与职责

| 线程 | 职责 | 明令禁止 |
| --- | --- | --- |
| tokio 接收流（iced subscription） | `recv_from`、解复用、校验、插抖动缓冲、回 ACK | 解码、阻塞等待播放 |
| `udp2cal-decode` | 按播放头拉取、解码/PLC、两级漂移校正、重采样、写环 | 网络 IO、用定时器当节拍 |
| `udp2cal-audio`（cpal 回调） | pop PCM、补静音、发布 `played_frames` 与声卡延迟、unpark | 加锁、分配、调用 Opus、写日志 |
| `reverse-capture` | WASAPI 环回 → 10 ms 组帧 → 1 帧节拍整形 → Opus → RTP | 应用层积压、等待 ACK |
| 广播 / 发现线程 | 每秒 `DISCOVER_REPLY`、应答 `DISCOVER_REQ` | 触碰音频路径 |

### 关键实现点（规则本身见协议第七章）

> 参数值（目标水位、PI 增益、前馈窗、比率上限）、状态机、算法步骤一律以
> [PROTOCOL_V3_RTP.md](PROTOCOL_V3_RTP.md) 第七章为准，本节**不复制**——
> 两处各写一份的结果就是慢慢跑偏。

| 机制 | 位置 | 为什么必须这样 |
| --- | --- | --- |
| 水位只用**一个口径** | `main.rs` 解码线程：`water = jb.water_ms() + ring_ms + device_latency_ms` | 抖动缓冲、解码环、声卡缓冲三段都是"可听点之前压着的媒体量"，加在一起才等于真实排队时长；分开算就会互相打架，需要偏移记账去补 |
| 设备延迟实测 | `audio.rs` 回调：`timestamp().playback − .callback` | 声卡自报，是唯一可信来源。请求的缓冲值不可信：cpal 0.15.3 把帧数换算成 `hnsBufferDuration` 时少乘 100 |
| 预解码深度 ≤40 ms | `rtp::JitterBuffer::pull`：`ring_ms >= DECODE_LIMIT_MS` 就返回 `Wait` | 不设上限时，断流期解码线程会以 CPU 速度灌 PLC，`next_ts` 跑到可听点前面几百毫秒，恢复后真包全被判迟到 |
| 回调唤醒解码线程 | `audio.rs`：`thread.unpark()` | 播放头前移本身就是"该拉下一帧"的信号，不需要定时器 |
| 前馈管设备钟差 + 反馈管长期残差 | `rtp::DeviceRateEstimator` / `rtp::DriftController` | 前馈观测 `(设备帧, QPC)`，测的是媒体时钟相对**声卡设备时钟**的频差；反馈对 4 s 低通后的水位做慢 PI，只修长期残差。网络攒批不进入前馈，因此不会变成变调 |
| 没有播放头，也没有偏移记账 | 旧 `PlayheadClock`（base/余数/offset/`note_*`）已删除 | 一旦存在"设备帧↔媒体时间戳"换算，就必须记住偏移；改为纯水位口径后，`played_frames` 不参与任何播放判定，换算链与三处补丁一起消失 |
| flush 只投递请求 | `audio.rs::Ring::request_flush` | `tail` 归消费者所有；生产者直接写会让 `len()` 回绕成 `usize::MAX`，解码线程永久停产（静默死锁） |
| 1 帧节拍整形 | `capture.rs`：发送时刻钉在 10 ms 网格 | 环回事件成簇交货，直接发会让接收端水位呈锯齿；落后 >4 帧重置而非补发 |

### 快速双击防抖

`ToggleRunning` 入口检查 200ms 防抖，避免旧 socket 未完全释放时新 socket 绑定失败。同时输入框 `PortChanged` 实时校验 `parse::<u16>()`，非法输入时边框变红。

```rust
if self.last_toggle_instant.elapsed() < Duration::from_millis(200) {
    return Task::none();
}
```

---

## 七、命名约定

| 项目 | 值 |
| --- | --- |
| 项目/EXE/APK | `UDP2CAL` / `udp2cal.exe` / `udp2cal.apk` |
| Rust crate | `udp2cal` / `udp2cal-protocol` |
| 协议 lib 名 | `udp2cal_protocol` |
| 注册表 | `HKCU\Software\UDP2CAL` |
| 开机自启 Run 键 | `UDP2CAL` |
| 防火墙规则 | `UDP2CAL 局域网音频串流` |
| 单实例互斥锁 | `UDP2CAL_SingleInstance_Mutex` |
| 发现服务端口 | `44043` |
| Android 包名 | `com.udp2cal.app` |

---

## 八、相关文档

| 文档 | 内容 |
| --- | --- |
| [PROTOCOL_V3_RTP.md](PROTOCOL_V3_RTP.md) | **v3 协议规范（现行）**：RTP 媒体面 + 控制面 + 时钟模型 |
| [BUILD.md](BUILD.md) | 构建流程与产物 |
| [FEATURES.md](FEATURES.md) | 功能特性与 Opus 参数 |
| [LATENCY_CALIBRATION.md](LATENCY_CALIBRATION.md) | 延迟口径与实测校准流程 |
| [CHANGELOG.md](CHANGELOG.md) | 版本历史 |
| [BUGFIX.md](BUGFIX.md) | 现行代码的 Bug 根因档案 |
| [archive/](archive/) | 历史存档：v2 协议、已完成迁移计划、历次审查报告、v3 之前的修复记录 |
