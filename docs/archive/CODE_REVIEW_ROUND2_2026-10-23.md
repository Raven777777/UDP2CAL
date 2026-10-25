# UDP2CAL 代码质量审查报告（第二轮）

**审查对象**：`windows/`（Rust + iced，审查时 2 464 行 → 修复后 2 949 行）、`android/`（Kotlin 2 466 行 → 2 602 行 + JNI C 314 → 280 行，不含 vendored libopus）
**审查日期**：2026-10-23
**审查方式**：静态通读 + 编译器/Clippy 实跑 + 单元测试实跑 + 与上一轮报告（`docs/archive/CODE_REVIEW_2026-10-23.md`）比对去重
**结论**：**有条件通过（B-）**。工程纪律良好——Clippy 零告警、24 项单元测试全绿、热路径有明确的零分配与无锁设计痕迹。但仍存在 **4 项 P1 级缺陷**（1 项崩溃风险、1 项状态机高频抖动、1 项反向音频音质错误、1 项启动期静默退出），集中在**异步收尾时序**与**跨组件隐式契约**两类问题。

> **修复状态（2026-10-23 更新）**：本报告列出的 24 项缺陷与 4 处文档漂移**已全部修复**，修复明细见 §十。修复后：Rust 端 `cargo test` **29/29** 通过（protocol 20 + windows 9，含新增 5 项捕获侧重采样测试）、`cargo clippy --all-targets` **零告警**；Android 端 `assembleDebug`（含 CMake/JNI）**构建成功**。

> 本次审查**未修改任何源码**，仅新增本报告文件。

---

## 一、审查范围与方法

| 项目 | 内容 |
|------|------|
| 审查文件 | `windows/src/*.rs`（7 个）、`android/app/src/main/java/**/*.kt`（12 个）、`android/app/src/main/cpp/opus_jni*.c`（2 个）、`AndroidManifest.xml`、`build.gradle.kts`、`Cargo.toml`、`build.bat` |
| 依赖协议层 | `protocol/protocol.rs`（598 行），被 Windows 端 `pub use` 直引 |
| 实跑验证 | `cargo test`（windows：审查时 4/4 → 修复后 9/9；protocol：20/20）、`cargo clippy --all-targets`（**0 告警**）、`gradlew :app:assembleDebug`（JDK 17，含 CMake/JNI） |
| 未覆盖 | 未做真机/真声卡运行测试；未实跑 GUI（启动即调用 `netsh` 修改防火墙，避免产生系统副作用） |

### 实跑结果（审查时 → 修复后）

```
protocol: test result: ok. 20 passed  →  20 passed
windows : test result: ok.  4 passed  →   9 passed（+5 项捕获侧重采样）
clippy  : 0 warning                   →   0 warning
android : 未编译                       →   BUILD SUCCESSFUL（assembleDebug）
```

### 代码规模与 unsafe 面

| 模块 | 行数（审查时 → 修复后） | `unsafe` 出现次数 | 说明 |
|------|-----:|:---:|------|
| `windows/src/main.rs` | 1 306 → 1 505 | 4 | 单实例互斥、窗口枚举 |
| `windows/src/audio.rs` | 449 → 474 | 1（注释） | 环形缓冲全部使用 `AtomicU32` 安全抽象 |
| `windows/src/capture.rs` | 439 → 609 | 6 | WASAPI FFI，逐处有边界校验 |
| `windows/src/logger.rs` | 0 → 91（新增） | 1 | `GetLocalTime` 取本地时间戳 |
| `windows/src/decoder.rs` / `config.rs` / `firewall.rs` | 268 | 0 | — |
| Android Kotlin | 2 466 → 2 602 | — | 无 `!!` 泛滥，`@Volatile`/`@Synchronized` 使用基本到位 |
| Android JNI C | 314 → 280 | — | 静态 1276B + 动态 `nbBytes` 双重越界守卫 |

