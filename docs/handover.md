# UDP2Mic AI 继任者指南
> 写给下一个接手这个项目的 AI / 开发者
> 最后更新: 2026-06-03 | 状态: **正式版 v1.0.0 — 全部已修复，内存泄漏已解决**

---

## 一、你现在接手的是什么

一个**完全可用的**局域网麦克风系统，支持人声降噪、噪声门、自适应 AGC：

```
[Android 手机] ──{AGC+Wiener降噪+噪声门}──→ Opus UDP ──→ [Windows PC] ──WASAPI──→ [VB-Cable]
                                                                          ↓
                                                                 微信/Zoom/OBS/剪映/游戏
```

**双端均已编译通过，协议 100% 对齐。**

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

> 本地 SDK 路径: `C:\Android\sdk` (非默认的 `%LOCALAPPDATA%\Android\Sdk`)

---

## 三、架构与文件依赖图

### 协议层 (唯一真理源)

```
protocol/protocol.rs          ← 唯一协议源 (Rust crate: udp2mic-protocol)
    ├── windows/src/protocol.rs   → pub use udp2mic_protocol::* (重导出)
    └── android/.../Udp2MicProtocol.kt  → Kotlin 等价实现 (手动对齐)
```

**关键**: 修改协议必须**三处同步** (`protocol.rs`, `windows/src/protocol.rs` 自动跟随, `Udp2MicProtocol.kt` 需手动对齐)。

### Windows 端模块依赖

```
main.rs ────────────────────────────────────┐
  ├── config.rs (注册表读写, 自启动)         │
  ├── float.rs  (Win32 悬浮窗, 独立线程)     │
  ├── firewall.rs (netsh 静默)               │
  └── [订阅: udp_receiver_stream] ───────────┤
       ├── tokio::UdpSocket                  │
       ├── protocol::decode_header           │
       ├── protocol::ReorderBuffer           │
       ├── decoder::OpusDecoder              │
       └── audio::AudioWriter ───────────────┘
            └── RingBuffer (Mutex + VecDeque + Weak) → cpal → WASAPI → VB-Cable
```

> **注意**: `debug.rs` 和 `vbcable.rs` 已在 2026-06-03 清理中删除 (vbcable 内联到 audio::detect_vb_cable)。

### Android 端模块依赖

```
MainActivity.kt
  └── CaptureService.kt (前台服务)
        ├── AudioRecord (MIC, 48kHz, 单声道, PCM16)
        ├── [音频处理链]
        │     ├── VoiceEnhancer (Wiener 降噪, 去除背景杂音突出人声)
        │     ├── NoiseGate (自适应噪声门, 静音切除)
        │     └── AGC (自适应增益, 目标 -16.5dBFS)
        ├── NoiseSuppressor (Android 硬件语音降噪, API 26+)
        ├── OpusEncoder.kt
        │     ├── OpusNative.kt (JNI 声明)
        │     └── opus_jni.c → libopus 1.5.2
        ├── Udp2MicProtocol.kt (协议编解码)
        └── UdpSender.kt (java.net.DatagramSocket)
```

---

## 四、已解决 Bug 记录

### 🔴 [P0] ✅ 立体声交错写入Bug (曾被误判为时钟漂移)

**真根因 — 两个致命 Bug**:

**Bug A: 立体声交错写入错误**
旧代码将单声道样本 `[A,B,C,D]` 直接按顺序填入立体声 WASAPI 缓冲区:
```
data[0]=A, data[1]=B, data[2]=C, data[3]=D
→ 声卡解读为 [左:A, 右:B, 左:C, 右:D]
→ 左右耳不同 + 2x播放速度 + 升调八度
```

**Bug B: 缓冲区容量按单声道计算**
立体声设备每 10ms 消费 480帧×2ch=960float，但缓冲区只按 48000float/s 规划 → 实际 0.25s 耗尽。

**为什么被误判为"2%时钟漂移"？**
underrun 增长速度 ≈ 960samples/0.5s。这是立体声 2x 消费速度导致的，恰好被解释为"Android 48.00kHz vs Windows 48.96kHz 偏差 2%"，但**实为 100% 的立体声写入错误**。

