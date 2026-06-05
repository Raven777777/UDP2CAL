# UDP2Mic AI 继任者指南
> 写给下一个接手这个项目的 AI / 开发者
> 最后更新: 2026-06-05 | 状态: **v1.0.6 — 智能 AGC 底噪安全区 + dBFS 噪声门 + 硬件降噪联动 + Windows 端 UI 现代化 + 全局单例音频守护线程**

## 一、你现在接手的是什么

一个**工业级稳定、低延迟且高度解耦的**局域网麦克风系统，支持**智能 AGC（底噪安全区锁定 + 目标 -18dBFS）**与**自适应/手动 dBFS 噪声门**。Windows 端采用**全局单例音频守护线程** + Channel 无锁通信，UI 启停不影响底层音频引擎：

```
[Android 手机] ──{生产→消费双协程}──→ 智能AGC(底噪追踪+安全区10dB) → 噪声门(-40dBFS自动) → Opus完全零分配编码 → UDP零分配发送 → [Windows PC]
    ↓
Windows：UDP 接收协程 → SyncChannel → 常驻 Audio Worker 线程（解码 + WASAPI 播放 → VB-Cable/扬声器）
    ↓
微信/Zoom/OBS/游戏
```

**双端均已编译通过。无任何残留的神经网络降噪。Android 端已集成智能 AGC、dBFS 噪声门、Android 硬件级 NoiseSuppressor 联动。**

---

## 二、编译命令

### Windows
```powershell
build_windows.bat
# 产物: udp2mic.exe (根目录)

```

### Android
```batch
build_android.bat
# 产物: udp2mic-release.apk (根目录，已签名)

```

**编译前提**: Rust 1.96+, VS Build Tools 2022 (C++), CMake 3.22+, JDK 17, Android SDK 35, NDK 27.0.12077973

> 本地 SDK 路径: `C:\Android\sdk`

---

## 三、架构精要与并发避坑指南（核心技术资产）

### 1. 生产-消费双协程音频流水线（v1.0.5 新架构）
过去 `AudioRecord.read()` 与 AGC/编码/发送串行在同一协程中，硬件阻塞读会导致后续处理错过最佳时机，造成缓冲区溢出（爆音）。

* **生产者协程**（`Dispatchers.IO`）：独占线程只做 `AudioRecord.read()` + 帧组装，通过 `Channel<ShortArray>(3)` 投递给消费者
* **消费者协程**：从 Channel 取帧，串行执行 AGC → 编码参数更新 → 噪声门 → 编码 → UDP 发送
* **Channel 容量 3**：提供 60ms 缓冲并行度，同时天然提供背压防止无限堆积

### 2. Pipeline 完全零分配（ByteArray + ShortArray 双闭环）（v1.0.5 终极优化）
**核心思路**：整个 Pipeline 中没有任何 `ByteArray` 或 `ShortArray` 的 `new`/`copyOf`（池热身期除外）。

#### a) JNI 层 `encoderEncodeTo` — 写入预分配缓冲区
```c
// opus_jni.c — 不走 NewByteArray，直接 memcpy 到 dest
jbyte* destBytes = (*env)->GetByteArrayElements(env, dest, NULL);
memcpy(destBytes + offset, packet, nbBytes);
(*env)->ReleaseByteArrayElements(env, dest, destBytes, 0);
return nbBytes;
```

#### b) `Udp2MicProtocol.writeHeader` — 原地写入包头
```kotlin
// 负载已在 dest 中，仅写入 6 字节包头
fun writeHeader(dest, headerOffset, payloadLen, sampleRate, seqNum, bitrate): Int
```

#### c) `UdpSender.send(data, offset, length)` — 零拷贝发送
```kotlin
sock.send(DatagramPacket(data, offset, length, addr, port))  // 无需 copyOf
```