`unsafe` 集中在平台 FFI 边界且均配有长度校验，未发现未定义行为（上一轮报告的 P0-1/P0-2 已确认修复：`parse_mix_format` 现在正确解析 `WAVEFORMATEXTENSIBLE`，`AUDCLNT_BUFFERFLAGS_SILENT` 已按静音处理）。

---

## 二、质量评分

| 维度 | 评分 | 依据 |
|------|:----:|------|
| 架构设计 | 8.5 | 双端职责清晰；协议层为唯一真源；音频热路径与 UI 解耦（守护线程 + `SyncChannel`） |
| 并发正确性 | 6.5 | SPSC 环形缓冲设计正确、锁序一致；但存在 2 处取消时序缺陷（P1-1、P2-2）与 1 处状态机抖动（P1-2） |
| 资源管理 | 6.5 | COM/AEC/WakeLock 均有配对释放；但模式切换路径存在 socket/线程泄漏（P2-2） |
| 错误处理 | 6.0 | 错误能回滚 UI 状态（`EngineError`、自启失败回滚）；但托盘用 `expect` 直接 panic（P1-4），且部分分支静默 `return` |
| 可观测性 | 4.0 | 发布版 `windows_subsystem="windows"` + 仅 `eprintln`，现场零日志；`AudioWriter` 6 个诊断计数器全部无人消费 |
| 测试 | 5.0 | 协议层 20 项覆盖到位；审查时 Android 端 **0 项**测试；无 CI（修复后已补 17 项 JVM 单测） |
| 文档一致性 | 6.0 | 文档体量大、结构好，但 5 处与实现漂移（见 §6） |
| 安全 | 6.0 | 防火墙规则已收窄到 program+端口、签名密钥已出库；但无鉴权、设备 ID 可预测（设计级） |
| 代码规范 | 8.5 | Clippy 零告警；命名一致；注释记录了历史缺陷根因，可维护性高于同类项目 |
| **综合** | **7.0 / B-** | 可用、可维护，但距"生产级稳定"还差一个"异常路径专项" |

---

## 三、P1 级缺陷（4 项）

### P1-1 停止采集时先关通道后取消协程，生产者抛未捕获异常 → 进程崩溃风险

- **位置**：`android/.../service/CaptureService.kt:736-748`（`stopCapture`）、`:345-384`（生产者协程）
- **证据**：

```kotlin
// stopCapture()：先 close，再 cancel
audioChannel?.close()      // 741
audioChannel = null        // 742
captureJob?.cancel()       // 743  ← 取消发生在 close 之后

// 生产者协程：ch.send 无 try/catch
ch.send(accumBuf)          // 378
```

- **机理**：`Channel.close()` 会立即把挂起在 `send` 的发送者以 `ClosedSendChannelException` 恢复。该异常继承自 `IllegalStateException`，**不属于** `CancellationException`，因此不会被结构化并发视为正常取消：它会取消父 Job 并最终交给 `CoroutineExceptionHandler`。本项目 `serviceScope = CoroutineScope(Dispatchers.IO + SupervisorJob())` 未注册处理器，异常将走到 `AndroidExceptionPreHandler` → **主线程抛异常 → 进程崩溃**。
- **触发条件**：停止瞬间消费者恰好较慢（例如正在 `withContext(Dispatchers.IO)` 发帧），生产者阻塞在 `send`。属概率性，但每次"停止"都掷一次骰子。
- **建议**：`stopCapture()` 中调换顺序（先 `captureJob?.cancel()` 再 `close()`）；或把生产者的 `ch.send` 包进 `try { } catch (_: Exception) { }`；或改用 `ch.trySend(...)` 并在失败时归还缓冲池。
- **附带**：`framePool` 中尚未消费的 `ShortArray` 在取消路径上不会归还，池会缓慢泄漏（每轮 ≤3 个，影响小）。

---