**修复** (audio.rs 最终版):
1. 回调按帧处理: `need_frames = data.len() / channels`，每帧单声道样本复制到所有通道
2. 动态获取 `stream_config.channels`
3. 相位连续流式重采样: `while phase < n_in { phase += effective_ratio }` 跨帧连续
4. 漂移比率严格 clamp 在 0.998~1.002 (实际晶振偏差远小于 0.2%)
5. 200ms 初始缓冲 + 1s 上限

### 🔴 [P1] ✅ Android opus_jni.c 缺少 OPUS_SET_SIGNAL(MUSIC)

**现象**: 采集的环境音/音乐经 Opus 编码后低频损失明显。

**根因**: `opus_jni.c` 中 `encoderCreate` 只设置了 `OPUS_SET_APPLICATION(OPUS_APPLICATION_AUDIO)`，未设置 `OPUS_SET_SIGNAL(OPUS_SIGNAL_MUSIC)`。Opus 默认 `OPUS_SIGNAL_AUTO` 在检测到"非语音"时仍可能启用语音优化高通滤波 (>100Hz)。

**修复**: 已添加 `opus_encoder_ctl(enc, OPUS_SET_SIGNAL(OPUS_SIGNAL_MUSIC));`

### 🔴 [P4/P5/P6] ✅ 自适应 AGC + 人声降噪 + 噪声门

**最终处理链**: `原始 PCM → [Wiener 人声降噪] → [噪声门] → [AGC 增益] → Opus 编码`

| 模块 | 原理 | 参数 |
|------|------|------|
| AGC | EMA RMS 追踪 (α=0.18), 快攻/慢放 | 目标 -16.5dBFS, 增益 2x~40x |
| Wiener 降噪 | SNR²/(SNR²+1) 帧级增益, 自适应噪声底学习 | 学习 ~1.6s, 增益下限 0.2 |
| 噪声门 | 自适应底噪 + 3.5x 阈值 + 噪声底冻结 | 30帧保持 (~600ms) |
| 硬件降噪 | Android NoiseSuppressor API | 设备支持时优先启用 |

**UI 开关**: 噪声门 (蓝色) + 人声降噪 (紫色) + AGC 实时增益显示 (NS✓/VE✓ 标记)

---

## 五、最终状态 — 全部已解决

### ✅ 内存泄漏 (全部修复)

**已确认的 5 个泄漏点全部修复**:

| # | 问题 | 文件 | 修复 |
|---|------|------|------|
| Bug1 | iced Subscription 线程泄漏 — 无稳定标识导致每次重渲染重启流任务 | main.rs | `Subscription::run_with_id(UdpReceiverId, stream)` |
| Bug2 | udp_receiver_stream 循环中 payload Vec 作用域不明确 | main.rs | 块作用域 `{ let payload; ... }` 确保及时释放 |
| Bug3 | VecDeque drain 后 Capacity 不缩减 | audio.rs | drain 后若空则 `shrink_to_fit()` |
| Bug4 | 音频后台线程 Arc 强引用导致引用循环 — AudioWriter Drop 后计数 2→1，线程永不退出 | audio.rs | `Arc::downgrade(&buf)` + `upgrade()`，underrun 计数器同样 Weak |
| Bug5 | decoder.rs pcm_buf 容量只增不减 | decoder.rs | `shrink_pcm_buf()` 每 500ms 缩回 MAX_FRAME_SAMPLES |

### ✅ P2 — Windows 断连重连 (已修复)

`udp_receiver_stream` 添加了 `StreamGuard` Drop 守护 + `output.is_closed()` 检查，断连时 `break` 退出，iced 自动重建流，状态完整恢复。

### ✅ P3 — 悬浮窗 GDI (已处理)

悬浮窗隐藏时跳过全部 GDI 绘制 (`visible` 状态判断)，`PeekMessageW` 循环已正确处理非绘制消息。

### 🔵 P2/P3 — 剩余优化项 (低优先级，不影响正常使用)

| 项目 | 说明 | 难度 |
|------|------|------|
| P2: 断连后按住 Option 状态恢复 | UI 恢复时 `enable_float` 等可选按钮状态可能未重置 | 低 |
| P3: WM_PAINT 使用 BeginPaint/EndPaint | 当前 GetDC/ReleaseDC 工作正常，长时间运行未复现问题 | 低 |

---

## 六、已踩过的坑 (千万别重蹈覆辙)

### 🔴 编译选项

