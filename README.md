# UDP2CAL — 局域网音频串流

> **当前版本: v1.1.0-hotfix3** — 三端代码审查 + 立体声解码修复 + 多 Bug 修复
> * 双向音频：手机麦克风→PC (VB-Cable) + PC 扬声器→手机听筒/扬声器
> * 反向串流：WASAPI Loopback → Opus 编码 → UDP 发送（Windows UI 按钮控制启停）
> * 声学回声消除：语音模式启用 AcousticEchoCanceler + MODE_IN_COMMUNICATION + 同 session 绑定
> * 立体声/单声道自适应：现代手机立体声编码，低端设备单声道降级（CONNECT payload 模式标志）
> * **android_old 发热优化**：48kHz FB + 40ms 帧合并 + JNI 动态帧长，CPU 120%→14%（实测 Sharp NP805SH / 骁龙210 / 1GB RAM）
> * **android_old 反向解码立体声修复**：opus_jni_decoder.c 单声道解码器→立体声，修复 PC 高品质模式反向无声
> * v2 控制协议: 二进制发现、独占连接(TYPE_CONNECT/ACK)、8字节设备ID鉴权
> * 双状态机: 广播状态机(Ready/Silent) + 连接状态机(Idle/Busy)，全局原子状态同步
> * 心跳熔断: 3秒 ACK 超时标记断连，1秒无包自动释放独占，防碰撞防抢占
> * 防抢占静默拒绝: BUSY 时异设备 TYPE_CONNECT 不回复 ACK，新设备永无法发送音频，已有连接完全不受影响
> * 保活 CONNECT 携带反向端口：Win 重启后自动恢复反向串流
> * 全链路生命周期: ACK 超时断连 / 无包超时释放 / 并发锁绑定，无需重启

## 概述

```
Android 手机 ↔ 双向音频 ↔ Windows PC
  正向: 手机麦克风 → Opus 编码 → UDP → PC → WASAPI → VB-Cable → 任意应用
  反向: PC 扬声器 → WASAPI Loopback → Opus 编码 → UDP → 手机听筒/扬声器
```

UDP2CAL 是一个**双向、低延迟**的局域网音频串流系统。提供两个 Android 客户端版本：**`android/`（现代版）** 面向 Jetpack Compose 设备，固定 48kHz 采集，支持全参数 Opus 调节 + 反向编码实时同步；**`android_old/`（低性能版）** 面向 Android 6~8 低端/翻盖机，48kHz 全频带 + 40ms 帧合并编码 + JNI 动态帧长 + CPU 自动降级保护，CPU ~14%（实测 Sharp NP805SH / 骁龙210 / 1GB RAM）。

**全链路零堆分配**（ByteArray + ShortArray 双闭环，仅池热身期 3 次构造）、**生产-消费双协程**消除阻塞饥饿、**JNI 双重边界守卫**防越界写穿。

---

## 快速开始

### Windows（项目根目录运行）

```
build_windows.bat
# 交互式选择:
#   [1] x86_64 (64-bit)  → udp2cal_x64.exe
#   [2] i686   (32-bit)  → udp2cal_x86.exe
# 产物: udp2cal_{arch}.exe (项目根目录)
```

**先决条件**: Rust 1.96+、VS Build Tools 2022 (C++桌面开发)、CMake 3.22+

> 若使用 CMake ≥ 4.0，编译前需设置 `$env:CMAKE_POLICY_VERSION_MINIMUM="3.5"`。
>
> 若系统中安装了 `upx.exe`，编译后会询问是否压缩 EXE；压缩后的产物后缀为 `.upx.exe`（如 `udp2cal_x64.upx.exe`）。

### Android（项目根目录运行）

```
build_android.bat
# 交互式选择:
#   [1] arm64-v8a (64-bit) — android/
#   [2] armeabi-v7a (32-bit) — android/
#   [3] arm64-v8a (64-bit) — android_old/
#   [4] armeabi-v7a (32-bit) — android_old/
# 产物: udp2cal_arm64-v8a.apk / udp2cal_armeabi-v7a.apk
# 低端设备推荐 armeabi-v7a + android_old
```

**先决条件**: JDK 17 (Temurin)、Android SDK 35、NDK 27.0.12077973

> 构建脚本交互式选择目标版本和 ABI，产物自动移动到项目根目录并清理构建缓存。

