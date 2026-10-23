# 版本历史

> 按版本倒序排列。单个 Bug 的根因分析与修复细节见 [BUGFIX.md](BUGFIX.md)。

---

## v1.1.2 — 2026-10-23 · 移除 android_old 兼容链

不再保留旧版本适配：`windows/` 与 `protocol/` 中所有为 `android_old/` 服务的分支被彻底删除，v1.1.2 起的 PC 端只与 `android/` 现代版互通。

**protocol（`udp2cal-protocol` 1.1.2）**
- 删除版本探测函数 `is_v2_packet()`，包头首字节严格等于 `PROTO_VERSION`，不符合即丢弃
- 移除"旧 v1（首字节 `0x80+`）拒绝"说明与 `test_reject_v1` 测试

**windows（`udp2cal` 1.1.2）**
- `main.rs`：删除 CONNECT 载荷 `bit0=low_perf` 解析；反向发送器不再接收低性能标志
- `main.rs`：P2P 状态移除 v1 音频流兼容分支，音频仅在 `DEVICE_BUSY` 且 device_id 匹配时被接受
- `capture.rs`：删除低性能反向分支（16kHz 单声道 / 64kbps / complexity=1 / 3:1 降采样），反向串流固定 48kHz 立体声全频带
- `capture.rs`：删除仅低性能路径使用的 `FRAME_SIZE_16K`、`CODEC_MONO`、`CODEC_STEREO` 与单声道下混缓冲

**android（`1.1.2`，versionCode 6）**
- CONNECT 标志位由 `0x03` 改为 `0x02`，彻底移除 `bit0=low_perf` 语义（bit0 保留固定 0）
- 关于页版本号同步为 v1.1.2

> ⚠️ 破坏性变更：v1.1.1 及更早的 `android_old` APK 无法连接本版 Windows 端；旧设备请成对使用 [Release v1.1.1](https://github.com/Raven777777/UDP2CAL/releases/tag/v1.1.1)。

## v1.1.0 — 2026-06-10 · 正式版发布

三端代码审查 + 许可证合规 + GPL-3.0 开源。

- GPL-3.0 许可证规范化（`LICENSE` 文件 + README 声明 + Android 关于页 GPL 合规文字）
- `build.rs` 硬编码 Windows Kits SDK 路径改为动态检测（`find_rc_path()`）
- 所有 Kotlin data class 标注 `@Suppress("ArrayInDataClass")`
- 全链路最终代码审查确认

## v1.1.0-hotfix3 — 2026-06-10 · 三端代码审查修复

- `android/`：WakeLock 无限期持有 + AEC 释放 + 模式切换时序修正
- `android_old/`：反向解码器立体声支持（修复高品质模式反向无声）+ 下混至单声道
- `windows/capture.rs`：编码循环 Vec 复用消除高频分配
- Kotlin `Protocol.sampleRateToHz` 对齐 Rust 48000

## v1.1.0-hotfix2 — 2026-06-10 · 全链路低延迟

- 端到端延迟 500ms → 50~80ms
- Opus 正反向编码实时同步
- UI 重构（状态卡片 / 测试音 / 关于移入高级设置）
- 默认复杂度 10 + 自动码率；高刷新率屏幕适配
- `android_old` 低延迟适配（AudioTrack 300→80ms）

## v1.1.0-hotfix — 2026-06-09 · android_old 发热优化

- JNI 动态帧长修复 40ms 帧降调
- 48kHz FB + 256kbps 最终定型（CPU 120%→14%，测试设备 Sharp NP805SH / 骁龙210 / 1GB）
- ACK 限频轮询（`soTimeout` 1→5ms，每 10 帧）
- WakeLock 无限期持有

## v1.0.9.2 — 2026-06-08 · P2P 独占增强

- Win 端异设备 CONNECT 静默拒绝（不推送 StatusUpdate，消除码率/电平跳动）
- Android 端保活恢复持续发送
- 新增 `android_old/` 适配 Android 6~8 旧设备（无 Compose，低性能默认配置 + VOICE_COMMUNICATION 回退 + 一键自动连接）
- `build_android.bat` 支持 4 选项同时编译新旧版本

## v1.0.6 — UI 与稳定性重构

- UI 卡片布局 + 动态 VU 色彩表 + 按钮交互反馈
- 全局单例音频守护线程根除反复启停泄漏
- `udp_receiver_stream` 参数化消除跨线程注册表竞态
- EMA NaN/Infinity 防线
- 200ms 防抖 + 实时端口校验

## v1.0.5 — Windows 接收端初始架构

- Windows 接收端初始架构
- 局域网广播发现服务

---

## 相关变更记录

| 日期 | 主题 | 文档 |
|------|------|------|
| 2026-10-23 | 移除 android_old 兼容链（破坏性变更，v1.1.2） | 本文 v1.1.2 |
| 2026-06-10 | GPL 许可证合规 + 最终代码审查（4 项） | [BUGFIX.md](BUGFIX.md) |
| 2026-06-09 | 全链路低延迟优化（500ms → <100ms） | [BUGFIX.md](BUGFIX.md) |
| 2026-06-09 | android_old 发热优化（CPU 120% → 14%） | [BUGFIX.md](BUGFIX.md) |
| 2026-06-09 | 全面改名 UDP2Mic → UDP2CAL + 广播控制 | [BUGFIX.md](BUGFIX.md) |
| 2026-06-09 | 双向音频串流 + AEC 移植与修复 | [BUGFIX.md](BUGFIX.md) |
| 2026-06-07 | Android 端 UDP 乐观连接修复 | [BUGFIX.md](BUGFIX.md) |