#### d) `ShortArrayPool` — 帧复用池（线程安全）
```kotlin
private class ShortArrayPool(val frameSize: Int, val capacity: Int = 3) {
    private val pool = arrayOfNulls<ShortArray>(capacity)
    private var head = 0
    private var count = 0

    @Synchronized fun borrow(): ShortArray  // 生产者：从池中借出
    @Synchronized fun recycle(buf: ShortArray)  // 消费者：处理完归还
}
```
生产者不再 `pcmAccum.copyOf()`，而是从池中 `borrow()` 一块缓冲区直接发送。消费者处理完后 `recycle()` 回池。**仅池空时（热身期前3帧）触发 ShortArray 构造**，之后永久零分配。

#### e) 乒乓发送缓冲区 — 防脏数据
```kotlin
val sendBuffers = arrayOf(ByteArray(MAX_PACKET), ByteArray(MAX_PACKET))
var bufIndex = 0
// 每帧轮换：当前帧用 bufIndex，下一帧自动切换
val buf = sendBuffers[bufIndex]; bufIndex = (bufIndex + 1) % 2
encoder?.encodeTo(frame, buf, 0)
udpSender?.send(buf, 0, written)
```
两块缓冲区交替使用，发送中的缓冲区不会被下一帧擦写。

#### f) 完整 Pipeline 分配图
```
AudioRecord.read → pcmBuffer (栈复用)
    ↓
accumBuf = framePool.borrow()  ← @Synchronized, 池空时才 new
    ↓
ch.send(accumBuf)  →  消费者 → AGC + 噪声门 → encodeTo(writeHeader + JNI memcpy)
    ↑                                    ↓
 consume 后 recycle(accumBuf)       sendBuffers[bufIndex]
    ↑                                    ↓
 归还池中                                UdpSender.send(buf, 0, written)
                                         (DatagramPacket offset+length, 零copy)
```

### 3. JNI 双重边界守卫 — 防缓冲区溢出（v1.0.5）
`opus_jni.c` 的 `encoderEncodeTo` 设有两道防线：

```c
// 第1道：静态阈值守卫（编码前）
// Opus 单帧 20ms 最恶劣情况下输出不超过 1276 字节
jsize destLen = (*env)->GetArrayLength(env, dest);
if (offset < 0 || offset >= destLen || (destLen - offset) < 1276) { return -1; }

// ... opus_encode ...

// 第2道：动态精确守卫（编码后，memcpy 前）
if (destLen - offset < (jsize)nbBytes) { return -1; }
```
双重保护确保极端 VBR 膨胀、码率突增场景下也绝不写穿 Java ByteArray 边界导致 SIGSEGV。

### 4. Channel 生命周期联动关闭（v1.0.5）
```kotlin
// 字段持有引用
private var audioChannel: Channel<ShortArray>? = null

fun stopCapture() {
    audioChannel?.close()   // 先关闭 Channel，让消费者 for 循环退出
    audioChannel = null
    captureJob?.cancel()     // 再取消协程
    captureJob = null
}

// doStartCapture finally 中也执行 ch.close() + audioChannel = null
```
双重保障：无论正常停止还是协程异常退出，Channel 都能及时关闭。

### 5. JNI 与音频流的高并发互斥防线（防闪退）
* **`@Synchronized`**：`OpusEncoder` 的 `encode`/`encodeTo`/`update`/`start`/`stop` 全部加锁，杜绝 JNI handle 多线程竞争。
* **`uintptr_t`**：C 层使用 `(EncoderState*)(uintptr_t)handle` 而非 `intptr_t`，防止 32 位设备符号位扩展。

### 6. AGC 样点级线性插值平滑（v1.0.5 修复）
* 新增 `agcPreviousGain` 状态字段
* 每帧内从 `gStart` 到 `gEnd` 逐样点线性插值，消除帧边界不连续

### 7. 免重启音频管线与"无缝热重连"
* **网络热重连**：消费者每帧检测 `Prefs` 变更，原地静默重建 `UdpSender`，麦克风/编码器常驻
* **参数热更新**：Opus 参数指纹 Hash 挡板，边缘触发 JNI `encoderUpdate`