> `android_old/` 也可单独构建：`cd android_old && gradlew assembleRelease -PtargetAbi=armeabi-v7a`

---

## 系统要求

| 平台 | 最低版本 | 架构 | 说明 |
|------|---------|------|------|
| **Android (android_new)** | **Android 10 (API 29)+** | arm64-v8a (64-bit) / armeabi-v7a (32-bit) | minSdk=29, targetSdk=35, 需要 `RECORD_AUDIO` 权限。面向现代设备，Jetpack Compose UI |
| **Android (android_old)** | **Android 6~8 (API 21~26)** | armeabi-v7a (32-bit 推荐) / arm64-v8a | minSdk=21, targetSdk=26, 无 Compose。专为低端/翻盖机定制的低性能版本 |
| **Windows** | **Windows 10+** | x86_64 (64-bit) / i686 (32-bit) | 依赖 WASAPI 音频引擎 + VB-Cable 虚拟声卡；系统托盘和防火墙规则仅 Windows 可用 |

> Android 编译要求 JDK 17、Android SDK 35、NDK 27.0.12077973；Windows 编译要求 Rust 1.96+、VS Build Tools 2022、CMake 3.22+。

---

## 主要特性

### 双向音频串流（v1.1.0 新增）
- **正向**：手机麦克风 → Opus 编码 → UDP → PC（VB-Cable 输出），逻辑不变
- **反向**：PC 扬声器 → WASAPI Loopback → Opus 编码 → UDP → 手机听筒/扬声器播放
- **Windows 端开关控制**：UI 按钮（开机自启右侧）控制反向串流启停，运行中不可更改
- **立体声编码**：高品质模式立体声最大码率全频带，手机端显示带宽+码率
- **低性能模式**：单声道 64kbps 宽带，complexity=1，适配低端 Android 设备

### Android 发送端（现代版，`android/`）
- **音源自适应切换**：仅一个 `isVoiceMode` 状态变量驱动全部音源决策。语音模式→VOICE_COMMUNICATION（安卓系统原生硬件降噪+AEC），全频模式→MIC（裸麦直出，硬件降噪关闭）；异常回退 MIC 保底通路
- **声学回声消除**：语音模式启用 AcousticEchoCanceler + MODE_IN_COMMUNICATION AudioTrack + 同音频 session 绑定
- **生产-消费双协程**：独占线程 `AudioRecord.read()` + `Channel` 投递消费者，消除饥饿。信号类型变更时消费者循环检测→自动重建 AudioRecord，保留网络层和反向播放器
- **全链路零堆分配**：`ShortArrayPool` 帧复用 + 乒乓发送缓冲区 + `encoderEncodeTo` 直接写入
- **纯直通无软件处理**：AudioRecord 取 PCM → 直通拷贝 → Opus 编码输出，APP 端无任何降噪/AGC/音效代码，硬件降噪由安卓系统全权托管
- **Opus CBR/VBR 热调节**：码率 32~512kbps；固定 48kHz 采集，协议层支持 5 种采样率（8k/12k/16k/24k/48k）
- **Opus 高级编码设置面板**：Jetpack Compose 全参数面板（复杂度/信号类型/带宽/VBR/FEC/DTX/丢包率/码率），面板内集成运行时状态卡片（音源/Opus编码/反向串流）和 1kHz 测试音开关。**手机调参实时同步到 PC 反向编码器**
- **1kHz 测试音模式**：内置正弦波发生器，无需麦克风即可调试端到端音频链路
- **JNI `@Synchronized` 互斥锁**：杜绝多线程并发闪退
- **网络无缝热重连**：改 IP 不重启录音流
- **UDP 广播自动发现**：统一 v2 协议发现局域网内 Windows 接收端
- **P2P 双向保活**：每 ~1s（50 帧）发送 TYPE_CONNECT（携带反向端口+模式标志）+ 非阻塞 1ms 超时 drainAck 收 CONNECT_ACK
- **优雅断连重连**：断连不退出采集，持续发 CONNECT 保活等待重连，收到 ACK 自动恢复；3 秒无 ACK 标记断连
- **持久化设备 ID**：8 字节唯一标识，SharedPreferences 存储，连接鉴权用

### Android Old 低性能版（`android_old/`）
专为 Android 6~8（API 21~26）低端设备/翻盖机定制的精简版本。