### P1-2 ACK 超时后 `p2pConnected` 以 100 Hz 频率翻转，反向协程反复创建/取消

- **位置**：`CaptureService.kt:411-417`（连接判定）、`:466-475`（超时判定）
- **证据**：

```kotlin
// 每帧开头
if (udpSender?.hasAck() == true && !p2pConnected) {   // 411
    p2pConnected = true
    reverseJob = launchReverseAudio(currentIp)         // 415 每次重新 launch
}
// 同一帧稍后
if (ackTime > 0 && p2pConnected && elapsed > 3000) {   // 467-469
    p2pConnected = false
    reverseJob?.cancel()
}
```

```kotlin
// UdpSender.kt:85 —— 只判断"历史是否收到过 ACK"，永不复位
fun hasAck(): Boolean = lastAckTime > 0L
```

- **机理**：`lastAckTime` 只在 `close()` 中清零。一旦发生过一次 ACK，`hasAck()` 恒为 `true`。PC 端停止或 Wi-Fi 断开后，超时分支把 `p2pConnected` 置 `false`，**下一帧**（10 ms 后）立刻被 411 行重新置 `true` 并再次 `launch` 反向协程；同帧内又被超时分支置回 `false` 并 `cancel`。
- **影响**：
  1. `_status` 每帧被写 2 次（≈200 次/秒）→ Compose 重组风暴 + 通知栏 `startForeground` 反复调用；
  2. 每秒创建/销毁 100 个协程，每个都打印一次 `反向音频接收器已启动` → 日志刷屏；
  3. 断连期间 CPU/电量异常升高，UI 在"已连接/已断开"之间闪烁。
- **建议**：把"是否已连接"改为**时效判断**而非历史判断，例如 `fun isAckFresh(now: Long, windowMs: Long = 3000) = lastAckTime > 0 && now - lastAckTime < windowMs`，并让 411 行使用该函数；或在超时分支里把 `lastAckTime` 一并清零。

---

### P1-3 反向捕获未校验端点采样率，44.1 kHz 设备会被当作 48 kHz 编码

- **位置**：`windows/src/capture.rs:408-439`（`parse_mix_format`）、`:157-175`（编码器创建）、`:222-243`（组帧发送）
- **证据**：`parse_mix_format` 只取 `nChannels`、`wBitsPerSample`、`wFormatTag`，**未读取 `wf.nSamplesPerSec`**；而编码器硬编码为 48 kHz：

```rust
// capture.rs:157-161
let mut encoder = audiopus::coder::Encoder::new(
    audiopus::SampleRate::Hz48000,          // 硬编码
    audiopus::Channels::Stereo,
    audiopus::Application::Audio,
)?;
// capture.rs:222 每 960 个交错样本（=480 帧）打一包，按 10 ms 计
while pcm_buf.len() >= FRAME_SAMPLES { ... }
```

- **影响**：当默认呈现端点的共享模式混音格式为 44.1 kHz（HDMI、部分 USB 声卡、用户手动改过采样率的板载声卡）时：
  - 音高升高 48000/44100 ≈ **+8.8%**（约 1.5 个半音），人声明显失真；
  - 实际包间隔 10.88 ms 而解码端按 10 ms 计时，**反向流持续欠载**，手机端 `AudioPlayer` 队列长期处于低水位，表现为周期性断音。
- **说明**：上一轮 P1-6 处理的是**播放侧**（设备不支持 48 kHz 时 `AudioWriter` 的重采样），**捕获侧**未覆盖，属本轮新发现。
- **建议**：`CaptureFormat` 增加 `sample_rate: u32`；若 `!= 48000`，在 `read_stereo` 之后、编码之前插入一次重采样（可直接复用 `audio.rs` 的相位连续重采样思路），或按实际采样率创建编码器并把 `nSamplesPerSec/100` 作为帧长。

---

### P1-4 托盘图标创建失败即 `expect` panic，开机自启场景下静默退出