| 之前的错误配置 | 正确配置 | 原因 |
|--------------|---------|------|
| `opt-level = "z"` | `opt-level = "s"` | z 为体积而牺牲, 可能引入未定义行为 |
| `panic = "abort"` | `panic = "unwind"` | abort 导致资源泄漏(锁/句柄不释放) |
| UPX 压缩 | 不压缩 | UPX 压缩的 EXE 在某些杀软上被误报 |
| `strip = "symbols"` | — | 当前已启用, 发布版可接受 |

### 🔴 Protocol 重复

- **Bug**: 重构前 `protocol/protocol.rs` 和 `windows/src/protocol.rs` 各有一份协议代码, MAX_REORDER 不一致 (3 vs 8)
- **修复**: `protocol/` → 独立 crate `udp2mic-protocol`, Windows 端 `pub use` 重导出
- **教训**: 永远不要让协议代码有两份拷贝

### 🔴 Protocol 对齐

- **Bug**: Kotlin `payloadLen: Short` 有符号, `and 0xFFFF` 检查在 Kotlin 的 `Short` 上无效
- **修复**: `Short` → `Int`, 一致性解码

### 🔴 Opus 编码 (Android / opus_jni.c)

- `OPUS_SET_BITRATE` **期望 bps**, 不是 kbps! 32kbps = `32 * 1000`
- **VBR=1 会在低音量时产生 3 字节 DTX 静音包**, 环境声全丢
- 必须: `VBR=0, VBR_CONSTRAINT=1, DTX=0, COMPLEXITY=8`, `INBAND_FEC=0`
- `OPUS_APPLICATION_VOIP` 或 `OPUS_SIGNAL_VOICE` 会过滤非语音音频, **不要用**

### 🔴 Opus 解码 (Windows / audiopus 0.3.0-rc.0)

- API 与 0.2 完全不同: `Decoder::new(SampleRate::Hz8000, Channels::Mono)`
- `decode_float(Some(packet), signals, false)` — **第三个参数 fec=false**, 不是 true
- `fec=true` 是把数据当纠错包处理, 没有上一帧就输出全零 (曾因此浪费 2 小时)

### 🔴 JNI 全局状态 (Android / opus_jni.c)

- **Bug**: 原 `opus_jni.c` 用 static 全局变量 `g_encoder`, 只支持单实例
- **修复**: 改为 malloc `EncoderState` 结构体 → 返回 `(jlong)(intptr_t)state`
- Kotlin `external fun` 签名必须与 JNI 完全匹配, 否则 `UnsatisfiedLinkError`

### 🔴 cpal WASAPI 输出 (Windows)

- `cpal::Stream` **是 !Send 的**, 不能跨线程传递
- 必须在目标线程内 `build_output_stream`, 线程 `park()` 保持不退出
- **立体声陷阱**: WASAPI 立体声回调 `data` 格式为 `[L0,R0, L1,R1, ...]`。单声道必须复制到所有通道，不能直接逐采样填入

### 🔴 音频缓冲 (Windows / audio.rs) — 版本演进

- **v1**: RingBuffer + Mutex + Condvar → underrun
- **v2**: 无锁原子 RingBuffer → 100% underrun
- **v3**: mpsc::channel → chunk 大小不匹配
- **v4**: `Mutex<VecDeque<f32>>` + 相位连续重采样 + 立体声按帧处理 → 可用
- **v5 (最终)**: 同上 + `Arc::downgrade()` + `shrink_to()` → 防泄漏

### 🔴 解码器缓冲 (Windows / decoder.rs)

- **Bug**: `pcm_buf()` 返回整个 5760 元素的 vec, 调用方不知道有效数据长度
- **修复**: `last_n` 追踪最后一次解码样本数, `pcm_data()` 返回 `&self.pcm_buf[..self.last_n]`

### 🔴 Win32 悬浮窗 (Windows / float.rs)

- `ReleaseDC` 在 windows 0.58 中签名变更: `ReleaseDC(hwnd, dc)` 而非 `ReleaseDC(Some(hwnd), dc)`
- **GDI 泄漏**: 每次 `CreateSolidBrush` 必须配 `DeleteObject`, 否则 GDI 句柄耗尽
- 悬浮窗在 `main()` 开始就创建 (早于 iced), 线程内 `sleep(200ms)` 让 iced 先完成初始化

### 🔴 iced 0.13 UI (Windows)

