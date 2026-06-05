# UDP2Mic — 局域网麦克风

> **当前版本: v1.0.6** — 智能 AGC 底噪安全区 + dBFS 噪声门 + 硬件降噪联动 + Windows 端容错强化

## 概述

```
Android 手机 → 智能 AGC(底噪追踪+10dB安全区) → dBFS噪声门 → Opus 编码 → UDP (广播自动发现) → Windows PC → WASAPI → VB-Cable → 任意应用
```

UDP2Mic 是一个**工业级稳定、低延迟**的局域网麦克风系统。Android 端采集音频，经 Opus 编码通过 UDP 发送到 Windows 接收端，接收端通过 WASAPI 输出到虚拟声卡（VB-Cable），可供微信、Zoom、OBS、游戏等任意应用使用。

**全链路零堆分配**（ByteArray + ShortArray 双闭环，仅池热身期 3 次构造）、**生产-消费双协程**消除阻塞饥饿、**JNI 双重边界守卫**防越界写穿。双端均已编译通过。

## 主要特性

### Android 发送端
- **生产-消费双协程**：独占线程 `AudioRecord.read()` + `Channel` 投递消费者，消除饥饿
- **全链路零堆分配**：`ShortArrayPool` 帧复用 + 乒乓发送缓冲区 + `encoderEncodeTo` 直接写入
- **智能 AGC（底噪安全区锁定）**：极慢底噪 dBFS 追踪 + 10dB 安全区（手动模式 0~20dB 可调），杜绝静音期 Noise Pumping；样点级线性插值平滑，目标人声 -18dBFS
- **自适应/手动 dBFS 噪声门**：-60~0dBFS 工业标准阈值，关门保留 10% 环境音掩蔽听觉断层
- **Android 硬件级 NoiseSuppressor 联动**：随噪声门开关热生效
- **Opus CBR/VBR 热调节**：5 种采样率（48k/24k/16k/12k/8k）自适应协商
- **JNI `@Synchronized` 互斥锁**：杜绝多线程并发闪退
- **网络无缝热重连**：改 IP 不重启录音流
- **UDP 广播自动发现**：一键搜索局域网内的 Windows 接收端

### Windows 接收端
- **Rust + iced 原生 UI**：暗色主题，悬浮窗实时音量显示
- **5 独立 Opus 解码器**：支持所有采样率无缝切换
- **相位连续流式重采样**：EMA 自适应漂移补偿，±0.2% 区间防止变调
- **VB-Cable 自动检测**：无虚拟声卡时回退到默认输出设备
- **局域网广播发现服务**：监听 44043 端口，自动回复手机搜索请求
- **注册表持久化配置**：开机自启、悬浮窗位置、监听地址
- **Windows 防火墙自动放行**
- **单实例互斥锁检测**

## 快速开始

### Windows（项目根目录运行）

```powershell
.\build_windows.bat
# 产物: udp2mic.exe
```

**先决条件**: Rust 1.96+、VS Build Tools 2022 (C++桌面开发)、CMake 3.22+

> 若使用 CMake ≥ 4.0，编译前需设置 `$env:CMAKE_POLICY_VERSION_MINIMUM="3.5"`。

### Android（项目根目录运行）

```powershell
.\build_android.bat
# 产物: udp2mic-release.apk
```

**先决条件**: JDK 17 (Temurin)、Android SDK 35、NDK 27.0.12077973

> 构建脚本会自动将产物复制到项目根目录。

## 文件结构

```
udp2mic/
├─ android/        # Android 发送端（Kotlin + JNI Opus）
├─ windows/        # Windows 接收端（Rust + iced + cpal）
│  ├─ src/
│  │  ├─ main.rs       # 主循环与 iced UI
│  │  ├─ audio.rs      # WASAPI 输出 + 流式重采样
│  │  ├─ config.rs     # 注册表配置读写
│  │  ├─ decoder.rs    # Opus 解码器封装
│  │  ├─ float.rs      # 悬浮窗线程（Win32 GDI）
│  │  ├─ firewall.rs   # 防火墙规则
│  │  └─ protocol.rs   # 协议重新导出
│  └─ Cargo.toml
├─ protocol/       # UDP 协议编解码（Rust crate，唯一真源）
├─ docs/
│  ├─ build.md         # 项目构建与技术细节
│  └─ handover.md      # 架构精要与避坑指南
├─ build_windows.bat
├─ build_android.bat
└─ README.md
```

## 更多文档

| 文档 | 内容 |
| --- | --- |
| `docs/build.md` | 编译方法、技术栈、自动协商机制、命名约定 |
| `docs/handover.md` | 架构精要、双协程流水线、零分配实现、JNI 防卫、Changelog |
| `protocol/README.md` | 协议规范 |

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
