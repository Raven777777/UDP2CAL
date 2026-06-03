# UDP2Mic 项目文档
> 最后更新: 2026-06-03 | 状态: **正式版 v1.0.0 — 全部问题已修复，零泄漏零Warning**

> 项目简介与快速开始见根目录: [README.md](../README.md)

## 项目概述

UDP2Mic 是一个局域网麦克风系统：

```
Android 手机 → AGC+Wiener降噪+噪声门 → Opus CBR → UDP → Windows PC → WASAPI → VB-Cable → 任何应用
```

**双端均已编译通过，协议 100% 对齐。**

---

## 一键构建

### Windows

```powershell
build_windows.bat
# 产物: udp2mic.exe (项目根目录)
```

**前提**: Rust 1.96+, VS Build Tools 2022 (C++桌面开发), CMake 3.22+

### Android

```batch
build_android.bat
# 产物: udp2mic-release.apk (项目根目录，已签名)
```

**前提**: JDK 17 (Temurin), Android SDK 35, NDK 27.0.12077973

> 构建脚本会自动将产物复制到项目根目录。

---

## 编译产物

| 文件 | 大小 | 说明 |
|------|------|------|
| `udp2mic.exe` | ~4.8 MB | Windows 接收端 |
| `udp2mic-release.apk` | ~2.4 MB | Android 发送端 (已签名) |

---

## 技术栈

| 层 | Windows | Android |
|----|---------|---------|
| 语言 | Rust 1.96+ | Kotlin + C (JNI) |
| UI | iced 0.13 (tiny-skia) | Jetpack Compose Material3 |
| 网络 | tokio::net::UdpSocket | java.net.DatagramSocket |
| 编码 | audiopus 0.3.0-rc.0 (decoder) | libopus 1.5.2 via JNI (encoder) |
| 音频 | cpal 0.15 (WASAPI) | AudioRecord (MIC source) |
| 输出 | VB-Audio Virtual Cable | UDP → 局域网 |
| 配置 | 注册表 HKCU\Software\UDP2Mic | SharedPreferences udp2mic_prefs |

---

## 关键技术参数

| 参数 | 值 |
|------|-----|
| 采样率 | 自动协商: 48000→24000→16000→8000 Hz (排除44.1kHz) |
| 码率 | 自动协商: 128kbps→64kbps→48kbps→24kbps |
| 漂移补偿 | 自适应帧内插值, ±0.2% clamp, EMA α=0.3 |
| 缓冲策略 | 200ms 预填静音 + 1秒上限 (Mutex\<VecDeque\> + Weak 弱引用) |
| 通道 | 单声道 → 复制到所有输出通道 (支持立体声设备) |
| 帧长 | 20ms (960 samples @48kHz) |
| 编码 | Opus CBR, DTX=OFF, VBR=OFF, COMPLEXITY=10, SIGNAL=MUSIC |
| 协议 | v1: 6字节包头 + Opus payload |
| 最大负载 | 1472 字节 (MTU 安全) |
| 乱序窗口 | MAX_REORDER=8 |
| UDP 端口 | 8899 (默认) |
| UI 尺寸 | 380×300 (主窗口) + 240×28 (悬浮窗) |

### 自动协商策略

| 检测到的采样率 | 自动匹配码率 |
|---------------|-------------|
| 48000 Hz | **128 kbps** |
| 24000 Hz | 64 kbps |
| 16000 Hz | 48 kbps |
| 8000 Hz | 24 kbps |

---

## 编译产物

| 文件 | 大小 | 说明 |
|------|------|------|
| `udp2mic.exe` | ~4.8 MB | Windows 接收端 (Release, strip) |
| `udp2mic-release.apk` | ~2.4 MB | Android 发送端 (已签名, R8 混淆) |

> 注: Windows 编译需设置 `CMAKE_POLICY_VERSION_MINIMUM=3.5` 环境变量以兼容新版 CMake。脚本已包含此设置。

## 项目文件清单

```
udp2mic/
├── protocol/                      # Rust crate: udp2mic-protocol (唯一协议源)
│   ├── Cargo.toml
│   └── protocol.rs                # 包头编解码 + ReorderBuffer
├── windows/                       # Windows 接收端
│   ├── Cargo.toml
│   ├── build.rs                   # 嵌入图标资源
│   └── src/
│       ├── main.rs                # iced UI + UDP 接收流 + 状态管理 (Subscription::run_with_id 防泄漏)
│       ├── audio.rs               # cpal WASAPI → RingBuffer → 音频输出 (Weak 弱引用防泄漏)
│       ├── decoder.rs             # audiopus OpusDecoder 封装 (5采样率 + shrink_pcm_buf)
│       ├── protocol.rs            # pub use udp2mic_protocol::* (重导出)
│       ├── config.rs              # 注册表读写 + Run 键开机自启
│       ├── float.rs               # Win32 分层透明悬浮窗 (visible 保护)
│       └── firewall.rs            # netsh 防火墙规则 (静默)
├── android/                       # Android 发送端
│   └── app/src/main/
│       ├── cpp/
│       │   ├── CMakeLists.txt     # NDK 构建 (本地 opus-src)
│       │   └── opus_jni.c         # C 层 Opus 编码 (OPUS_SIGNAL_MUSIC)
│       └── java/com/udp2mic/app/
│           ├── MainActivity.kt    # Compose UI + 自动采样率协商
│           ├── service/CaptureService.kt  # 前台采集 + AGC+Wiener+噪声门
│           ├── native/OpusEncoder.kt      # JNI 编码器封装 (Closeable)
│           ├── native/OpusNative.kt       # JNI 声明
│           ├── Udp2MicProtocol.kt         # Kotlin 协议 (与 Rust 对齐)
│           ├── UdpSender.kt              # UDP 发送 (DNS 缓存 + 超时)
│           └── Prefs.kt                  # SharedPreferences
├── icon.png                       # 应用图标
├── build_windows.bat              # Windows 一键构建 → 复制到根目录
├── build_android.bat              # Android 构建(含签名) → 复制到根目录
├── debug_adb.bat                  # ADB 调试工具
└── docs/
    ├── build.md                   # 本文档
    └── handover.md                # AI 继任者指南
```

