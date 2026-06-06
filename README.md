# UDP2Mic — 局域网麦克风

> **当前版本: v1.0.8** — Opus信号类型驱动音源自适应切换（isVoiceMode），VOICE_COMMUNICATION/MIC双模式 + 异常回退MIC保底通路
>
> ⚠️ **Android 端已完备**，除非遇到致命 bug 否则不再更新。后续开发重点为 Windows 接收端。

## 概述

```
Android 手机 → isVoiceMode?→VOICE_COMMUNICATION(系统硬件降噪)/MIC(裸麦直出) → 直通无软件处理 → Opus 编码 → UDP (广播自动发现) → Windows PC → WASAPI → VB-Cable → 任意应用
```

UDP2Mic 是一个**工业级稳定、低延迟**的局域网麦克风系统。Android 端采集音频——仅维护一个 `isVoiceMode` 状态变量，由 Opus 信号类型驱动音源自动切换，**无任何软件降噪/AGC/音效处理代码**——经 Opus 编码通过 UDP 发送到 Windows 接收端，接收端通过 WASAPI 输出到虚拟声卡（VB-Cable），可供微信、Zoom、OBS、游戏等任意应用使用。

**全链路零堆分配**（ByteArray + ShortArray 双闭环，仅池热身期 3 次构造）、**生产-消费双协程**消除阻塞饥饿、**JNI 双重边界守卫**防越界写穿。双端均已编译通过。

---

## 快速开始

### Windows（项目根目录运行）

```
build_windows.bat
# 交互式选择:
#   [1] x86_64 (64-bit)  → udp2mic_x64.exe
#   [2] i686   (32-bit)  → udp2mic_x86.exe
# 产物: udp2mic_{arch}.exe (项目根目录)
```

**先决条件**: Rust 1.96+、VS Build Tools 2022 (C++桌面开发)、CMake 3.22+

> 若使用 CMake ≥ 4.0，编译前需设置 `$env:CMAKE_POLICY_VERSION_MINIMUM="3.5"`。
>
> 若系统中安装了 `upx.exe`，编译后会询问是否压缩 EXE；压缩后的产物后缀为 `.upx.exe`（如 `udp2mic_x64.upx.exe`）。

### Android（项目根目录运行）

```
build_android.bat
# 产物: udp2mic_arm64-v8a.apk 或 udp2mic_armeabi-v7a.apk
# 运行后选择 [1] arm64-v8a (64位) 或 [2] armeabi-v7a (32位)
```

**先决条件**: JDK 17 (Temurin)、Android SDK 35、NDK 27.0.12077973

> 构建脚本交互式选择目标 ABI，产物自动移动到项目根目录并清理构建缓存。

---

## 主要特性

### Android 发送端
- **音源自适应切换**：仅一个 `isVoiceMode` 状态变量驱动全部音源决策。语音模式→VOICE_COMMUNICATION（安卓系统原生硬件降噪），全频模式→MIC（裸麦直出，硬件降噪关闭）；异常回退 MIC 保底通路
- **生产-消费双协程**：独占线程 `AudioRecord.read()` + `Channel` 投递消费者，消除饥饿。信号类型变更时消费者循环检测→自动重建 AudioRecord
- **全链路零堆分配**：`ShortArrayPool` 帧复用 + 乒乓发送缓冲区 + `encoderEncodeTo` 直接写入
- **纯直通无软件处理**：AudioRecord 取 PCM → 直通拷贝 → Opus 编码输出，APP 端无任何降噪/AGC/音效代码，硬件降噪由安卓系统全权托管
- **Opus CBR/VBR 热调节**：FEC=2（允许 CELT + FEC，不强制 SILK 模式）；码率 32~512kbps（OPUS 协议上限 510kbps）；5 种采样率自适应协商
- **JNI `@Synchronized` 互斥锁**：杜绝多线程并发闪退
- **网络无缝热重连**：改 IP 不重启录音流
- **UDP 广播自动发现**：一键搜索局域网内的 Windows 接收端

### Windows 接收端
- **Rust + iced 原生 UI**：暗色主题，实时音量电平显示
- **5 独立 Opus 解码器**：支持所有采样率无缝切换
- **相位连续流式重采样**：EMA 自适应漂移补偿，±0.2% 区间防止变调
- **VB-Cable 自动检测**：无虚拟声卡时回退到默认输出设备
- **系统托盘集成**：关闭按钮隐藏到托盘，右键菜单退出，双击恢复窗口
- **局域网广播发现服务**：监听 44043 端口，自动回复手机搜索请求
- **注册表持久化配置**：开机自启、监听地址、端口
- **Windows 防火墙自动放行**
- **单实例互斥锁检测**