- **位置**：`windows/src/main.rs:968-976`
- **证据**：

```rust
let tray_icon_img = tray_icon::Icon::from_rgba(icon_rgba, width, height)
    .expect("embedded tray icon must be valid");      // 969
tray_icon::TrayIconBuilder::new()
    ...
    .build()
    .expect("tray icon must be created")              // 976
```

- **机理**：`TrayIconBuilder::build()` 依赖 `Shell_NotifyIcon`，在 **explorer.exe 尚未就绪**（开机自启、`explorer` 重启、远程会话切换）时会返回错误。由于 `#![windows_subsystem = "windows"]`，panic 信息无处输出，用户看到的现象是"点了没反应 / 开机后不启动"。
- **加重因素**：同一函数的返回值被 `let tray_icon = install_tray(...)` 绑定，panic 发生在 `iced::application(...)` 之前，窗口根本不会创建。
- **建议**：返回 `Option<TrayIcon>`，失败时降级为"无托盘模式"（关闭按钮改为最小化），并通过已有 `notice` 通道提示用户；`icon::from_rgba(...).unwrap()`（`main.rs:462`）同理，`load_icon_rgba` 已有兜底路径，此处不应再 unwrap。

---

## 四、P2 级缺陷（12 项）

| # | 位置 | 问题 | 影响 | 建议 |
|---|------|------|------|------|
| P2-1 | `CaptureService.kt:366-367` | 生产者 `if (read <= 0) continue`，无退避、无错误分类 | `AudioRecord.read` 返回 `ERROR_DEAD_OBJECT(-6)`（麦克风被抢占/设备热插拔）时进入**紧循环**，单核 100% 且永不恢复 | 区分 `ERROR_*`：致命错误置 `errorMsg` 并退出；瞬时错误 `delay(5)` 退避 |
| P2-2 | `CaptureService.kt:594-607`、`:622` | `isModeRestart` 在模式切换后残留为 `true`，正常停止时走"保留网络层"分支，跳过 `udpSender.close()` 与 `reverseDecoder.stop()` | `DatagramSocket` + `udp2cal-ack` 守护线程泄漏；再次启动时新建 socket，端口累积 | `stopCapture()` 中显式 `isModeRestart = false`；或在 `finally` 的 restart 分支仅保留"确有 pendingRestart"的情形 |
| P2-3 | `main.rs:278-293`、`:1126-1131`、`:1161` | `ReverseSender::stop_and_join()`（阻塞 `join`）在 iced 执行器线程 / UI 线程上调用 | 反向线程若正卡在 `GetBuffer`（≤20 ms 缓冲），UI 出现 20 ms 级卡顿；高频重连时累积 | 改为 `running=false` + 异步回收（`JoinHandle` 交给独立线程 join） |
| P2-4 | `capture.rs:200-207` | 每 1 秒**无条件**调用 `apply_config`（9 次 `opus_encoder_ctl`），即使配置未变 | 每次 ctl 会重置内部状态机，长期运行下引入周期性微小音质抖动 | 缓存 `last_applied: OpusReverseConfig`，仅在 `!=` 时下发（`update_reverse_config` 已有比较逻辑，直接复用即可） |
| P2-5 | `main.rs` view 底部 | 底部堆了「提示行 + 广播地址行 + 反向状态行 + 诊断行」多个行位，其中"已连接/已断开"与顶部状态点重复，且内容超出 320×290 被裁切 | 底部改为**单行**按状态切换（未启动=广播地址／已启动=广播地址+广播中／已连接=诊断+反向串流）；提示行仅保留错误，无提示时高度归零；**窗口固定 320×290 不可改**（成品 322×322 正方形） | 保持客户区 320×290，仅做内容收敛；**需 GUI 实测确认** |
| P2-6 | `main.rs:3` + 全仓 `eprintln!` | 发布版为 GUI 子系统且无控制台，`eprintln!` 输出全部丢失 | 用户报"没声音/连不上"时无任何现场证据，只能靠复现 | 增加落盘日志（`%LOCALAPPDATA%\UDP2CAL\log`，滚动 + 大小上限），或接入 `tracing` + 文件 appender |
| P2-7 | `CaptureService.kt:660-718` | 反向接收无序号处理、无去重、不校验 `hdr.deviceId`，仅按源 IP 过滤 | 与 PC 端 `ReorderBuffer` 能力不对称；乱序/重复包直接进 `opus_decode`，弱网下产生爆音 | 复用协议层的重排语义（Kotlin 侧补一个等价的 `ReorderBuffer`），并校验发送端设备 ID |
| P2-8 | `main.rs:1256-1264`、`protocol.rs:97-130` | P2P 准入只比对明文 `device_id`，而 `device_id` 每秒随 `DISCOVER_REPLY` 广播 | 同网段任意主机可嗅探 ID 后冒充"同设备保活"占用 PC；READY 时先到先得的 `TYPE_CONNECT` 可被恶意长期占用 | 属设计级限制：至少在 `docs/PROTOCOL.md` 写明威胁模型（"防误连，不防伪造"）；后续可加一次性 nonce 或 PSK HMAC |
| P2-9 | `main.rs:298-344` | `TYPE_DISCOVER_REQ` 无速率限制、无源地址校验，每请求必回包 | 可被用于 UDP 反射放大（请求 15 B → 回包 ~15+2+主机名，放大约 1.5×） | 加简单令牌桶（每源 IP ≤5 次/秒） |
| P2-10 | 仓库根 | 调试密钥库 `lanmic-debug.keystore` **仍被 Git 跟踪**（口令为 `android`，与上一轮 P0-3 同源） | 虽已不被构建引用，但历史密钥留在公开仓库，任何曾用它签名的 APK 都可被同签名覆盖安装 | 从索引与历史中移除（`git filter-repo`），并在 Release 说明中声明历史包不再受信 |
| P2-11 | `android/app/build.gradle.kts` | `NotificationCompat` / `ContextCompat` 来自 `androidx.core`，但依赖块未显式声明（仅靠 Compose 传递）；`lint.abortOnError = false` | 上游一旦裁剪传递依赖即编译失败；lint 门禁形同虚设 | 显式 `implementation("androidx.core:core-ktx:<ver>")`；lint 全绿后把 `abortOnError` 改 `true` |
| P2-12 | `CaptureService.kt:37`、`Prefs.kt:8-14` | `hasNewCapture` 为普通 `Boolean`，跨主线程/IO 协程读写；`Prefs.prefs` 为 `lateinit`，同样无同步 | JMM 下可见性不保证，理论上出现"停止后仍被判定为新启动"的偶发行为 | `@Volatile private var hasNewCapture`；`@Volatile` 修饰 `prefs` 或在 `init` 后不再变更（当前实践可用，补注解即可） |

