# 版本历史

> 按版本倒序排列。单个 Bug 的根因分析与修复细节见 [BUGFIX.md](BUGFIX.md)。

---

## 未发布 — 2026-10-25 · 第三轮质量审查修复（22 项）

> 审查报告：[CODE_REVIEW_2026-10-25.md](../CODE_REVIEW_2026-10-25.md)。
> 本轮全部为缺陷修复，**无新特性**；协议行为有一处收紧（序列号语义），因此
> Android 与 Windows 仍需成对升级。

**媒体面时序（阻断级）**
- 序列号改为与时间戳**同频**推进：发送端每 10 ms 帧无条件 `seq+1`，不论该帧是否真的发出
  （ACK 超时暂停、DTX 空帧、编码失败均不再例外）。旧行为会让断流恢复包被判定为"迟到"并永久丢弃
- 接收端插入算法调整顺序：`ext_ts < next_ts`（真迟到）与重复判定提前，
  "序号落后但时间戳合法"改判为**断点**并按时间戳重新对齐（新增 `stats.seq_realign` 计数）
- 修复：ACK 中断 0.2–3 s 后正向音频永久静音、DTX 结束/长断流后双向静音
- 回归测试：`jb_realigns_when_seq_lags_behind_ts`、`jb_long_gap_across_seq_wrap_still_recovers`（Rust 与 Kotlin 镜像各一套）

**Windows 侧音频路径**
- 输出环 `clear()` 不再由生产者写 `tail`：改为投递 `flush_to` 请求、由音频回调落地，
  `len()` 增加快照校验。修复 flush 与 pop 竞争时 `len()` 回绕成 `usize::MAX`
  导致解码线程永久停产
- 播放头改为统计**声卡真实输出位置**（欠载补的静音也计入），不再永久落后欠载量
- 新增并发回归测试 `ring_len_never_wraps_under_flush_race`

**控制面与连接管理**
- 反向发送目标的触发条件纳入源 IP 变化（手机 Wi-Fi 重连换 IP 后反向不再静音）
- `update_reverse_config` 移到绑定校验之后：未绑定设备无法改写当前会话的反向编码参数
- 控制面保活检查改为周期执行，不再依赖 `recv_from` 超时（RTP 持续到达时也会判定超时）
- 「已连接」改为由控制面绑定状态驱动，码率归零不再与连接状态混淆

**Android**
- `AudioPlayer` 的 `write/flush/stop/播放头读取` 统一加锁，消除 AudioTrack 并发
  `IllegalStateException` 静默吞没
- 测试音生成在落后节拍时用 `yield()` 而非 `delay(0)` 忙等
- 麦克风权限被拒后弹窗指引（含"去设置"跳转与返回后复检）
- `UdpSender.connect()` 失败路径回收已创建的 socket

**工具与文档**
- 日志轮转改为运行期按字节计数触发（不再只在启动时检查一次）
- `doctor --fix` 后自动复检，摘要不再恒定显示"待处理"
- `PROTOCOL_V3_RTP.md` 4.4 / 7.4 / 7.9 / 8.1 同步修订，删除抖动缓冲侧冗余的 `timestamp_offset`

---

## v1.2.0 — 2026-10-24 · 协议 v3：RTP/Opus 媒体面 + 播放头时钟

> 一次干净断代的重写：v2 客户端无法连接 v3 客户端，两端必须同时升级。
> 规范见 [PROTOCOL_V3_RTP.md](PROTOCOL_V3_RTP.md)。

**媒体面换成标准 RTP**
- 12 字节 RTP 固定头（V=2/P=0/X=0/CC=0，PT=111，48 kHz），负载就是 Opus 包本身
- 固定 10 ms 帧（480 采样）；负载上限 1275 字节，RTP 包上限 1287 字节，UDP/IP 上限 1315 字节
- 序列号 16 位 + 时间戳 32 位，均用有符号差值扩展到 64 位；回绕不清空缓冲
- `M=1` 语义：会话首包 / DTX 结束 / 时间戳不连续后的首包；只有「M=1 且跳变 >200 ms」才重同步

**控制面 v3**
- 15 字节控制头首字节 `0x03`；删除 `is_audio / codec / sample_rate / bitrate / seq_num` 位域
- `CONNECT` 固定 15 字节（反向端口、正向 SSRC、PT、帧长、Opus 参数、反向码率、丢包率）
- `CONNECT_ACK` 固定 6 字节（反向 SSRC、PT、帧长）
- `DISCOVER_REPLY` 携带 `listen_port`（不再假定 44044）与 ≤254 字节设备名
- 控制包与正向 RTP 共用一个 socket，按首字节解复用；`TYPE_DATA` 废弃，收到即丢