- **48kHz 全频带编码**：正向 48kHz Opus 编码，FB 全频带，256kbps VBR；反向 48kHz 立体声最大码率（PC 高品质模式）
- **40ms 帧合并编码**：合并 2 帧打包编码，编码频率 50→25 帧/秒，CPU 占用从 120% 降至 ~14%
- **JNI 动态帧长**：`opus_jni.c` 硬编码 `state->frame_size` → 改用实际数组长度，支持 20ms/40ms/60ms 灵活帧，修复 40ms 帧降调问题
- **CPU 自动降级**：读取 `/proc/self/stat` 进程 CPU 时间，每 2s 检测；>40% 时渐进降级（FB→SWB→WB→MB，码率 256→128→64→32kbps），通过 `encoder.update()` 热切换，无需重启采集流
- **ACK 限频轮询**：每 10 帧检查一次 ACK，`soTimeout` 5ms，drainAck CPU 开销降为 1/50
- **传统 View 布局**：无 Jetpack Compose，minSdk=21，targetSdk=26
- **一键自动连接**：UDP 广播发现局域网 PC 端，自动连接无需手动输入
- **均衡配置**：Opus 复杂度 1、DTX 开启、VBR 256kbps、全频带 FB
- **十字键交互**：上下调音量，左右切换关于页，适配物理键盘翻盖机
- **音源回退**：优先 VOICE_COMMUNICATION（系统硬件降噪），失败回退 MIC 裸采集

> 测试设备：SHARP NP805SH（翻盖机）— Snapdragon 210 (MSM8909) / 4×Cortex-A7 @1.1GHz / 1GB RAM / Android 8.1。优化后 CPU ~14%，温度 37°C 稳定。

---

### Opus 编码器参数说明（现代版 `android/` 设置面板）

现代版 Android App 内置 Opus 高级编码设置面板，以下为各参数详解：

| 参数 | 默认值 | 可选值 | 说明 |
|------|--------|--------|------|
| **编码复杂度** | 10（默认最高品质） | 1~10 (滑动条) | 1=最快速(省CPU)，10=最佳质量(费CPU)。局域网推荐较高复杂度 |
| **信号类型** | 语音 | 语音 / 音乐 | 语音模式启用系统硬件降噪(NS+AGC)+AEC；音乐模式裸麦直出 |
| **音频带宽** | 全频带(FB) | NB 8k / MB 12k / WB 16k / SWB 24k / FB 48k | 限制编码频率范围，越低越省码率。语音推荐 WB 或 SWB |
| **VBR (可变码率)** | 关闭(CBR) | 开启(VBR) / 关闭(CBR) | VBR 在静音时自动降低码率节省带宽；开启 DTX 时强制 VBR |
| **VBR 码率约束** | 无约束 | 无约束 / 约束 | 约束时 VBR 不超过目标码率；无约束时允许临时突增 |
| **DTX (不连续传输)** | 关闭 | 开启 / 关闭 | 开启后静音段停止传输数据，大幅节省带宽。开启后强制 VBR |
| **FEC (前向纠错)** | 关闭(0)（低延迟默认） | 关闭(0) / 开启(2) | 开启后丢包容忍度提高，适合弱网环境。局域网推荐关闭以降低延迟 |
| **预期丢包率** | 0%（低延迟默认） | 0~30% (滑动条) | 告知编码器预期丢包率，FEC 开启时按此比例插入冗余数据。局域网设置 0% 可避免冗余编码 |
| **码率** | 自动分配（默认） | 自动(0) / 32~512 kbps (手动) | 自动模式下：48kHz→512k，24kHz→256k，16kHz→128k，≤12kHz→64k |
| **采样率** | 48000 Hz | 固定不可调 | 采集固定 48kHz，协议层支持动态切换(8k/12k/16k/24k/48k) |

> 所有参数在编码运行中可热修改，无需重启采集流（通过 `encoder.update()` 每帧检测变更毫秒级同步）。