- `Task::run()` 接收 Stream, 不是 Future
- `Subscription::run_with_id()` 创建唯一标识的流, `Subscription::batch()` 合并多个订阅
- `column!`/`row!` 中的 if-else 分支类型必须一致 (用 `.into()` 转 Element)
- `#![windows_subsystem = "windows"]` 隐藏控制台窗口

---

## 七、代码清理记录 (2026-06-03)

| 清理项 | 详情 |
|--------|------|
| **移除 `debug.rs`** | 整文件删除。无调试模式，不再产生 `udp2mic_debug.log` |
| **移除 `vbcable.rs`** | 内联到 `audio::detect_vb_cable().is_some()` |
| **移除 `config.debug_mode`** | 注册表字段和所有读取逻辑 |
| **移除 `ToggleDebug`** | Message、UI 按钮、调试面板全部删除 |
| **移除 `StreamGuard`** | 纯调试用结构体，eprintln 无用 |
| **简化 decoder.rs** | 移除 `total_frames`/`total_samples`/`expected_samples_per_frame` 死字段 |
| **简化 Android Prefs** | 移除 `debugMode`/`sampleRate`/`bitrate` 死字段 |
| **简化 Android OpusEncoder** | 移除 `debugMode`/`totalRawBytes` 死字段 |
| **简化 Android OpusNative** | 移除 `encoderGetDebugInfo()` 死函数 |
| **简化 Android Protocol** | 移除 `MAX_REORDER`/`defaultBitrateForSr()` 死代码 |
| **移除 Android 死依赖** | `ktor-network`/`lifecycle-service` 从 build.gradle.kts 删除 |

---

## 八、常见问题排查

### Windows 端无法启动

1. `程序已在运行中` → 检查任务管理器, 杀掉之前的进程
2. `绑定失败: 端口被占用` → 8899 端口被其他程序占用
3. `音频: 无可用音频输出设备` → 安装 VB-Audio Virtual Cable

### Android 端无法连接

1. `麦克风权限未授予` → 设置 → 应用 → UDP2Mic → 权限
2. `网络连接失败` → 确保手机和 PC 在同一局域网
3. 没有声音 → 检查 Windows 防火墙是否放行 UDP 8899

### 声音问题

1. 断续/爆音 → 检查 WiFi 信号强度, 5GHz 优于 2.4GHz
2. 没有环境声 → 确认编码器 `VBR=0` `DTX=0`
3. 延迟过大 → 20ms 帧 + UDP 局域网通常 <50ms

---

## 九、如果还需要改什么

| 需求 | 涉及文件 | 风险 |
|------|---------|------|
| 改端口 | config.rs + firewall.rs (Windows) / Prefs.kt (Android) | 低 |
| 改采样率 | main.rs + decoder.rs + audio.rs (Windows) / CaptureService.kt + opus_jni.c (Android) | 中 |
| 改码率 | CaptureService.kt / MainActivity.kt (仅 Android 端) | 低 |
| 改协议 | protocol/protocol.rs + Udp2MicProtocol.kt (**必须双端同步**) | 高 |
| 加功能 (Windows UI) | main.rs (iced 0.13) | 中 |
| 加功能 (Android UI) | MainActivity.kt (Compose) | 中 |
| 升级 iced | Cargo.toml + main.rs (API 可能破坏性变更) | 高 |
| 升级 audiopus | Cargo.toml + decoder.rs (API 曾从 0.2 大改到 0.3) | 高 |

---

## 十、环境速查

| 工具 | 版本 | 路径/备注 |
|------|------|----------|
| Rust | 1.96.0 stable | `rustup` 管理 |
| CMake | 4.3.3 | 需设 `CMAKE_POLICY_VERSION_MINIMUM=3.5` |
| JDK | 17.0.19 Eclipse Temurin | `JAVA_HOME` = `C:\Program Files\Eclipse Adoptium\jdk-17.0.19.10-hotspot` |
| Android SDK | 35 | `ANDROID_HOME` = `C:\Android\sdk` |
| NDK | 27.0.12077973 | SDK Manager 安装 |
| Gradle | 8.9 | wrapper 自带 |
| iced | 0.13.1 | tiny-skia 后端 |
| audiopus | 0.3.0-rc.0 | decoder feature only |
| cpal | 0.15.3 | WASAPI host |
| windows crate | 0.58 | Win32 API 绑定 |
| libopus | 1.5.2 | 本地源码 `android/app/src/main/cpp/opus-src/` |
| VB-Cable | — | https://vb-audio.com/Cable/ |