**接收端时钟模型**
- 声卡播放头是唯一音频时钟：`playhead_ts = base + played_device_frames + timestamp_offset`
- 抖动缓冲只在播放头到期时生成 PLC，连续 12 帧后输出静音但保持 PLAYING
- 漂移校正改为水位驱动（>120 ms 丢 5 ms / <20 ms 插 5 ms，间隔 500 ms 音频时间），
  删除 3 秒 EMA、`recv_timeout(30ms)`、60 ms PLC 计时、`Instant` 重排衰减
- Windows 解码线程预解码深度钉在 ~40 ms，由 cpal 回调 `unpark` 唤醒

**发送端**
- Android 采集协程直接 `read → encode → RTP → send`，删除 `Channel<ShortArray>(3)`、`ShortArrayPool`、双协程
- Windows 反向发送改为 WASAPI 事件驱动 + 10 ms 组帧，删除 2 ms 轮询与 3 秒 `pcm_buf` 上限
- 编码失败/发送失败：序号仍 +1（让对端看到缺口），时间戳照常 +480；DTX 空帧不占序号

**包长保护**
- 编码缓冲直接取 1275 字节，超限整帧丢弃、计数，绝不截断
- 手动码率档位改为 512 / 768 / 960 / 1000 kbps，1024 kbps 被禁止（1280 字节会越界）
- 新增回归测试：`windows/tests/reverse_max_bitrate.rs`（禁止档位也不得产出 >1275 字节）

**文档**
- 新增 `docs/PROTOCOL_V3_RTP.md`（现行规范）；`docs/PROTOCOL.md` 降级为 v2 历史存档
- `docs/ARCHITECTURE.md` 重写管线、线程、时钟模型与漂移校正章节

**联机实测修复（2026-10-24 19:00，真机"卡塔"）**
- 反向发送：静音补齐改为"端点空闲 >200 ms + 10 ms 墙钟节拍"双重约束。原实现在事件等待
  超时时无条件补帧，与事件带回的 20 ms 数据叠加，实测 3092 包/30 s（103 pps，超实时 3%），
  接收端缓冲十几秒即被灌满
- 抖动缓冲：超出重排窗口（`next_seq+64`）的包改为**只丢弃该包**，不再整块重同步。
  原逻辑会把已缓冲的 64 帧（640 ms）一起丢掉，听感即"卡塔"；现在只会在播放头走到时
  产生一次 10 ms PLC
- 诊断 I/O 移出实时线程：`logf!` 是同步写盘，实测每 30 s 打断一次反向 RTP（详见"30 s 破音"）
- 反向发送线程的配置检查由每帧改为 500 ms 一次，实时路径不再有锁流量
- 两端新增重同步原因日志（`反向重同步：<原因> | nextTs=… 水位=…`）与 `发送中断` 计数
- **采集侧整帧累积**：`AudioRecord.read` 允许返回不足 480 采样，原实现把余量补零后当整帧发送
  → 等于丢音频 + 帧率翻倍（实测 190 包/秒，PC 端 5730 包里 2374 包被判超窗丢弃）。改为累积到
  整 480 采样再编码
- 首包只建立基准，不再推进 `epoch`（避免上层白重建一次解码器、也不再产生"首个包"伪重同步日志）
- 漂移校正：高水位按绝对值**限速排空**（>300 ms 每 500 ms 削 5 ms；200–300 ms 每 2 s 削 5 ms；
  <200 ms 不动），低水位仍只按趋势补偿


## 未发布 — 2026-10-24 · 抖动与省电修复（波形中断问题）

> 真机现象：双向音频周期性出现 10~50ms 中断，切 app 加重。日志定位为
> Wi-Fi 省电聚合（停顿→突发交替，`丢样` 最高 26K/s，但也有 60s 完全干净的窗口）。

**Wi-Fi 省电锁（源头）**
- Android `CaptureService` 采集期间持有 `WIFI_MODE_FULL_LOW_LATENCY` WifiLock，停止时释放（此前全 App 无任何 WifiLock）

**双重补偿去重（放大器）**
- 停滞兜底计时器补过的帧记入 `pendingConcealed`，事件驱动 `lost` 先扣除再补差额——同一段丢失不再补两遍（修复超产 → 环形缓冲溢出 → 丢样暴增）
- Windows 计时器阈值 100→60ms（去重后可安全下调，补偿恢复更快）

**自适应重排窗口（放大器）**
- `ReorderBuffer` 窗口 2 → 2~16 动态调整（NetEQ 思路）：按乱序/迟到深度 EMA 扩窗，把"迟到"变"等待"；干净网络收敛回 2（低延迟优先）；无事件 5s 估计减半
- Rust 与 Kotlin 双端同步实现；新增自适应行为单测

**docs**
- BUILD.md PLC 表新增自适应窗口与 WifiLock 条目

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