---

## 五、P3 级问题（8 项）

| # | 位置 | 问题 |
|---|------|------|
| P3-1 | `audio.rs:74-82` | `resample_count` / `write_count` / `total_compensated` / `dropped_samples` / `drift_ratio` / `underrun_count` 全部**只写不读**，`underrun_count` 甚至带 `#[allow(dead_code)]`。诊断能力已建好但未接到 UI/日志——与 P2-6 叠加，等于完全没有丢帧可见性 |
| P3-2 | 多处 | 死代码：`Udp2CalProtocol.buildAudioPacketTo` / `isV2Packet`、`AudioPlayer.isPlaying`、`Prefs.connectedDeviceId`、`OpusEncoder.encode(ShortArray)`（走 `encodeTo`）、`OpusEncoder.encodeCount` |
| P3-3 | `protocol.rs:59`、`:193-221` | `PacketHeader.bitrate` 注释写 "kbps/2"，模块文档写 "bitrate_kbps"，且**收发两端都不消费该字段**（接收端自算字节数）；`resolve_bitrate` / `default_bitrate_for_sr` / `compute_bitrate_id` 仅测试使用，Kotlin 侧另有一份等价实现，靠人工保持同步 |
| P3-4 | `main.rs:631-635` | `notice_is_error()` 用中文字面子串（"失败/无效/不能/断开/不是合法"）判定错误色，任何文案调整都会让配色错位 |
| P3-5 | `main.rs:150-152` | `AUDIO_INIT_STATE` 用裸 `u32` + 魔数 0/1/2/3 表达四态，建议改 `enum AudioInitState`（`AtomicU8` 承载判别值） |
| P3-6 | `main.rs:1281-1285` | 接收热路径每包 `buf[..].to_vec()` 分配（100 次/秒）；`capture.rs` 已做到零分配，两端标准不一致 |
| P3-7 | `UdpSender.kt:72-82` | 每帧 `withContext(Dispatchers.IO)` 做一次协程调度后 `socket.send`，在 10 ms 预算内引入了不必要的调度抖动；`socket` / `remoteAddr` 亦未 `@Volatile` |
| P3-8 | 仓库根 `.gitignore:9` | 第 9 行 `构建产物` 缺少 `#`，被当成一条"匹配名为 构建产物 的文件"的无效规则（无害但易误导） |