### Windows 接收端
- **Rust + iced 原生 UI**：暗色主题，实时音量电平显示
- **反向串流按钮**：UI 按钮控制 PC→Phone 反向音频启停（开机自启右侧），设置持久化到注册表
- **5 独立 Opus 解码器**：支持所有采样率无缝切换
- **相位连续流式重采样**：EMA 自适应漂移补偿，±0.2% 区间防止变调
- **VB-Cable 自动检测**：无虚拟声卡时回退到默认输出设备
- **系统托盘集成**：关闭按钮隐藏到托盘，右键菜单退出，双击恢复窗口；UI 显示 VB-Cable 状态、P2P 连接状态（●已占用/●已连接/●空闲）、反向串流状态
- **P2P 独占通信系统**：
  - 双状态机：广播(Ready 周期广播/Silent 停止) + 连接(Idle 监听/Busy 独占)
  - TYPE_CONNECT 唯一准入，READY 时拒绝所有音频
  - 设备 ID 1对1 过滤 + 并发互斥锁防抢占，重启 Win 即清空非法绑定
  - **防抢占静默拒绝**：DEVICE_BUSY 时异设备 TYPE_CONNECT → device_id 不匹配 → 不回复 CONNECT_ACK → 新设备 `drainAck()` 永 false → `p2pConnected` 永 false → 新设备不编不送音频（但持续发 CONNECT 等待，由 Win 端静默丢弃）；已有设备保活和音频流完全不受影响
- **双向保活**：CONNECT → CONNECT_ACK，1 秒保活周期；300ms 接收超时，1 秒无包标记断连
- **统一 v2 协议**：15B 包头携带设备 ID，所有包同格式
- **二进制发现服务**：监听 44043，v2 DISCOVER_REQ/REPLY
- **广播状态机**：READY 时每秒广播 TYPE_DISCOVER_REPLY 到 255.255.255.255:44043；BUSY 时自动静音
- **注册表持久化配置**：开机自启、监听地址、端口、设备 ID（8 字节 u64 存档）、反向串流开关
- **初始化状态检测**：Opus 解码器 / WASAPI 音频引擎初始化失败时实时反馈到 UI
- **Windows 防火墙自动放行**（静默 `NETSH`，无 CMD 窗口）
- **单实例互斥锁检测**：`CreateMutexW` 防止重复启动

---

## 文件结构

```
udp2mic/
├─ android/        # Android 发送端（Kotlin + JNI Opus，面向现代设备）
├─ android_old/    # Android 发送端低性能版（Kotlin + JNI Opus，48kHz FB 全频带，面向低端/翻盖机）
│  └─ app/src/main/java/com/udp2cal/app/
│     ├─ service/CaptureService.kt  # 采集服务（48kHz、40ms帧合并、ACK限频、CPU自动降级）
│     ├─ AudioPlayer.kt             # 反向音频播放器（48kHz 单声道输出）
│     ├─ UdpSender.kt               # UDP 发送器（drainAck 5ms超时）
│     ├─ MainActivity.kt            # 一键连接 + 十字键交互
│     └─ native/Opus*.kt            # Opus 编码器/解码器 JNI 封装（立体声解码）
├─ windows/        # Windows 接收端（Rust + iced + cpal）
│  ├─ src/
│  │  ├─ main.rs       # 主循环与 iced UI、托盘、UDP 接收流、音频工作线程
│  │  ├─ audio.rs      # WASAPI 输出 + 流式重采样
│  │  ├─ capture.rs    # WASAPI Loopback + Opus 编码 + 反向 UDP 发送
│  │  ├─ config.rs     # 注册表配置读写
│  │  ├─ decoder.rs    # Opus 解码器封装
│  │  ├─ firewall.rs   # 防火墙规则
│  │  └─ protocol.rs   # 协议重新导出
│  └─ Cargo.toml
├─ protocol/       # UDP 协议编解码（Rust crate，唯一真源）
├─ www/            # 项目官网 / 产品展示页（HTML + CSS 暗色主题）
├─ build_windows.bat
├─ build_android.bat
├─ d_install_apk.bat    # APK 一键安装脚本
├─ debug_adb.bat        # ADB 调试脚本（编译、安装、Logcat 监控）
├─ LICENSE
└─ README.md
```

---

## 编译产物

| 文件 | 大小 | 说明 |
| --- | --- | --- |
| `udp2cal_{x64,x86}.exe` | ~5.98 MB | Windows 接收端（架构后缀，UPX 压缩后为 `.upx.exe`）|
| `udp2cal_arm64-v8a.apk` | ~2.65 MB | Android 现代版 (64位，已签名) |
| `udp2cal_armeabi-v7a.apk` | ~2.65 MB | Android 现代版 (32位，已签名) |
| `android_old/app/build/outputs/apk/release/app-release.apk` | ~2.65 MB | Android Old 低性能版（推荐低端/翻盖机使用）|