### 8. 局域网广播自动发现（v1.0.5 新功能）
* **Windows 端**：`start_broadcast_listener()` 常驻 44043 端口
* **Android 端**：`DiscoveryManager.discoverServer()` + IP 输入框右侧 `autorenew` 图标

### 9. 智能 AGC / Opus DTX-VBR / Compose 优化
* AGC 边缘触发重置 10.0x，自动上限 100x，手动 0~200x
* DTX 开启时 VBR 强制锁定，前端 UI 级联禁用
* Compose `LaunchedEffect` 分流高频状态，防止全局 Recomposition

### 10. Prefs 初始化安全
* `Prefs.init(applicationContext)` 消除 Activity 隐式泄漏

---

## 四、Windows 接收端重点优化记录（v1.0.6）

以下优化专为 Windows 端 Rust 代码在**极端边缘情况下的容错性、UI 异步处理行为、资源释放顺序**所做的强化。

### 1. `udp_receiver_stream` 竞态条件消除（`main.rs`）

**问题**：原代码在 `udp_receiver_stream` 异步流内部调用 `config::Config::load()` 重新读取注册表配置，而主线程在 `ToggleRunning` 时同步调用 `self.config.save()` 写入注册表。多线程读写注册表可能导致脏数据。

**修复**：`udp_receiver_stream` 改为接收 `listen_ip: String` 和 `listen_port: u32` 参数，由 `subscription()` 通过 `self.config` 传值，消除跨线程注册表竞态。

```rust
// 之前：内部重新 load()
fn udp_receiver_stream() -> impl Stream<Item = Message> {
    let cfg = config::Config::load(); // 多线程竞态风险
    ...
}

// 之后：从 subscription() 传参
fn subscription(&self) -> Subscription<Message> {
    let ip = self.config.listen_ip.clone();
    let port = self.config.listen_port;
    Subscription::run_with_id(UdpReceiverId, udp_receiver_stream(ip, port))
}
fn udp_receiver_stream(listen_ip: String, listen_port: u32) -> impl Stream<Item = Message> { ... }
```

### 2. EMA 漂移比率 NaN/Infinity 防线（`audio.rs`）

**问题**：极端网络抖动（如长时间断流后瞬间涌入大量包）可能导致 `produce_rate` 为 0 或异常值，使 `measured = device_rate / produce_rate` 变为 `Infinity` 或 `NaN`，污染 EMA 滤波器后使 `drift_ratio` 永久锁定为 `NaN`，导致音频静音。

**修复**：在 EMA 更新前增加三层安全校验：
```rust
if produce_rate > 1000.0 && produce_rate.is_normal() {
    let measured = self.device_rate as f64 / produce_rate;
    if measured.is_finite() && measured > 0.5 && measured < 1.5 {
        self.drift_ratio = self.drift_ratio * 0.7 + measured * 0.3;
    }
}
```
* `is_normal()`：排除零、次正则、Infinity、NaN
* `is_finite()`：二次确认
* `0.5 ~ 1.5` 范围限制：防止极端值大幅污染 EMA

### 3. HINSTANCE 空指针修复（`float.rs`）

**问题**：悬浮窗线程使用 `HINSTANCE(std::ptr::null_mut())` 创建窗口，不符合 Win32 最佳实践，在严格安全策略环境下可能导致窗口创建失败。

**修复**：使用 `GetModuleHandleW(None)` 获取真实进程模块句柄：
```rust
let inst = GetModuleHandleW(None).unwrap_or_default();
```

### 4. 快速双击防抖 + 端口校验（`main.rs`）

**问题**：快速双击"启动/停止"按钮可能导致旧 socket 未完全释放、新 socket 绑定失败，用户卡在"已启动但无法接收数据"的尴尬状态。

**修复**：
* **200ms 防抖**：`ToggleRunning` 入口检查 `last_toggle_instant.elapsed() < 200ms` 时直接跳过
* **实时端口校验**：`PortChanged` 时实时校验 `parse::<u16>()`，非法输入时输入框边框变红
* **启动时严格校验**：端口无效时拒绝启动并显示错误提示文字