---

## 文件结构

```
udp2mic/
├─ android/        # Android 发送端（Kotlin + JNI Opus）
├─ windows/        # Windows 接收端（Rust + iced + cpal）
│  ├─ src/
│  │  ├─ main.rs       # 主循环与 iced UI、托盘、UDP 接收流、音频工作线程
│  │  ├─ audio.rs      # WASAPI 输出 + 流式重采样
│  │  ├─ config.rs     # 注册表配置读写
│  │  ├─ decoder.rs    # Opus 解码器封装
│  │  ├─ firewall.rs   # 防火墙规则
│  │  └─ protocol.rs   # 协议重新导出
│  └─ Cargo.toml
├─ protocol/       # UDP 协议编解码（Rust crate，唯一真源）
├─ build_windows.bat
├─ build_android.bat
└─ README.md
```

---

## 编译产物

| 文件 | 大小 | 说明 |
| --- | --- | --- |
| `udp2mic_{x64,x86}.exe` | ~4.8 MB | Windows 接收端（架构后缀，UPX 压缩后为 `.upx.exe`）|
| `udp2mic_arm64-v8a.apk` | ~2.4 MB | Android 发送端 (64位，已签名) |
| `udp2mic_armeabi-v7a.apk` | ~2.4 MB | Android 发送端 (32位，已签名) |

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
| `udp2mic-protocol` | local | 局域网 UDP 协议（包头编解码、重排序缓冲区） |
| `iced` | 0.14 | GUI 框架（tiny-skia 软件渲染，tokio 运行时） |
| `tokio` | 1.40 | 异步 UDP 收发、超时控制、事件流 |
| `cpal` | 0.15 | WASAPI 音频播放 |
| `audiopus` | 0.3.0-rc.0 | Opus 音频解码 |
| `windows` | 0.58 | Win32 API（单实例互斥、窗口控制） |
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
    "Win32_Foundation",
] }
```

> `iced` 必须禁用默认特性（避免 wgpu 后端引发 `windows` crate 版本冲突），显式启用 `tokio` + `tiny-skia` 渲染器。
> `tokio` 必须显式启用 `time` 和 `net` 特性，否则 `tokio::time::sleep` 和 `tokio::net::UdpSocket` 不可用。

---

### 技术栈

| 层级 | 技术选型 | 运行时更新策略（免重启） |
| --- | --- | --- |
| **UI 层（Android）** | Jetpack Compose | 采用 `Flow` 细粒度订阅与局部缓存变量，杜绝高频重绘引发的滑块卡顿 |
| **UI 层（Windows）** | Iced (Rust) | 模块化深色卡片布局 + 动态 VU 色彩表 + 按钮 hover/pressed 反馈；`run_with(session_id)` 强制状态隔离 |
| **网络层** | Kotlin Coroutines + UDP Socket / Rust async | 动态比对 `Prefs`，静默热重连不断流。支持 UDP 广播自动发现 |
| **音频采集** | AudioRecord（isVoiceMode 驱动 VOICE_COMMUNICATION / MIC） | **生产-消费双协程** + `ShortArrayPool` 零分配帧复用（池容量 5）；Opus信号类型变更时自动重建AudioRecord |
| **核心算法** | —（无任何软件音频处理） | 仅维护 `isVoiceMode` 一个布尔状态；硬件降噪由安卓系统全权处理 |
| **编码层** | libopus (JNI) | `encoderEncodeTo` 直接写入 & 双重边界守卫 + `@Synchronized` 互斥锁；FEC=2（允许 CELT + FEC）突破 300kbps SILK 天花板 |
| **发送层** | UDP DatagramSocket | 乒乓缓冲区 + `send(offset,length)` 零拷贝，防脏数据 |
| **Windows 音频引擎** | cpal / WASAPI — 全局单例 Audio Worker 守护线程 | 仅在 `init_audio_worker()` 初始化一次，通过 `SyncChannel<AudioMessage>` 接收，UI 启停不影响底层音频线程 |

---

### 自动协商与热同步机制

| 协商项 | 策略 |
| --- | --- |
| 采样率 | 固定 48kHz（48k→24k→16k→8k 优先级硬匹配） |
| 自动码率安全防线 | Prefs 码率 0 时根据采样率动态分配默认值（48kHz→512k，24kHz→256k，16kHz→128k，≤12kHz→64k）；Rust `compute_bitrate_id()`/`default_bitrate_for_sr()` 与 Kotlin `computeBitrateId` 100% 对齐 |
| 传输机制 | 6 字节包头（Big-Endian）实时携带码率/采样率 |
| 接收端解析 | `resolve_bitrate()` + 5 独立 Opus 解码器 + AudioWriter 重采样 |
| **局域网自动发现** | Windows 监听 44043，回复 `"UDP2MIC_REPLY:{port}"`。Android 端 IP 框右侧图标一键搜索 |

---

### 运行时行为

| 操作 | 行为 |
|------|------|
| 点击关闭按钮 (X) | 隐藏窗口到系统托盘，任务栏消失 |
| 点击最小化按钮 (—) | 最小化到任务栏 |
| 左键单击托盘图标 | 无操作（仅右键显示菜单） |
| 双击托盘图标 | 恢复并置顶主窗口 |
| 托盘右键 → 退出 | 退出程序 |

**配置持久化**：
- 配置存储在注册表 `HKCU\Software\UDP2Mic`
- 监听 IP、端口在点击"启动"时保存
- 开机自启开关保存到 `HKCU\...\Run` 键 + 配置键
- 每次启动均从停止状态开始（`is_running` 为运行时状态，不持久化）

**局域网自动发现**：
- Windows 端监听 UDP 端口 44043
- 手机端广播 `UDP2MIC_DISCOVER` → PC 回复 `UDP2MIC_REPLY:{当前监听端口}`
- 端口随用户修改实时更新（每次发现请求从注册表读取最新值）

**图标**：
- `icon.png` 通过 `include_bytes!` 编译期嵌入 EXE
- 同时用作窗口图标和托盘图标
- `build.rs` 另将 `icon.png` 转为 ICO 嵌入 EXE 文件资源（资源管理器图标）
- 若 `icon.png` 缺失或无法解码，回退到程序绘制的 32×32 绿色圆点

---

### Windows 端架构精要

#### 数据流

```
[Android 手机] ──Opus 编码 UDP──→ [Windows PC]
    ↓