---

## Windows 端详细说明

### 依赖一览

#### 构建依赖（`[build-dependencies]`）

| Crate | 用途 |
|-------|------|
| `winres` | 将 ICO 资源嵌入 EXE 文件 |
| `image` | 构建时将 `icon.png` 转为 256×256 ICO |

`build.rs` 流程：`icon.png` → Lanczos3 缩放 256×256 → `icon.ico` → `winres::WindowsResource` 嵌入 EXE PE 资源。

#### 运行时依赖（`[dependencies]`）

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

#### 关键依赖配置

```toml
[profile.release]
opt-level = "s"       # 按体积优化
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
> `tokio` 必须显式启用 `time` 和 `net` 特性，否则 `tokio::time::sleep` 和 `tokio::net::UdpSocket` 不可用。

---

### 技术栈

| 层级 | 技术选型 | 运行时更新策略（免重启） |
| --- | --- | --- |
| **UI 层（Android 现代版）** | Jetpack Compose + Material 3 | 采用 `Flow` 细粒度订阅与局部缓存变量；Opus 高级编码设置全参数面板（复杂度/信号/带宽/VBR/FEC/DTX/丢包率） |
| **UI 层（Android Old）** | 传统 View + AppCompat | 十字键交互、一键自动连接、关于页滑动切换；无 Compose，兼容 API 21+ |
| **UI 层（Windows）** | Iced (Rust) 360×300 固定窗口 | 模块化深色卡片布局 + 动态 VU 色彩表（绿/橙/红）+ 实时显示 VB-Cable 检测状态 + P2P 占用状态 + 反向串流状态；`run_with(session_id)` 强制状态隔离 |
| **网络层** | Kotlin Coroutines + UDP Socket / Rust async | 动态比对 `Prefs`，静默热重连不断流。v2 二进制广播自动发现 |
| **音频采集（现代版）** | AudioRecord（isVoiceMode 驱动 VOICE_COMMUNICATION / MIC，固定 48kHz） | **生产-消费双协程** + `ShortArrayPool` 零分配帧复用（池容量 5）；Opus信号类型变更时自动重建AudioRecord，保留网络层 |
| **音频采集（Old 低性能版）** | AudioRecord（VOICE_COMMUNICATION 优先，48kHz FB 全频带） | **生产-消费双协程** + **40ms帧合并编码**（2帧打包，JNI动态帧长，25帧/秒）；ACK限频检查（每10帧）；`delay(1)`防忙等；CPU自动降级（/proc/self/stat 每2s检测）|
| **核心算法** | —（无任何软件音频处理） | 仅维护 `isVoiceMode` 一个布尔状态；硬件降噪由安卓系统全权处理 |
| **编码层** | libopus (JNI) | `encoderEncodeTo` 直接写入 & 静态 1276B + 动态双重边界守卫 + `@Synchronized` 互斥锁；FEC=2（允许 CELT + FEC）突破 300kbps SILK 天花板 |
| **发送层** | UDP DatagramSocket | 双缓冲乒乓 + `send(offset,length)` 零拷贝，防脏数据；非阻塞 1ms 超时 drainAck（收 CONNECT_ACK）|
| **Windows 音频引擎** | cpal / WASAPI — 全局单例 Audio Worker 守护线程 | 仅在 `init_audio_worker()` 初始化一次，通过 `SyncChannel<AudioMessage>` 接收，UI 启停不影响底层音频线程 |

---

### 自动协商与热同步机制

| 协商项 | 策略 |
| --- | --- |
| 采样率（现代版） | Android 固定 48kHz 采集；接收端 5 独立 Opus 解码器自动匹配（8k/12k/16k/24k/48k） |
| 采样率（Old 低性能版） | Android 48kHz FB 全频带编码；PC 反向 48kHz 立体声最大码率（高品质模式）；Android 48kHz 解码+播放 |
| 帧长（Old 低性能版） | 合并 2 帧（40ms）打包编码，JNI 动态帧长，编码频率 25 帧/秒，CPU ~14%（实测 Sharp NP805SH） |
| 自动码率安全防线 | Prefs 码率 0 时根据采样率动态分配默认值（48kHz→512k，24kHz→256k，16kHz→128k，≤12kHz→64k）；Rust `compute_bitrate_id()`/`default_bitrate_for_sr()` 与 Kotlin 100% 对齐 |
| 传输机制 | 15 字节包头（v2 统一协议，Big-Endian）实时携带码率、采样率、8 字节设备 ID |
| 接收端解析 | `resolve_bitrate()` + 5 独立 Opus 解码器 + AudioWriter 相位连续重采样 |
| **局域网自动发现** | Windows 监听 44043，v2 二进制协议。Android 端 IP 框右侧图标一键搜索 |
| **心跳与保活** | Android 每 ~1s（50 帧）发 CONNECT（携带反向端口+模式标志+Opus 编码同步到 PC 反向端）；Win 端 300ms 接收超时 + 1s 无包标记断连；Android 3s 无 ACK 标记断连 |
| **反向串流保活** | 保活 CONNECT 携带反向端口，Win 重启后自动恢复反向串流。仅端口真正变化时重启发送器，防止每秒卡顿 |
| **模式切换** | 语音↔音乐模式切换时保留 UdpSender(网络层) 和 reverseDecoder，重建 AudioPlayer(路由/声道)，零网络中断 |

---

### 运行时行为

| 操作 | 行为 |
|------|------|
| 点击关闭按钮 (X) | 隐藏窗口到系统托盘，任务栏消失 |
| 点击最小化按钮 (—) | 最小化到任务栏 |
| 左键单击托盘图标 | 无操作（仅右键显示菜单） |
| 双击托盘图标 | 恢复并置顶主窗口 |
| 托盘右键 → 退出 | 退出程序 |

**UI 状态指示**：
| 指示器 | 含义 |
|--------|------|
| `● 等待连接 / ● 已断开` | 红色 — 正在监听但无数据流 |
| `● 已连接` | 绿色 — 收到音频数据 |
| `● 已占用 / ● 已占用 {device_id}` | 橙色 — v2 TYPE_CONNECT 独占绑定，异设备 CONNECT 将被静默拒绝 |
| `● 空闲` | 绿色 — 已启动但无可信连接 |
| `● VB-Cable` / `● VB-Cable 未安装` | 绿色/橙色 — 虚拟声卡检测状态 |
| `● 广播中` / `● 已静音` | 绿色/灰色 — 广播状态机 Ready/Silent |
| `[停止] [开机自启] [反向串流]` | 反向串流按钮 — 停止时可用点击切换，运行时锁定。开启后显示 `● 反向串流已启用` |

**配置持久化**：
- 配置存储在注册表 `HKCU\Software\UDP2CAL`
- 监听 IP、端口在点击"启动"时保存；设备 ID 首次运行时自动生成（8 字节 u64）
- 开机自启开关保存到 `HKCU\...\Run` 键 + 配置键
- 反向串流开关在停止时切换并保存到注册表，Win 重启后自动恢复
- 每次启动均从停止状态开始（`is_running` 为运行时状态，不持久化）

**局域网自动发现**：
- Windows 端常驻监听 UDP 端口 44043（独立线程 `start_broadcast_listener`）
- **v2 二进制协议**：手机端 `TYPE_DISCOVER_REQ` → PC 回复 `TYPE_DISCOVER_REPLY`（含端口 + 设备名）
- **广播状态机**（`start_broadcast_state_machine`）：READY 时每秒向 `255.255.255.255:44043` 广播 `TYPE_DISCOVER_REPLY`；BUSY 时自动静音
- 端口随用户修改实时更新（每次发现请求从注册表读取最新值）

**图标**：
- `icon.png` 通过 `include_bytes!` 编译期嵌入 EXE
- 同时用作窗口图标和托盘图标
- `build.rs` 另将 `icon.png` 转为 ICO 嵌入 EXE 文件资源（资源管理器图标）
- 若 `icon.png` 缺失或无法解码，回退到程序绘制的 32×32 绿色圆点

**P2P 防抢占拒绝行为**：

当 Windows 端处于 `DEVICE_BUSY`（已绑定一台 Android）时，第三方设备手动输入 IP:端口发送 `TYPE_CONNECT`：

| 角色 | 行为 |
|------|------|
| **Windows 端** | `device_id` 不匹配 → 不回复 `CONNECT_ACK`，静默丢弃；全局状态 `GLOBAL_DEVICE_STATE` 和 `BOUND_DEVICE_ID` **完全不变** |
| **第三方 Android 端** | `drainAck()` 1ms 超时永不返回 true → `p2pConnected` 永为 false → **不编码不发送任何音频数据**；持续发 CONNECT 等待（由 Win 端静默丢弃，不影响已有连接） |
| **已有 Android 端** | 正常收到 `CONNECT_ACK`（device_id 匹配保活分支），音频流正常解码播放，**完全不受影响** |

> 唯一例外：极低概率的 8 字节 device_id 碰撞会导致 Windows 误判为同设备保活，回复 `CONNECT_ACK` 到第三方。但由于第三方无合法音频采集流，仍无法发送音频数据。

**释放独占的三种途径**：
1. **Windows 端点击"停止"** → 调用 `reset_to_ready()` 清空 `BOUND_DEVICE_ID` + 恢复 `DEVICE_READY`
2. **已有 Android 端 1 秒无包超时** → `reset_to_ready()` 自动释放
3. **重启 Windows 程序** → 全局状态初始化回到 `DEVICE_READY`

---

### Windows 端架构精要

#### 数据流

```
[Android 手机] ──Opus 编码 UDP──→ [Windows PC]
    ↓