### 5. 代码可读性提升（`config.rs` + `main.rs`）

* 新增 `Config::is_auto_start() -> bool` 便捷方法
* 所有 `!(self.config.auto_start != 0)` 替换为 `!self.config.is_auto_start()`

### 6. UI 现代化改造：卡片布局 + 动态 VU 表 + 交互反馈（`main.rs`）

**问题**：原 UI 为单一纵向平铺，按钮为纯静态色块，音频电平条为单色，状态提示文本（"绑定失败"、"等待连接"）被遗忘在界面底部难以察觉。

**修复**：

- **模块化卡片布局**：放弃单一纵向平铺，改用深色渐进微卡片包装"网络配置"与"数据监测"区域，界面层级更加精致清晰
- **动态 VU 色彩表**：为音频电平进度条注入动态色彩策略：
  - `-60dB ~ -25dB`：**极客绿**（安全区）
  - `-25dB ~ -10dB`：**警告橙**（过渡区）
  - `-10dB ~ 0dB`：**电平过载红**（削波区）
  - 逼真还原专业混音台的视觉回馈
- **按钮 Hover/Pressed 反馈**：加入微调色彩算法，按钮在鼠标悬停及点击时拥有灵敏的平滑明暗反馈
- **统一输入框样式**：美化了输入框底色与圆角边框，使其在暗黑主题下更加深邃融合，保留端口报错时的红色高亮
- **状态文本精细化**：将 `status_text` 放置在页脚，让错误提示能真正直观、优雅地显现

### 7. 全局单例音频守护线程 — 彻底根除反复启停内存泄漏（`main.rs`）

**问题**：在 Iced UI 中反复点击"启动/停止"时，底层的 `udp_receiver_stream` 会被不断地销毁和重建。旧版每次启动调用 `audio::start_audio()`（封装 cpal/wasapi），销毁旧流时音频底层异步线程无法被优雅关停（多数音频库的默认封装都有此通病），导致每点一次"启动"就凭空多出一个新的驻留线程和音频缓冲区，造成内存和线程数疯狂上涨，最终导致系统卡死。

**修复**：采用专业音频软件架构——**后台全局单例音频守护线程 (Audio Worker Thread)**：

```rust
enum AudioMessage {
    Packet { seq_num: u8, sample_rate: u8, payload: Vec<u8> },
    Reset,
}

static AUDIO_TX: OnceLock<SyncSender<AudioMessage>> = OnceLock::new();
static AUDIO_LEVEL_DB: AtomicU32 = AtomicU32::new((-60.0f32).to_bits());

fn init_audio_worker() {
    // 仅在程序启动时调用一次
    let (tx, rx) = sync_channel::<AudioMessage>(200);
    AUDIO_TX.set(tx).ok();
    std::thread::spawn(move || {
        let dec = ...;  // 解码器只初始化一次
        let aw = ...;   // AudioWriter 只初始化一次
        for msg in rx { /* 常驻循环，通过 Channel 接收数据 */ }
    });
}
```

**核心原理**：

- **音频引擎（audio）和解码器（decoder）在整个软件生命周期内只初始化一次**，无论 UI 如何启停，底层音频线程不受影响
- **Iced UI 和网络流只负责收发数据**，通过 `SyncChannel<AudioMessage>` 把数据喂给常驻后台线程
- `Reset` 消息替代旧版的完全销毁重建：收到后仅清空 `ReorderBuffer` 和 RMS 统计，不解构任何音频资源
- 同时为 Iced 的 `run_with_id` 引入 `session_id` 计数器，强迫每次启动当作全新订阅处理，避免 Iced 内部状态残留

**效果**：无论用户点击多少次启动/停止，内存和线程数始终稳定，内存泄漏被彻底根除。

---

## 五、Android 发送端重点优化记录（v1.0.6）

以下优化专为 Android 端 Kotlin 代码在**智能 AGC 底噪安全区锁定、dBFS 噪声门、硬件降噪联动、并发安全**方面所做的强化。

