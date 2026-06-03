# UDP2Mic — 局域网麦克风

UDP2Mic 是一个轻量级的局域网麦克风解决方案，支持 Android 端采集（AGC、Wiener 降噪、噪声门）并通过 Opus 编码经 UDP 发送到 Windows 接收端，接收端通过 WASAPI 输出到虚拟声卡（VB-Cable）供任意应用使用。

目标用户：需要在局域网内将手机作为麦克风发送音频到 Windows 主机的开发者与个人用户。

主要特性：
- 低延迟 Opus CBR 编码（20ms 帧）
- 自适应采样率与码率（48k/24k/16k/8k）
- 人声增强链：Wiener 降噪、噪声门、AGC
- Windows 端使用 `iced` UI，支持悬浮窗与注册表配置
- 协议实现为独立 crate：`protocol/`（唯一协议真源）

快速开始

Windows（在项目根目录运行）：

```powershell
.\build_windows.bat
```

Android（在项目根目录运行）：

```powershell
.\build_android.bat
```

先决条件（摘要）
- Rust 1.96+、VS Build Tools 2022、CMake 3.22+
- Android: JDK 17、Android SDK 35、NDK 27.0.12077973

更多细节请参阅：
- 项目构建与技术细节： `docs/build.md`
- 交接与内部实现细节： `docs/handover.md`
- 协议规范： `protocol/README.md`

文件结构（简要）

```
udp2mic/
├─ android/        # Android 发送端（Kotlin + JNI Opus）
├─ windows/        # Windows 接收端（Rust + iced + cpal）
├─ protocol/       # UDP 协议（Rust crate，唯一真源）
├─ docs/           # 项目文档
├─ build_windows.bat
├─ build_android.bat
└─ README.md       # 本文件
```

如需贡献或运行问题，请先阅读 `docs/handover.md`。谢谢使用！