UDP 接收协程 → SyncChannel → 常驻 Audio Worker 线程（解码 + WASAPI 播放）
    ↓
VB-Cable / 扬声器 → 微信 / Zoom / OBS / 游戏
                                     ↑
[Windows PC] ──反向 WASAPI Loopback──┘
    ↓
WASAPI 捕获 → Opus 编码 → UDP → [Android 手机]
```

#### 全局单例音频守护线程

音频引擎（`audio`）和解码器（`decoder`）在整个软件生命周期内只初始化一次。无论 UI 如何点击"启动/停止"，底层音频守护线程不受影响。

```
      程序启动
         │
    ┌────┴────┐
    │ Audio   │ (常驻线程，不解构)
    │ Worker  │ ← SyncChannel<AudioMessage>
    └─────────┘
         │
    ┌────┴────┐
    │ Iced UI │ ← 仅负责网络收发和数据投递
    │ Network │
    └─────────┘
```

- **常驻后台线程**：`init_audio_worker()` 在 `main()` 中调用一次，通过 `SyncChannel<AudioMessage>` 接收数据
- **`Reset` 消息**替代完全销毁重建：收到后仅清空重排序缓冲区和 RMS 统计，不解构任何音频资源
- **`session_id` 计数器**：每次启动通过自增 `session_id` 强制 Iced 视为全新订阅，避免状态残留

#### UDP 接收流

`udp_receiver_stream` 通过参数传递 IP 和端口，不直接读取注册表，消除跨线程竞态：

```rust
fn subscription(&self) -> Subscription<Message> {
    let ip = self.config.listen_ip.clone();
    let port = self.config.listen_port;
    Subscription::run_with((ip, port), |data| udp_receiver_stream(data.0.clone(), data.1))
}
```

#### EMA 漂移补偿防线

`audio.rs` 中使用三分量 EMA 滤波器进行漂移补偿，更新前多重校验，并结合缓冲区水位快速修正：

```rust
// ── 每 3 秒：EMA 更新漂移比率 ──
if now.duration_since(self.last_ratio_update) > std::time::Duration::from_secs(3) {
    let current_fill = self.buf.lock().map(|b| b.len()).unwrap_or(0);
    let dt = now.duration_since(self.last_fill_time).as_secs_f64();
    if dt > 2.0 && self.last_fill_sample > 0 {
        let consume_rate = (self.last_fill_sample - current_fill) as f64 / dt;
        let produce_rate = self.device_rate as f64 - consume_rate;
        if produce_rate > 1000.0 && produce_rate.is_normal() {
            let measured = self.device_rate as f64 / produce_rate;
            if measured.is_finite() && measured > 0.5 && measured < 1.5 {
                self.drift_ratio = self.drift_ratio * 0.7 + measured * 0.3;
            }
        }
    }
    // 缓冲区严重偏离时极小步长快速修正
    let fill_pct = current_fill as f64 / self.target_fill as f64;
    if fill_pct < 0.5 {
        self.drift_ratio += 0.0005;
    } else if fill_pct > 1.5 {
        self.drift_ratio -= 0.0005;
    }
    self.drift_ratio = self.drift_ratio.clamp(0.998, 1.002); // ±0.2%
}
self.effective_ratio = (source_rate as f64 / self.device_rate as f64) / self.drift_ratio;
```

- `is_normal()`：排除零、次正则、Infinity、NaN；`is_finite()`：二次确认
- `0.5 ~ 1.5` 范围限制：防止极端值大幅污染 EMA
- **缓冲区水位修正**：低于 50% 或高于 150% 时以 `±0.0005` 步长微调，防止缓冲耗尽/膨胀
- **±0.2% 硬限幅**：`clamp(0.998, 1.002)` 确保漂移补偿不引起人耳可感知的变调
- **复用重采样缓冲区**：预分配 8192 f32（32KB），避免每帧 4KB 堆分配

#### 快速双击防抖

`ToggleRunning` 入口检查 200ms 防抖，避免旧 socket 未完全释放时新 socket 绑定失败。同时输入框 `PortChanged` 实时校验 `parse::<u16>()`，非法输入时边框变红。

```rust
if self.last_toggle_instant.elapsed() < Duration::from_millis(200) {
    return Task::none();
}
```

---

### 历史重大变更（Windows 端）

| 版本 | 变更点 |
|------|--------|
| **v1.1.0-hotfix3** | 2026-06-10: 三端代码审查修复。android WakeLock 无限期持有 + AEC 释放 + 模式切换时序修正；android_old 反向解码器立体声支持（修复高品质模式反向无声）+ 下混至单声道；windows capture.rs 编码循环 Vec 复用消除高频分配；Kotlin Protocol sampleRateToHz 对齐 Rust 48000 |
| **v1.1.0-hotfix2** | 2026-06-10: 全链路低延迟优化（500ms→50ms）；Opus 正反向编码实时同步；UI 重构（状态卡片/测试音/关于移入高级设置）；默认复杂度 10 + 自动码率；高刷新率屏幕适配；android_old 低延迟适配（AudioTrack 300→80ms） |
| **v1.1.0-hotfix** | 2026-06-09: android_old 发热优化。JNI 动态帧长修复 40ms 帧降调；48kHz FB+256kbps 最终定型（CPU 120%→14%，测试设备 Sharp NP805SH/骁龙210/1GB）；ACK 限频轮询（soTimeout 1→5ms，每10帧）；WakeLock 无限期持有 |
| **v1.0.9.2** | 2026-06-08: P2P 独占增强——Win 端异设备 CONNECT 静默拒绝（不推送 StatusUpdate，消除码率/电平跳动）；Android 端保活恢复持续发送；新增 `android_old/` 适配 Android 6~8 旧设备（无 Compose，低性能默认配置 + VOICE_COMMUNICATION 回退 + 一键自动连接）；`build_android.bat` 支持 4 选项同时编译新旧版本 |
| **v1.0.6** | UI 卡片布局 + 动态 VU 色彩表 + 按钮交互反馈；全局单例音频守护线程根除反复启停泄漏；`udp_receiver_stream` 参数化消除跨线程注册表竞态；EMA NaN/Infinity 防线；200ms 防抖 + 实时端口校验 |
| **v1.0.5** | Windows 接收端初始架构，局域网广播发现服务 |

---

## 命名约定

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

## 更多文档

| 文档 | 内容 |
| --- | --- |
| `protocol/README.md` | UDP 协议规范 |
| `www/index.html` | 项目官网/产品展示页（暗色主题） |

---

## 发布产物

```
udp2cal_x64.exe               # Windows 接收端 64-bit
udp2cal_x64.upx.exe           # UPX 压缩版（额外产物）
udp2cal_x86.exe               # Windows 接收端 32-bit
udp2cal_x86.upx.exe           # UPX 压缩版（额外产物）
udp2cal_arm64-v8a.apk         # Android 发送端 64-bit（已签名，ProGuard 缩减）
udp2cal_armeabi-v7a.apk       # Android 发送端 32-bit（已签名，ProGuard 缩减）
icon.png                      # 图标源文件（已内嵌到 exe，分发时可选附带）
```
