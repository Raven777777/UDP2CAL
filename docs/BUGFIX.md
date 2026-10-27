# UDP2CAL Bug 修复记录

> 只记录**现行代码**（协议 v3）的 bug 根因与修复方式，按时间倒序。
> v3 重写之前的记录已归档到 [archive/BUGFIX_V2_ERA.md](archive/BUGFIX_V2_ERA.md)：
> 那些条目涉及的模块（`Channel`/`ReorderBuffer`/`ShortArrayPool`/v2 音频头）
> 在当前代码里已不存在，留在本文只会误导。
>
> 相关文档：[版本历史](CHANGELOG.md) · [架构说明](ARCHITECTURE.md) · [文档索引](README.md)

---

## 2026-10-27 — 控制面会话绑定与反向链路自愈

| # | 模块 | 问题 | 根因 | 修复 |
|---|------|------|------|------|
| 1 | 三端 | 旧会话 ACK 可刷新连接、覆盖反向 SSRC | ACK 只带 reverse_ssrc/PT/frame，不绑定 forward_ssrc | ACK 扩为 10 字节并回显 forward_ssrc；Android 只接受当前会话 |
| 2 | Windows | 单次控制包丢失时 Windows 先超时拆链 | ACK 窗口与绑定超时同为 3 s，无重传余量 | `PEER_TIMEOUT_MS=4500`，比 ACK 窗口长 1.5 s |
| 3 | Windows | 停止播放后恢复，手机永久无声 | WASAPI 端点失效/服务重启后旧采集客户端只返回错误，线程把错误当空闲，永久发合成静音；发送线程死亡后槽位仍为 `Some` | `GetBuffer` 错误区分并连续 3 次重建客户端；`ReverseSender` 存活标志 + 保活自动重启；`stop` 同步 join |
| 4 | Android | 播放协程退出后无人重启 | 只有 `activeReverseSsrc` 变化才启动播放；协程异常/写入连续失败会永久退出 | 控制面监督 `reversePlayJob`，2 s 退避后重启；切换会话时 `cancelAndJoin` 旧循环 |
| 5 | Android | ACK 新鲜度受系统墙钟跳变影响；换 SSRC 与 ACK 提交有竞态 | `System.currentTimeMillis` + 无会话锁 | 改用 `elapsedRealtime`，ACK 提交与 `regenerateForwardSsrc` 同锁 |

> 未修复（设计限制）：首次 CONNECT 仍只凭公开 `device_id` 绑定，未认证的局域网主机
> 可以在 Ready 状态抢先绑定并指定 `reverse_port`，从而接收 Windows 环回声。要彻底修复
> 需要配对码/确认对话框等认证机制，超出本次小修范围。

---

## 2026-10-27 — 反向串流生命周期与版本号

| # | 模块 | 问题 | 根因 | 修复 |
|---|------|------|------|------|
| 1 | Android | 首次启动连接后反向串流没声音，重连一次才正常 | `reversePlayLoop` 等 `audioPlayer/reverseDecoder` 最多 3 s；冷启动时麦克风/AudioTrack 初始化可能更久，超时后协程永久退出，而 `activeReverseSsrc` 没变，控制面不会再次启动它 | 去掉固定超时，等待到播放器/解码器就绪或协程取消 |
| 2 | Windows/Android | Windows 停止播放音频后，手机反向串流断开，需要重连 | 端点空闲 2 s 后反向发送器停止补静音 → 接收端欠载/重同步；Android 长时间欠载后 `AudioTrack.write` 失败会让播放协程直接 `return`，此后无人重启 | Windows 空闲时持续按 10 ms 节拍发静音；Android `AudioTrack` 失效时原地重建并重试写入 |
| 3 | Android | 高级设置底部版本号一直显示 `v1.2.0` | 文本硬编码 | 改为读取 `PackageInfo.versionName`（`build.gradle.kts` 的 `versionName`） |

### 验证

- `protocol`：`cargo test` 74 passed。
- `windows`：`cargo check` 通过。
- `android`：`:app:testDebugUnitTest` 63 passed（JDK 17）。

---

## 2026-10-25 — 第三轮质量审查（22 项）

### 概述

对 `windows/`、`protocol/`、`android/`、`scripts/` 的全量审查定位 22 项问题
（P0×2、P1×2、P2×6、P3×12），本轮全部处理。审查报告见
[CODE_REVIEW_2026-10-25.md](archive/CODE_REVIEW_2026-10-25.md)。

其中 4 项用独立验证工程（真实 `udp2cal-protocol` 实现 + 逐行等价复刻的解码循环与无锁环）
复现后修复，其余为静态确认。

### P0（阻断级：单向永久静音）

| # | 模块 | 问题 | 根因 | 修复 |
|---|------|------|------|------|
| 1 | 三端 | ACK 中断 0.2–3 s 后正向音频永久静音 | `CaptureService.sendFrames` 在 `!isAckFresh` 时同时跳过 `seq`，但 `timestamp` 无条件推进；接收端断流期间用 PLC/静音推进 `next_seq`，恢复包 `ext_seq` 永久落后 → 被"迟到"判定丢弃，且该判定位于 M=1 重同步之前，重同步分支不可达 | 序列号与时间戳同频推进（每帧无条件 +1）；接收端把"序号落后但时间戳合法"改判为断点并重新对齐 |
| 2 | 三端 | DTX 结束 / 长断流后双向静音 | 规范 4.4 要求"DTX 不递增序列号"，与接收端的迟到判据不自洽：恢复包 `gap=0` 不满足重同步条件，随后被序号判定丢弃。Windows 反向链路严格按规范实现，开 DTX 即中招；Android 正向"错误地"递增序列号恰好掩盖了该缺陷 | 同 #1；同步修订规范 4.4（序列号每帧递增）与 7.4（判定顺序） |