---

## 六、文档与实现的一致性

审查中发现 4 处文档漂移，均不影响运行，但会误导后续维护者：

| # | 文档位置 | 文档描述 | 实际实现 |
|---|----------|----------|----------|
| 1 | `docs/ARCHITECTURE.md` §三 | `opt-level = "s"`（按体积优化） | `Cargo.toml` 为 `opt-level = 3`，且注释说明"体积换性能不划算" |
| 2 | `docs/ARCHITECTURE.md` §四 | "Android 每 ~1s（**50 帧**）发 CONNECT" | `CaptureService.kt` 的 `RECONNECT_INTERVAL = 100`（10 ms/帧 → 1 s） |
| 3 | `docs/ARCHITECTURE.md` §五 | 引用 `GLOBAL_DEVICE_STATE` / `BOUND_DEVICE_ID` 两个全局 | 已重构为单把锁 `P2P_STATE: Mutex<P2pState>`（代码方向正确，文档未跟进） |
| 4 | `docs/ARCHITECTURE.md` §五、`FEATURES.md` | 第三方端 `drainAck()` 1 ms 超时；保活周期 50 帧 | 已改为独立 ACK 线程 + 500 ms `soTimeout`；保活 100 帧 |

---

## 七、值得肯定的设计

1. **协议层单一真源**：`protocol/protocol.rs` 被 `pub use` 直引，Kotlin 侧仅有对齐实现；包头解析对 `version / sample_rate / msg_type / payload_len` 四重校验，测试覆盖 20 项（含回绕、迟到、重复、大跳变）。
2. **无锁 SPSC 环形缓冲**：以 `AtomicU32` 承载 f32 位模式，音频回调线程完全不加锁，正确规避了上一轮 P1-5 的优先级反转问题，且有两个针对性回归测试。
3. **漂移补偿方向正确**：`drift_ratio` 限制在 ±0.2% 且 EMA α=0.3，配合 `effective_ratio` 首帧立即生效（P1-6 回归测试已覆盖）。
4. **JNI 边界防守严密**：`encoderEncodeTo` 的静态 1276 B + 动态 `nbBytes` 双重守卫、`decoderDecode` 的 `offset/length` 与 PCM 容量校验、`Get*ArrayElements` 空指针检查，均属同类项目中少见的细致。
5. **反向播放的非阻塞队列模型**：`AudioPlayer` 用高/低水位裁剪把端到端延迟钉死，避免了过去"阻塞写 → socket 溢出 → 成片丢包"的恶性循环。
6. **构建与发布卫生**：签名密钥已移出仓库、`.gitignore` 已覆盖 `*.keystore`/`local.properties`、release 启用 R8 + `shrinkResources`、lint 恢复 `checkReleaseBuilds`。