### 1. 智能 AGC：底噪安全区锁定 + 目标 -18dBFS（`CaptureService.kt`）

**问题**：旧版 AGC 持续追踪 `agcSmoothedRms`，在静音期会盲目放大环境底噪（Noise Pumping），产生"空气抽吸感"。

**修复**：全新智能 AGC 架构：

```kotlin
// 极慢底噪追踪（alphaTrackNoise = 0.002）
if (currentDb < agcNoiseFloorDb + 3.0 || currentDb < -45.0) {
    agcNoiseFloorDb = agcNoiseFloorDb * (1.0 - alphaTrackNoise) + currentDb * alphaTrackNoise
}
// 安全区锁定：仅当声音高出底噪 10dB 才视为"真人说话"
val isRealVoice = currentDb > (agcNoiseFloorDb + AGC_SAFE_ZONE_DB)
if (isRealVoice) {
    val dbDeficit = -18.0 - currentDb  // 目标人声 -18dBFS
    val idealGain = 10^(dbDeficit / 20) // 计算理想放大倍数
    val targetGain = idealGain.coerceAtMost(userMaxGainLimit)
    agcCurrentGain = agcCurrentGain * 0.8f + targetGain * 0.2f // 快进慢出
} else {
    // 底噪区 → 增益沉降回 1.0f，绝不放大环境噪声
    agcCurrentGain = agcCurrentGain * 0.7f + 1.0f * 0.3f
}
```

**核心原理**：
- `agcNoiseFloorDb` 极慢追踪麦克风底噪（-50dB → -45dB → ...）
- `AGC_SAFE_ZONE_DB = 10.0`：能量高出底噪 10dB 才判定为"真人说话"，杜绝底噪被误放大
- 目标人声 `-18dBFS`：国际广播标准电平，远近说话音量被归一化到统一响度
- 样点级线性插值保持，从 `agcPreviousGain` 渐变到 `agcCurrentGain`，消除咔哒爆音

### 2. dBFS 噪声门阈值（`CaptureService.kt` + `Prefs.kt` + `MainActivity.kt`）

**问题**：旧 RMS 绝对值阈值（0~300）在嘈杂环境不够用，且不直观。

**修复**：改用工业标准 dBFS（满刻度分贝）作为阈值：
- UI 滑块范围：`-60 dBFS`（几乎不切）~ `0 dBFS`（封死所有声音）
- 默认值：`-40 dBFS`（典型底噪水平）
- 自动模式：内部 `ambientEnergy` 追踪转 dBFS 后对比
- 手动模式：滑块值直接作为 dBFS 切除阈值

### 3. 关门 10% 环境音保留 + 延迟静音架构（`CaptureService.kt`）

**问题**：`frame.fill(0)` 彻底静音导致听觉断层，且过早 fill 会影响 AGC 底噪追踪。

**修复**：
- **延迟静音**：引入 `shouldMuteFrame` 标记，噪声门只标记不破坏缓冲区，在编码前最后执行衰减
- **10% 保留**：关门时不彻底静音，保留 10% 环境音掩蔽听觉断层：
```kotlin
if (shouldMuteFrame) {
    for (i in frame.indices) {
        frame[i] = (frame[i] * 0.1f).toInt().coerceIn(-32768, 32767).toShort()
    }
}
```

### 4. Android 硬件级 NoiseSuppressor 联动（`CaptureService.kt`）

**新增**：`android.media.audiofx.NoiseSuppressor` 实例化管理：
- `updateHardwareNoiseCancellation()` 方法动态启停硬件降噪
- 录音初始化时根据 `Prefs.noiseGate` 启动
- 生产者循环每帧检测 Prefs 变化，热生效（无需重启 Service）
- `stopCapture()` 时确保释放硬件资源

### 5. 并发安全熔断与池容量调优（`CaptureService.kt`）