---

## 配置项

### Windows (注册表 HKCU\Software\UDP2Mic)

| 键 | 类型 | 默认值 | 说明 |
|----|------|--------|------|
| `listen_ip` | REG_SZ | `0.0.0.0` | 监听地址 |
| `listen_port` | REG_DWORD | `8899` | 监听端口 |
| `auto_start` | REG_DWORD | `0` | 开机自启 |
| `float_window_enable` | REG_DWORD | `1` | 悬浮窗开关 |
| `float_window_x` | REG_DWORD | `100` | 悬浮窗 X 坐标 |
| `float_window_y` | REG_DWORD | `100` | 悬浮窗 Y 坐标 |

### Android (SharedPreferences udp2mic_prefs)

| 键 | 类型 | 默认值 | 说明 |
|----|------|--------|------|
| `target_ip` | String | `192.168.1.100` | 目标 IP |
| `target_port` | Int | `8899` | 目标端口 |
| `test_tone` | Boolean | `false` | 1kHz测试音模式 |
| `noise_reduction` | Boolean | `false` | 噪声门开关 |
| `voice_enhance` | Boolean | `false` | 人声降噪 (Wiener) 开关 |

---

## 音频处理链 (Android 端)

```
原始 PCM → [Wiener 人声降噪] → [噪声门] → [AGC 增益] → Opus 编码
```

| 模块 | 原理 | 关键参数 |
|------|------|----------|
| **Wiener 降噪** | SNR²/(SNR²+1) 帧级增益, 自适应噪声底 | 学习 ~1.6s, 增益下限 0.2 |
| **噪声门** | 自适应底噪 + 阈值 + 噪声底冻结 | 3.5x 阈值, 30帧保持, 快开慢关 |
| **AGC** | EMA RMS 追踪, 快攻/慢放 | 目标 -16.5dBFS, 2x~40x |
| **NoiseSuppressor** | Android 硬件 API | 设备支持时优先, NS✓ 标记 |

---

## 自动协商机制 (v1)

| 协商项 | 策略 |
|--------|------|
| 采样率 | `MainActivity.detectBestSampleRate()` 按 48k→24k→16k→8k 优先级检测 |
| 码率 | `MainActivity.autoBitrate()` 根据采样率自动计算 |
| 传输 | 码率和采样率通过 6 字节包头 Byte[4]/Byte[0] 低4位传给接收端 |
| 接收端 | `protocol::resolve_bitrate()` 自动解析; 5个独立 Opus 解码器; AudioWriter 自动重采样 |

---

## 命名约定

| 位置 | 值 |
|------|-----|
| 项目/EXE/APK | `UDP2Mic` / `udp2mic.exe` |
| Rust crate | `udp2mic` / `udp2mic-protocol` |
| 注册表 | `HKCU\Software\UDP2Mic` |
| 开机自启 Run 键 | `UDP2Mic` |
| 防火墙规则 | `UDP2Mic 局域网麦克风` |
| 单实例互斥锁 | `UDP2Mic_SingleInstance_Mutex` |
| 线程名 | `udp2mic-audio`, `udp2mic-fw` |
| Android 包名 | `com.udp2mic.app` |
| SharedPreferences | `udp2mic_prefs` |
| 通知频道 | `udp2mic_capture` |
| WAKE_LOCK tag | `UDP2Mic:Capture` |
| 悬浮窗类名 | `LmFlo` |

---

## 已知未解决问题

| # | 问题 | 影响 | 状态 |
|---|------|------|------|
| 1 | **立体声交错写入Bug** | 升调+Underrun | ✅ 已修复 |
| 2 | **opus_jni.c 缺 OPUS_SET_SIGNAL(MUSIC)** | 低频削减 | ✅ 已修复 |
| 5 | **🔴 内存泄漏** | ~每17秒+0.1MB | ✅ 已修复 |
| 6 | **P4-P6: 音频处理链** | UNPROCESSED 低电平 + 噪声 | ✅ 已修复 |

> 详细 Bug 记录和修复方案见 [docs/handover.md](./handover.md)
> 协议文档见 [protocol/README.md](../protocol/README.md)