### P1（严重）

| # | 模块 | 问题 | 根因 | 修复 |
|---|------|------|------|------|
| 3 | Windows | 解码线程可能永久停产（静音、无日志） | `Ring::clear()` 由生产者写消费者索引 `tail`，破坏 SPSC 不变量；`len()` 的 head/tail 两次读取之间若插入 flush，相减回绕成 `usize::MAX` → `filled_frames() >= max_depth` 恒真 | 生产者改为投递 `flush_to` 请求、由音频回调落地；`len()` 增加"读 tail→读 head→再读 tail"快照校验 + `saturating_sub` 兜底；新增并发回归测试 |
| 4 | Windows | 手机换 IP 后反向音频静音 | `reverse_stale` 未纳入源 IP，只有 `DeviceID/SSRC/reverse_port` 变化才重启反向发送器 | 增加 `peer.addr.ip() != src.ip()` 条件 |

### P2（中等）

| # | 模块 | 问题 | 修复 |
|---|------|------|------|
| 5 | Windows | 未绑定设备可改写当前会话的反向编码参数 | `update_reverse_config` 移到绑定关系校验之后 |
| 6 | Windows | RTP 持续到达时控制面保活检查永不执行 | 改为每 100 ms 周期执行，与流量解耦 |
| 7 | Windows | 输出欠载不计入播放头，延迟单次累积、PLC 触发点推迟 | `played` 改为统计声卡真实输出位置（含欠载静音） |
| 8 | 三端 | 断流跨度 >32767 包（>5.4 min）后无法恢复 | 同 #1 的重新对齐逻辑（时间戳由 32 位扩展，跨度上限约 12 h） |
| 9 | 三端 | 抖动缓冲 `timestamp_offset` 只写不读，规范 7.9 形同空转 | 删除冗余字段与 `note_pcm_correction`；偏移只由 `PlayheadClock` 承担，同步修订规范 |
| 10 | Android | `flush()` 与 `writeBlocking()` 并发访问 AudioTrack，异常被静默吞掉 | `AudioPlayer` 增加 `trackLock`，`write/flush/stop/播放头` 全部串行化（锁序 linkLock → trackLock） |

### P3（轻微 / 维护性）

| # | 模块 | 问题 | 修复 |
|---|------|------|------|
| 11 | 协议 | `build_control` 对 >512 字节负载静默截断，与 `build_control_to` 语义不一致 | 增加 `debug_assert` + 文档说明（保留 release 下的兜底截断，避免 panic） |
| 12 | Android | 测试音落后节拍时 `delay(0)` 忙等 | 改为 `yield()` |
| 13 | Android | `sendFrames` 注释与实现（DTX 时递增、失败时不递增）三方矛盾 | 统一为"每帧递增"，重写注释 |
| 14 | Android | `JitterBuffer.reset()` 未清 `lastRebaseReason`，新会话日志打印旧原因 | `reset()` 中清空 |
| 15 | Android | 麦克风权限被拒无任何提示 | 新增说明弹窗 + "去设置"跳转 + `onResume` 复检 |
| 16 | Windows | 音频初始化重试后 `wake_decoder()` 唤醒已退出线程 | `DECODE_THREAD` 由 `OnceLock` 改为可替换的 `Mutex<Option<Thread>>` |
| 17 | Android | `UdpSender.connect()` 失败路径不回收 socket | 失败分支关闭已创建的 socket 并清空字段 |
| 18 | Windows | 日志 2 MiB 上限只在启动时检查 | 运行期按已写字节计数触发轮转，失败时延后重试 |
| 19 | 构建 | `doctor --fix` 后不重算问题数，摘要恒为"待处理" | 抽出 `_check_projects()`，修复后复检 |
| 20 | Windows | `StatusUpdate` 只置真不置假，码率 0 仍显示"已连接" | 由控制面绑定状态驱动 |
| 21 | Windows | 报告曾判定"重采样相位为负导致外插" | **复核为误报**：相位恒在 `[0, ratio)`，不存在负相位。已在代码中补充推导注释，不做改动 |
| 22 | 协议 | `DRIFT_MIN_SAMPLES` 为未使用常量 | 删除 |

### 验证

- `protocol/`：`cargo test` 43 项通过（新增 2 项断流自愈回归，删除 1 项失效的 `timestamp_offset` 断言）
- `windows/`：`cargo test` 14 项单测 + 2 项集成测试通过（新增 3 项无锁环/flush 回归）
- 独立验证工程复跑：ACK 中断 1 s 后恢复从"30 包全丢"变为"0 丢包、28/30 帧出音"；
  DTX 500 ms 恢复从"恢复包被丢弃"变为 `Resync`
- Android：本机仅有 JDK 25/27（AGP 要求 17–22），`gradlew` 无法执行；
  Kotlin 侧改动经静态复核，测试用例已补齐，需在 JDK 17–22 环境执行
  `./gradlew :app:testDebugUnitTest` 确认

---