UDP 接收协程 → SyncChannel → 常驻 Audio Worker 线程（解码 + WASAPI 播放）
    ↓
VB-Cable / 扬声器 → 微信 / Zoom / OBS / 游戏
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

`audio.rs` 中使用三分量 EMA 滤波器进行漂移补偿，更新前三层校验：

```rust
if produce_rate > 1000.0 && produce_rate.is_normal() {
    let measured = self.device_rate as f64 / produce_rate;
    if measured.is_finite() && measured > 0.5 && measured < 1.5 {
        self.drift_ratio = self.drift_ratio * 0.7 + measured * 0.3;
    }
}
```

- `is_normal()`：排除零、次正则、Infinity、NaN
- `is_finite()`：二次确认
- `0.5 ~ 1.5` 范围限制：防止极端值大幅污染 EMA

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
| **v1.0.8** | 升级 iced 0.14 + tokio 1.52；系统托盘完整功能（右键菜单退出、双击恢复、左键不弹出）；`icon.png` 编译期内嵌 exe；`build_windows.bat` 交互式 x86/x64 选编译 + UPX 压缩；广播发现端口实时更新；`docs/` 合并入 README；代码清理（零警告） |
| **v1.0.6** | UI 卡片布局 + 动态 VU 色彩表 + 按钮交互反馈；全局单例音频守护线程根除反复启停泄漏；`udp_receiver_stream` 参数化消除跨线程注册表竞态；EMA NaN/Infinity 防线；200ms 防抖 + 实时端口校验 |
| **v1.0.5** | Windows 接收端初始架构，局域网广播发现服务 |

---

## 命名约定

| 项目 | 值 |
| --- | --- |
| 项目/EXE/APK | `UDP2Mic` / `udp2mic.exe` |
| Rust crate | `udp2mic` / `udp2mic-protocol` |
| 注册表 | `HKCU\Software\UDP2Mic` |
| 开机自启 Run 键 | `UDP2Mic` |
| 防火墙规则 | `UDP2Mic 局域网麦克风` |
| 单实例互斥锁 | `UDP2Mic_SingleInstance_Mutex` |
| 发现服务端口 | `44043` |
| Android 包名 | `com.udp2mic.app` |

## 更多文档

| 文档 | 内容 |
| --- | --- |
| `protocol/README.md` | UDP 协议规范 |

---

## 发布产物

```
udp2mic_x64.exe       # Windows 接收端 64-bit（使用 UPX 则为 .upx.exe）
udp2mic_x86.exe       # Windows 接收端 32-bit
icon.png              # 图标源文件（已内嵌到 exe，分发时可选附带）
```