| 修复 | 问题 | 方案 |
| --- | --- | --- |
| 对象池耗尽 | 池容量 3 ≤ 通道容量 3，生产者可能池空卡死 | 池容量 3 → **5** |
| `ngActive` 不同步 | 仅在初始化赋值，UI 无法感知中途开关 | 每秒汇报 `ngActive = Prefs.noiseGate` |
| 硬件故障无限重启 | 麦克风被占用时 `pendingRestart` 死循环 | 熔断：`errorMsg` 含"麦克风初始化失败"时清空 `pendingRestart` |
| AGC-噪声门死锁 | 噪声门 fill(0) 导致 AGC 增益暴冲 100 倍 | 延迟静音架构解耦；AGC 使用原始信号 dBFS（不受噪声门影响） |
| 快照误触发 | 说话声触发 `currentRms > ambientEnergy * 4.0` | 连续 6 帧计数器验证环境突变 |
| 安全区不热生效 | `AGC_SAFE_ZONE_DB` 定义在 for 循环外，切换 AGC 模式不更新 | 移入循环内每帧读取 `Prefs.agcSafeZone` |
| 死代码残留 | `Prefs.noiseGateAuto` 无引用；`windows/src/vbcable.rs` 未导入 | 已删除冗余代码 |

---

## 六、历史重大变更记录 (Changelog)

| 版本 | 变更点 | 核心目的 / 解决的痛点 |
| --- | --- | --- |
| **v1.0.6** | **智能 AGC + dBFS 噪声门 + Windows 端 UI 现代化 + 全局单例音频守护线程** | Android：智能 AGC 底噪安全区锁定（10dB）+ 目标 -18dBFS 杜绝底噪放大；dBFS 阈值滑块（-60~0dB）取代 RMS；关门保留 10% 环境音；NoiseSuppressor 硬件降噪联动；对象池 3→5、熔断保护、延迟静音解耦；安全区每帧热生效修复；清除死代码。Windows：UI 卡面布局 + 动态 VU 色彩表 + 按钮 Hover/Pressed 反馈 + 统一输入框样式 + 状态文本页脚化；全局单例音频守护线程（Audio Worker）彻底根除反复启停内存泄漏；`run_with_id(session_id)` 状态隔离 |
| **v1.0.5** | **全链路零分配 / 双协程 / 双重边界守卫 / ShortArrayPool / 广播发现 / AGC插值** | 双协程消除 AudioRecord 阻塞饥饿；JNI `encoderEncodeTo` + `writeHeader` + `UdpSender::send(offset)` + `ShortArrayPool@Synchronized` 实现 ByteArray+ShortArray 全零分配，根除一切 GC 抖动；JNI 双重边界 1276+动态 守卫防越界写穿；Channel 生命周期联动关闭；UDP 广播自动搜索 PC；样点级 AGC 插值消除爆音；uintptr_t + applicationContext 消除底层隐患 |
| **v1.0.4** | **JNI 同步锁 / 网络无缝热重连** | 引入 `@Synchronized` 根除多线程并发闪退；主页改 IP 不重启录音流 |
| **v1.0.3** | **AGC 解耦 / Opus 免重启参数热更新** | 剥离 restart() 逻辑，参数指纹挡板 |
| **v1.0.2** | **移除了 RNN/TFLite 降噪** | 回归轻量化经典管线 |
| **v1.0.1** | **重构为 Kotlin 协程常驻服务** | 废除频繁线程销毁重建 |

---

## 七、接手后推荐的后续演进方向

1. **JNI 层双向锁（可选）**：在 `EncoderState` 中加入 `pthread_mutex_t` 锁，实现 Kotlin + C 双层保险。
2. **MDNS 发现替代 UDP 广播**：支持跨子网、多网卡环境下的自动发现。
3. **Compose 高频状态子组件拆分**：如需 100ms 级音量条刷新，将高频状态拆为独立子组件。
4. **Windows 端也可考虑广播自动搜索**：让 PC 也能主动发现 Android 设备（反向搜索）。
5. **Windows 端持续改进**：可考虑统一错误上报机制（如 toast 通知代替 UI 内状态文字）、支持多声卡切换 UI、以及音频链路的延迟统计仪表盘。
