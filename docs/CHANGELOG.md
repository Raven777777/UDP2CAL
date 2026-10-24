# 版本历史

> 按版本倒序排列。单个 Bug 的根因分析与修复细节见 [BUGFIX.md](BUGFIX.md)。

---

## 未发布 — 2026-10-24 · Opus 同源 ML 特性与反向链路修复

> 本轮围绕「Windows 端接入共享 opus-src 1.6.1」展开：启用 ML 特性（Deep PLC / DRED）、
> 重写丢包隐藏为事件驱动、修复反向串流自动码率无声。

**Opus 共享源码树（构建）**
- Windows 端经 `[patch.crates-io]` 接入仓库根 `opus-src/`（1.6.1），替代 audiopus_sys 内置的 1.3.1，与 Android 完全同源
- `vendor/audiopus_sys` 副本：build.rs 解析 `OPUS_SOURCE_DIR` → 默认回落 `../../opus-src`；CMake 强制 Release（dev profile 的 /MDd 调试 CRT 与 rustc /MD 不兼容，LNK2001）
- 开启 `OPUS_DEEP_PLC` + `OPUS_DRED`（OSCE / QEXT 保持关闭），两端产物各增约 2 MB
- `vendor/audiopus` 副本新增 `Decoder::set_complexity`（Opus 1.5 解码器接口）与 `Encoder::set_dred_duration`（请求码 4050，1.3.1 头文件所无）
- vendor 目录瘦身：仅保留 `Cargo.toml` + `LICENSE` + `src/**`，自写 README 记录本地修改

**丢包隐藏（PLC）——事件驱动**
- `ReorderBuffer::insert_and_drain` 回调新增 `lost` 参数：大跳变重同步时携带确切丢包数，解码端先精确补偿再解码真包（Windows 与 Android Kotlin 端同步改造）
- 计时器降级为整流停滞兜底（DTX/断流）：Windows 100ms / Android 80ms，远大于乱序窗口；修复计时器式 PLC 与乱序等待叠加导致的 `dropped` 持续增长（实测 ~145K/分钟）
- Windows 接收端补 PLC：此前丢包直接静音；Android 反向接收端新增 `decoderPlc` JNI + 40ms 轮询
- Deep PLC 运行时激活：Windows 全部 5 个解码器复杂度=5；Android 解码器创建时设 5
- DRED：编码端跟随 FEC 开关（开启时 500ms 冗余，码流 +12~32 kb/s）；两端同为 1.6.1，互操作无兼容问题

**反向串流自动码率无声（真机排查定版）**
- 自动码率（bitrate_kbps=0）在 PC 反向编码器上的映射由 `Bitrate::Max` 改为**固定 1024 kbps VBR**（1280 B/10ms 帧，1472 上限内留余量）
- 真机 A/B 实测：600/700/1024 正常，2048 与 Max 无声；Max 在离线环境无法复现（恒定 638B），疑似真机链路额外约束，工程上弃用
- 自动码率下手机端强制 VBR 并关闭 VBR 约束（编码器/动态同步/keepalive flags 三处 + UI 灰显）
- 新增 `UDP2CAL_REVERSE_AUTO_BR` 环境变量诊断覆盖口；反向链路 30s 诊断汇总（发包数/编码错误/超限丢弃/零长帧）
- 回归测试 `windows/tests/reverse_max_bitrate.rs`：Max/2048k/1024k/960k 编码与手机端等效解码

**docs**
- FEATURES：自动码率/VBR/FEC 条目更新；LEGAL：audiopus/audiopus_sys 许可证修正为 ISC 并补 vendor 说明

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
- 修正 `.gitignore` 中失效的"构建产物"规则

**构建 / 依赖**
- libopus 源码树从 `android/app/src/main/cpp/opus-src` 迁至仓库根 `opus-src/`，作为 Android 与 Windows 共用的唯一副本
- libopus 1.5.2 → 1.6.1：仅取 1.4–1.6 的缺陷修复与纯增量 API（新增 9 个 `*24` 接口），`OPUS_OSCE` / `OPUS_DRED` / QEXT 保持默认关闭
- Android 原生库体积 +3.7%（arm64-v8a / MinSizeRel：1 514 304 → 1 569 744 B）

---

## v1.1.2 — 2026-10-23 · 移除旧版 Android 客户端兼容链

不再保留旧版本适配：`windows/` 与 `protocol/` 中所有为旧版 Android 客户端（v1.1.1 及更早）服务的分支被彻底删除，v1.1.2 起的 PC 端只与 `android/` 互通。

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

> ⚠️ 破坏性变更：v1.1.1 及更早的 Android APK 无法连接本版 Windows 端。

## v1.1.0 — 2026-06-10 · 正式版发布

三端代码审查 + 许可证合规 + GPL-3.0 开源。

- GPL-3.0 许可证规范化（`LICENSE` 文件 + README 声明 + Android 关于页 GPL 合规文字）
- `build.rs` 硬编码 Windows Kits SDK 路径改为动态检测（`find_rc_path()`）
- 所有 Kotlin data class 标注 `@Suppress("ArrayInDataClass")`
- 全链路最终代码审查确认

## v1.1.0-hotfix3 — 2026-06-10 · 三端代码审查修复

- `android/`：WakeLock 无限期持有 + AEC 释放 + 模式切换时序修正
- `windows/capture.rs`：编码循环 Vec 复用消除高频分配
- Kotlin `Protocol.sampleRateToHz` 对齐 Rust 48000

## v1.1.0-hotfix2 — 2026-06-10 · 全链路低延迟

- 端到端延迟 500ms → 50~80ms
- Opus 正反向编码实时同步
- UI 重构（状态卡片 / 测试音 / 关于移入高级设置）
- 默认复杂度 10 + 自动码率；高刷新率屏幕适配

## v1.0.9.2 — 2026-06-08 · P2P 独占增强

- Win 端异设备 CONNECT 静默拒绝（不推送 StatusUpdate，消除码率/电平跳动）
- Android 端保活恢复持续发送

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
| 2026-10-23 | 移除旧版 Android 客户端兼容链（破坏性变更，v1.1.2） | 本文 v1.1.2 |
| 2026-06-10 | GPL 许可证合规 + 最终代码审查（4 项） | [BUGFIX.md](BUGFIX.md) |
| 2026-06-09 | 全链路低延迟优化（500ms → <100ms） | [BUGFIX.md](BUGFIX.md) |
| 2026-06-09 | 全面改名 UDP2Mic → UDP2CAL + 广播控制 | [BUGFIX.md](BUGFIX.md) |
| 2026-06-09 | 双向音频串流 + AEC 移植与修复 | [BUGFIX.md](BUGFIX.md) |
| 2026-06-07 | Android 端 UDP 乐观连接修复 | [BUGFIX.md](BUGFIX.md) |
