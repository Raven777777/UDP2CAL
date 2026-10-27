# UDP2CAL — 局域网双向音频串流

把 Android 手机当作电脑的麦克风与扬声器：手机麦克风音频经 Opus 编码通过局域网送达 PC 的虚拟声卡，PC 扬声器音频反向回传到手机听筒。

接收端稳态水位目标默认 150 ms（抖动缓冲 + PCM 环 + 声卡缓冲）；端到端延迟还包含采集、编码、发送与网络传输，实际值随设备/网络变化。界面显示的是**接收端可测部分**，不包含发送端与网络。

同一链路实测：目标 150 → 总延迟 156–168 ms；目标 80 → 83–99 ms 且 `PLC=0`。目标可用 `UDP2CAL_WATER_TARGET_MS`（80–600，越界钳位）现场试，但**不要靠压低目标去赢延迟**——水位就是断供时的全部缓冲（见 [docs/LATENCY_CALIBRATION.md](docs/LATENCY_CALIBRATION.md) §5.2）。

```
Android 手机 ↔ 双向音频 ↔ Windows PC
  正向: 手机麦克风 → Opus 编码 → UDP → PC → WASAPI → VB-Cable → 任意应用
  反向: PC 扬声器 → WASAPI Loopback → Opus 编码 → UDP → 手机听筒/扬声器
```

---

> ### 旧手机支持
>
> 本仓库仅维护 Android 10+ 的 `android/` 客户端与配套的 `windows/`、`protocol/`。
>
> **Android 6~8 旧手机**所需的 App 与 Windows 端软件，请前往 **[Release v1.1.1](https://github.com/Raven777777/UDP2CAL/releases/tag/v1.1.1)** 下载；两者需成对使用，旧版 App 无法连接当前版本的 PC 端。

---

## 组件

| 目录                 | 说明                                                                                | 系统要求                        |
| ------------------ | --------------------------------------------------------------------------------- | --------------------------- |
| `windows/`         | Windows 接收端（Rust + iced），WASAPI 播放 / Loopback 回传，系统托盘常驻                           | Windows 10+，需 VB-Cable 虚拟声卡 |
| `android/`         | Android 客户端（Kotlin + Jetpack Compose），全参数 Opus 面板                                 | Android 10 (API 29)+        |
| `opus-src/`        | libopus 1.6.1 源码树（Android JNI 与 Windows 共用的唯一副本，勿在其内部另存副本）                            | —                           |
| `vendor/`          | `audiopus` / `audiopus_sys` 本地副本（含本地扩展，经 `[patch.crates-io]` 引用，勿用上游覆盖）                | —                           |
| `protocol/`        | 协议 v3 crate：控制面 + RTP 媒体面（协议唯一真源；Android 端经 JNI 复用同一份实现）                          | —                           |

---

## 快速开始

```bat
build.bat win --release                              :: 编译 Windows 接收端
build.bat android --modern --abi arm64-v8a --release  :: 编译 Android 客户端
build.bat doctor --fix                               :: 环境体检与自动修复
```

不带参数运行 `build.bat` 可进入交互式菜单。

**开发**：Windows Rust 1.96+、VS Build Tools 2022、CMake 3.22+；Android JDK 17~22、Android SDK 35、NDK 27.0.12077973，外加 Rust `aarch64-linux-android` target（协议层交叉编译成 `libudp2cal_protocol.so`，见 [docs/BUILD.md](docs/BUILD.md)）

---

## 文档

全部技术文档位于 [`docs/`](docs/README.md)。

| 文档                                           | 内容                              |
| -------------------------------------------- | ------------------------------- |
| [docs/BUILD.md](docs/BUILD.md)               | 环境要求、`build.bat` 全部命令、编译产物、常见问题 |
| [docs/FEATURES.md](docs/FEATURES.md)         | 两端功能特性、Opus 参数详解、界面截图           |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 项目结构、技术栈、运行时行为、代码落点、命名约定      |
| [docs/PROTOCOL_V3_RTP.md](docs/PROTOCOL_V3_RTP.md) | **协议规范（现行）**：RTP 媒体面、控制面、时钟模型 |
| [docs/LATENCY_CALIBRATION.md](docs/LATENCY_CALIBRATION.md) | 延迟口径、`playhead_ts` 与 DMA 缓冲、实测校准流程 |
| [docs/CHANGELOG.md](docs/CHANGELOG.md)       | 版本历史                            |
| [docs/BUGFIX.md](docs/BUGFIX.md)             | 现行代码的 Bug 根因档案                  |
| [docs/LEGAL.md](docs/LEGAL.md)               | 版权声明、GPL-3.0 授权、第三方组件许可         |
| [docs/archive/](docs/archive/)               | 历史存档：v2 协议、迁移计划、历次审查报告、旧修复记录   |

---

## 许可证

© 2026 UDP2CAL Contributors. 本程序为自由软件，以 **GNU General Public License v3.0** 发布，详见 [`LICENSE`](LICENSE)；第三方开源组件许可见 [docs/LEGAL.md](docs/LEGAL.md)。