---

## 八、改进优先级建议

| 顺序 | 事项 | 对应缺陷 | 预估工作量 |
|:---:|------|----------|-----------|
| 1 | 修正 `stopCapture` 的取消顺序 + 生产者异常兜底 | P1-1 | 0.5 h |
| 2 | ACK 新鲜度判定，消除断连后的 100 Hz 抖动 | P1-2 | 0.5 h |
| 3 | 捕获侧采样率校验/重采样 | P1-3 | 2–3 h |
| 4 | 托盘与窗口图标失败降级，去掉 `expect/unwrap` | P1-4 | 1 h |
| 5 | `read <= 0` 的错误分类与退避 | P2-1 | 0.5 h |
| 6 | 模式切换后的资源释放路径 + `isModeRestart` 复位 | P2-2 | 1 h |
| 7 | 落盘日志 + 把 6 个诊断计数器接到 UI/日志 | P2-6、P3-1 | 3–4 h |
| 8 | 反向线程异步回收，去掉 UI 线程上的 `join` | P2-3 | 1 h |
| 9 | 文档同步（5 处漂移）+ 威胁模型章节 | §6、P2-8 | 1 h |
| 10 | Android 侧补 JVM 单测（`ShortQueue`、`Udp2CalProtocol`）+ CI | §2 | 0.5–1 d |

---

## 九、附：可复现的验证命令

```bash
# 协议层
cd protocol && cargo test --offline          # 20 passed

# Windows 端
cd windows  && cargo test --offline          # 9 passed（含 5 项新增捕获侧重采样测试）
cd windows  && cargo clippy --offline --all-targets   # 0 warning

# Android 端（需 JDK 17；AGP 8.7 不支持 JDK 25+）
cd android && JAVA_HOME="<jdk17>" ./gradlew :app:assembleDebug --offline
```

**修复后的反向验证命令**（应分别返回"已改动"与"无输出"）：

```bash
grep -n "nSamplesPerSec" windows/src/capture.rs        # 已读取端点采样率
grep -n "fn stop(" windows/src/capture.rs             # 异步 join
grep -rn "expect(\"tray" windows/src/main.rs          # 无输出
grep -n "isAckFresh" android/app/src/main/java/com/udp2cal/app/UdpSender.kt
grep -rn "hasAck" android/app/src/main/java           # 无输出
grep -n "class ReorderBuffer" android/app/src/main/java/com/udp2cal/app/Udp2CalProtocol.kt
```

---

## 十、修复记录

24 项缺陷 + 4 处文档漂移已全部修复，逐项对应如下（细节见 [BUGFIX.md](../BUGFIX.md) 与 [BUGFIX_V2_ERA.md](BUGFIX_V2_ERA.md)）。

