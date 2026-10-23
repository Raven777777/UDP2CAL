# UDP2CAL 文档中心

> 当前版本 **v1.1.2** · 本目录为项目全部技术文档的唯一入口

UDP2CAL 是一个双向、低延迟的局域网音频串流系统：Android 手机与 Windows PC 之间通过私有 UDP v2 协议互传 Opus 音频流。

> **`android_old/` 已停止开发**（自 v1.1.1 起），且自 **v1.1.2** 起 PC 端已删除低性能适配，与当前版本**不互通**；相关文档仅作归档。旧设备请成对使用 [Release v1.1.1](https://github.com/Raven777777/UDP2CAL/releases/tag/v1.1.1) 的 Windows 端与 APK。

---

## 文档索引

| 文档 | 内容 | 适用读者 |
|------|------|----------|
| [BUILD.md](BUILD.md) | 构建环境要求、`build.bat` 全部子命令、编译产物清单、ADB 调试与体检 | 首次构建者、发布者 |
| [FEATURES.md](FEATURES.md) | 三端功能特性、Opus 编码参数详解、截图 | 使用者、调参用户 |
| [ARCHITECTURE.md](ARCHITECTURE.md) | 系统架构、数据流、技术栈选型、依赖矩阵、自动协商机制、运行时行为、文件结构、命名约定 | 维护者、二次开发者 |
| [PROTOCOL.md](PROTOCOL.md) | UDP v2 私有协议规范：包头布局、消息类型、P2P 独占生命周期、常量表 | 协议实现者 |
| [ANDROID_OLD.md](ANDROID_OLD.md) | `android_old/` 低性能版专项说明 — **已停止开发、仅归档、与 v1.1.2+ PC 端不互通** | 旧设备用户、性能优化者 |
| [CHANGELOG.md](CHANGELOG.md) | 版本历史与重大变更 | 全部 |
| [BUGFIX.md](BUGFIX.md) | 已修复 Bug 的根因分析与修复方式（按时间倒序） | 全部 |
| [LEGAL.md](LEGAL.md) | 版权声明、GPL-3.0 授权、第三方开源组件许可 | 分发者、合规审查 |

---

## 建议阅读路径

- **只想用起来** → 根目录 [README](../README.md) 的「快速开始」→ [FEATURES.md](FEATURES.md)
- **要自己编译** → [BUILD.md](BUILD.md) → [ARCHITECTURE.md](ARCHITECTURE.md)
- **要改协议或加端** → [PROTOCOL.md](PROTOCOL.md) → [ARCHITECTURE.md](ARCHITECTURE.md) → [BUGFIX.md](BUGFIX.md)
- **要给旧手机适配** → [ANDROID_OLD.md](ANDROID_OLD.md)（已归档，仅作参考；v1.1.2 起 PC 端不兼容，需成对使用 [Release v1.1.1](https://github.com/Raven777777/UDP2CAL/releases/tag/v1.1.1)）

---

## 目录结构

```
docs/
├─ README.md          # 本文件（文档索引）
├─ BUILD.md           # 构建与产物
├─ FEATURES.md        # 功能特性与 Opus 参数
├─ ARCHITECTURE.md    # 系统架构与实现细节
├─ PROTOCOL.md        # UDP v2 协议规范
├─ ANDROID_OLD.md     # 低性能版专项
├─ CHANGELOG.md       # 版本历史
├─ BUGFIX.md          # Bug 修复记录
├─ LEGAL.md           # 许可证与合规
└─ assets/            # 文档截图（webp）
```
