# 版本历史

> 按版本倒序排列。单个 Bug 的根因分析与修复细节见 [BUGFIX.md](BUGFIX.md)。

---

## 未发布 — 第二轮质量审查修复（24 项）

> 详细根因与修复方式见 [BUGFIX.md](BUGFIX.md#2026-10-23--第二轮质量审查24-项)，完整报告见 [CODE_REVIEW_ROUND2_2026-10-23.md](archive/CODE_REVIEW_ROUND2_2026-10-23.md)。

**windows（`udp2cal`）**
- 反向捕获：解析端点混音采样率，非 48 kHz（如 44.1 kHz）在编码前重采样，修复音高偏差 8.8%
- 反向编码：仅在下发配置真正变化时才调用 `opus_encoder_ctl`，消除每秒一次的状态重置
- 托盘：创建失败不再 `panic`，降级为"关窗即退出"并提示
- 停止反向线程改为异步 `join`，不再阻塞 UI/执行器线程
- 新增落盘日志（`%LOCALAPPDATA%\UDP2CAL\udp2cal.log`，2 MiB 轮转），全部 `eprintln!` 迁移
- 新增音频诊断计数器并在界面/日志中展示（重采样 / 丢样 / 欠载）
- 发现服务加按源 IP 的令牌桶；设备 ID 生成混入 OS 熵
- 接收路径改用有界载荷缓冲池；`AudioInitState` 枚举化；提示级别枚举化
- 底部状态区改为**单行**并按状态切换：未启动=广播地址；已启动=广播地址+广播中；已连接=音频诊断+反向串流状态。去掉与顶部重复的"已连接手机端/已重新连接/已断开"提示，窗口保持 320×290（成品 322×322 正方形）
- 顶部 P2P 标识去掉设备识别码，仅显示 `● 已占用`
- 地址卡片两态统一锁定 32px 行高，消除启动/停止切换时 1.2px 的高度跳变（text_input 固有高度 30.8px）

**android（versionCode 6 / v1.1.2）**
- 修复停止采集时可能由 `ClosedSendChannelException` 导致的进程崩溃
- 修复断连后 `p2pConnected` 以 100 Hz 翻转：改为 ACK 时效判定
- 修复麦克风失效时 `AudioRecord.read` 的 100% CPU 紧循环
- 修复模式切换后正常停止时 `UdpSender`/ACK 线程泄漏
- 反向流新增重排与去重（`Udp2CalProtocol.ReorderBuffer`）
- 修复 `AudioPlayer` 队列溢出时立体声相位错位
- `UdpSender.send()` 去掉每帧协程调度；设备 ID 改用 `SecureRandom`
- 显式声明 `androidx.core:core-ktx`
- 新增协议层 JVM 单元测试 17 项（`Udp2CalProtocolTest`）

**protocol（`udp2cal-protocol`）**
- `generate_device_id()` 混入 OS 熵与进程号，不再可由时间戳推算
- 文档标注码率字段为保留字段、码率换算函数为规范参考实现

**仓库**
- `android_old/lanmic-debug.keystore`、`android_old/local.properties` 移出版本库
- 修正 `.gitignore` 中失效的"构建产物"规则

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
