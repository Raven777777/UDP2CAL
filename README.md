# UDP2CAL — 局域网双向音频串流

把 Android 手机当作电脑的麦克风与扬声器：手机麦克风音频经 Opus 编码通过局域网送达 PC 的虚拟声卡，PC 扬声器音频反向回传到手机听筒。端到端延迟 50~80ms。

```
Android 手机 ↔ 双向音频 ↔ Windows PC
  正向: 手机麦克风 → Opus 编码 → UDP → PC → WASAPI → VB-Cable → 任意应用
  反向: PC 扬声器 → WASAPI Loopback → Opus 编码 → UDP → 手机听筒/扬声器
```

---

> ### ⚠️ 项目状态
>
> **`android_old/`（Android 6~8 低性能版）已停止开发，并自 v1.1.2 起被移出兼容链。**
>
> `windows/` 与 `protocol/` 中原有的低性能模式适配代码（`low_perf` 标志、16kHz 单声道反向编码、旧版本探测与 v1 拒绝分支）已全部删除。当前 PC 端只接受 `android/` 现代版，与 `android_old/` **不互通**；该目录源码仅作归档保留。
>
> 旧设备如需继续使用，请成对使用 **[Release v1.1.1](https://github.com/Raven777777/UDP2CAL/releases/tag/v1.1.1)** 的 Windows 端与已签名 APK——v1.1.1 的旧 APK 无法连接 v1.1.2 及以后的 PC 端。
>
> 当前维护范围：`windows/`、`android/`、`protocol/`。

---

## 组件

| 目录 | 说明 | 系统要求 |
|------|------|----------|
| `windows/` | Windows 接收端（Rust + iced），WASAPI 播放 / Loopback 回传，系统托盘常驻 | Windows 10+，需 VB-Cable 虚拟声卡 |
| `android/` | Android 现代版（Kotlin + Jetpack Compose），全参数 Opus 面板 | Android 10 (API 29)+ |
| `protocol/` | UDP v2 协议编解码 crate（协议唯一真源） | — |
| ~~`android_old/`~~ | ~~Android 低性能版（传统 View），为 Android 6~8 翻盖机优化~~ — **已停止开发，仅归档源码；v1.1.2 起 PC 端不再兼容** | — |

---

## 快速开始

```bat
build.bat win --release                              :: 编译 Windows 接收端
build.bat android --modern --abi arm64-v8a --release  :: 编译 Android 现代版
build.bat doctor --fix                               :: 环境体检与自动修复
```

不带参数运行 `build.bat` 可进入交互式菜单。

> `android_old/` 已停止开发且不再兼容：v1.1.2 起 `windows/` 与 `protocol/` 已移除低性能模式适配代码，旧 APK 无法连接本版 PC 端。旧设备请成对使用 [Release v1.1.1](https://github.com/Raven777777/UDP2CAL/releases/tag/v1.1.1)。

**开发**：Windows Rust 1.96+、VS Build Tools 2022、CMake 3.22+；Android JDK 17~22、Android SDK 35、NDK 27.0.12077973

---

## 文档

全部技术文档位于 [`docs/`](docs/README.md)。

| 文档 | 内容 |
|------|------|
| [docs/BUILD.md](docs/BUILD.md) | 环境要求、`build.bat` 全部命令、编译产物、常见问题 |
| [docs/FEATURES.md](docs/FEATURES.md) | 三端功能特性、Opus 参数详解、界面截图 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 系统架构、技术栈、自动协商机制、运行时行为、命名约定 |
| [docs/PROTOCOL.md](docs/PROTOCOL.md) | UDP v2 协议规范、P2P 独占生命周期、常量表 |
| [docs/ANDROID_OLD.md](docs/ANDROID_OLD.md) | 低性能版专项（已归档、不兼容）：发热优化实测数据与限制 |
| [docs/CHANGELOG.md](docs/CHANGELOG.md) | 版本历史 |
| [docs/BUGFIX.md](docs/BUGFIX.md) | Bug 根因分析与修复记录 |
| [docs/LEGAL.md](docs/LEGAL.md) | 版权声明、GPL-3.0 授权、第三方组件许可 |

---

## 许可证

© 2026 UDP2CAL Contributors. 本程序为自由软件，以 **GNU General Public License v3.0** 发布，详见 [`LICENSE`](LICENSE)；第三方开源组件许可见 [docs/LEGAL.md](docs/LEGAL.md)。