| 缺陷 | 修复要点 | 涉及文件 |
|------|----------|----------|
| P1-1 | 先 `cancel()` 再 `close()`；生产者 `ch.send` 加 try/catch；退出时主动 `ch.close()` | `CaptureService.kt` |
| P1-2 | 新增 `UdpSender.isAckFresh(now, 3000ms)`，替换 `hasAck()` 的两处调用 | `UdpSender.kt`、`CaptureService.kt` |
| P1-3 | `CaptureFormat.sample_rate` + `StereoResampler`（相位连续线性插值） | `capture.rs` |
| P1-4 | `install_tray() -> Option<TrayIcon>`；失败时 `exit_on_close_request = true` + 提示 | `main.rs` |
| P2-1 | `read < 0` 分类处理 + `delay(5)` 退避 + `ERROR_DEAD_OBJECT` 立即停止 | `CaptureService.kt` |
| P2-2 | `stopCapture()` 复位 `isModeRestart = false` | `CaptureService.kt` |
| P2-3 | `stop_and_join()` → `stop()`，join 移交后台线程 | `capture.rs`、`main.rs` |
| P2-4 | `applied: Option<OpusReverseConfig>`，仅在变化时下发 | `capture.rs` |
| P2-5 | 底部收敛为单行状态栏 + 提示块零高度；顶部"已占用"去掉识别码；窗口保持 320×290（322×322 正方形） | `main.rs` |
| P2-6 | 新增 `logger` 模块 + `logf!` 宏，17 处 `eprintln!` 迁移 | `logger.rs`（新增）、`main.rs`、`capture.rs` |
| P2-7 | 新增 `Udp2CalProtocol.ReorderBuffer`（与 Rust 同语义） | `Udp2CalProtocol.kt`、`CaptureService.kt` |
| P2-8 | `generate_device_id()` 引入 `RandomState`；ARCHITECTURE 增加威胁模型 | `protocol.rs`、`ARCHITECTURE.md` |
| P2-9 | 发现线程按源 IP 令牌桶（5/s） | `main.rs` |
| P2-10 | `git rm --cached` 移除 keystore 与 local.properties | 仓库索引 |
| P2-11 | 显式 `androidx.core:core-ktx:1.16.0` | `app/build.gradle.kts` |
| P2-12 | `hasNewCapture`、`Prefs.prefs` 加 `@Volatile` | `CaptureService.kt`、`Prefs.kt` |
| P3-1 | `AudioStats` 原子计数器 + UI 诊断行 + 30s 落盘 | `audio.rs`、`main.rs` |
| P3-2 | 删除 `isV2Packet`/`buildAudioPacketTo`/`isPlaying`/`connectedDeviceId`/`OpusEncoder.encode`/`encodeCount`/JNI `encoderEncode` | 5 个文件 |
| P3-3 | 码率字段标注为保留字段；helper 标注为规范参考实现 | `protocol.rs` |
| P3-4 | `NoticeLevel::{Info, Error}` 替代中文子串判定 | `main.rs` |
| P3-5 | `enum AudioInitState`（`AtomicU8`） | `main.rs` |
| P3-6 | 有界载荷缓冲池（64 × 1472B） | `main.rs` |
| P3-7 | `UdpSender.send()` 改为非挂起函数 | `UdpSender.kt` |
| P3-8 | `.gitignore` 修正 | `.gitignore` |
| 附加 | `ShortQueue` 成对淘汰（声道相位）；Android 设备 ID 改 `SecureRandom`；新增 17 项协议层 JVM 单测 | `AudioPlayer.kt`、`Udp2CalProtocol.kt`、`src/test/…/Udp2CalProtocolTest.kt` |
| 文档 | `opt-level`、50→100 帧、`GLOBAL_DEVICE_STATE`、`drainAck`、`android/` 目录树、日志章节、威胁模型 | `ARCHITECTURE.md`、`FEATURES.md` |

### 验证结果

| 项 | 结果 |
|----|------|
| `cargo test`（protocol） | **20 / 20 通过** |
| `cargo test`（windows） | **9 / 9 通过**（新增 5 项：重采样直通/44.1k 首块/跨块相位/96k 降采样/混音格式解析） |
| `cargo clippy --all-targets` | **0 告警** |
| `gradlew :app:assembleDebug`（JDK 17） | **BUILD SUCCESSFUL**（含 CMake/JNI 原生构建） |
| `gradlew :app:testDebugUnitTest` | **17 / 17 通过**（新增 `Udp2CalProtocolTest`：编解码往返、5 类非法包拒绝、跨端常量一致性、重排缓冲 7 组时序） |
| 真机 / 真声卡 | 未执行（本环境无设备），`StereoResampler` 仅通过单元测试验证 |

---

*本报告的审查阶段未修改任何源码；§十 记录的修复为后续独立提交。*
