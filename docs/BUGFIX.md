# UDP2CAL Bug 修复记录

> 记录项目中已修复的 bug 及其根因分析、修复方式。
> 格式：`YYYY-MM-DD` — 按时间倒序排列。
>
> 相关文档：[版本历史](CHANGELOG.md) · [架构说明](ARCHITECTURE.md) · [文档索引](README.md)

---

## 2026-10-25 — 第三轮质量审查（22 项）

### 概述

对 `windows/`、`protocol/`、`android/`、`scripts/` 的全量审查定位 22 项问题
（P0×2、P1×2、P2×6、P3×12），本轮全部处理。审查报告见
[CODE_REVIEW_2026-10-25.md](../CODE_REVIEW_2026-10-25.md)。

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

## 2026-10-23 — 第二轮质量审查（24 项）

### 概述

对 `windows/` 与 `android/` 的第二轮全量审查共定位 24 项问题（4 项 P1、12 项 P2、8 项 P3），本轮全部修复。审查报告见 [CODE_REVIEW_ROUND2_2026-10-23.md](archive/CODE_REVIEW_ROUND2_2026-10-23.md)。

### P1（用户可感知 / 崩溃）

| # | 模块 | 问题 | 根因 | 修复 |
|---|------|------|------|------|
| 1 | Android | `stopCapture()` 可能触发进程崩溃 | 先 `channel.close()` 再 `captureJob.cancel()`；`close()` 让阻塞在 `send` 的生产者抛 `ClosedSendChannelException`（非 CancellationException），未捕获 → 顶层 `CoroutineExceptionHandler` → 主线程崩溃 | 调换为"先 cancel 再 close"；生产者 `send` 加 try/catch 兜底 |
| 2 | Android | 断连后状态以 100 Hz 翻转 | `UdpSender.hasAck()` 只判断 `lastAckTime > 0` 且永不复位；超时置 `p2pConnected=false` 后下一帧立刻被置回 true | 新增 `isAckFresh(now, 3000ms)` 时效判定，替换全部历史判定 |
| 3 | Windows | 44.1 kHz 端点反向音频升调 8.8% | `parse_mix_format` 只解析声道/位深，未读 `nSamplesPerSec`，编码器硬编码 48 kHz | `CaptureFormat` 增加 `sample_rate`；新增 `StereoResampler`（线性插值 + 跨缓冲相位连续）归一到 48 kHz |
| 4 | Windows | 托盘创建失败导致进程静默退出 | `install_tray()` 内 `expect()`；explorer 未就绪（开机自启/远程会话）时 panic，且 GUI 子系统无控制台 | 返回 `Option<TrayIcon>`，失败降级为"关窗即退出"并写入日志/提示 |

### P2（健壮性 / 资源 / 可观测性）

| # | 模块 | 问题 | 修复 |
|---|------|------|------|
| 5 | Android | `AudioRecord.read <= 0` 无退避 → 麦克风被抢占时 100% CPU 紧循环 | 区分错误码：`ERROR_DEAD_OBJECT` 或连续 100 次错误则停止采集；其余 `delay(5)` 退避 |
| 6 | Android | 模式切换后 `isModeRestart` 残留，正常停止时跳过 `udpSender.close()` → socket/ACK 线程泄漏 | `stopCapture()` 中显式复位 `isModeRestart` |
| 7 | Windows | UI/异步路径同步 `join` 反向线程 → 界面卡顿 | `stop_and_join()` 改为 `stop()`：置标志后把 join 交给后台线程 |
| 8 | Windows | 每秒无条件重下发 9 个 `opus_encoder_ctl` | 缓存 `applied: Option<OpusReverseConfig>`，仅在变化时下发 |
| 9 | Windows | 底部提示与顶部状态点重复（"已连接手机端/已重新连接/已断开"闪烁），且状态行过高被方形窗口裁切 | 底部改为**单行**并按状态切换（未启动=广播地址／已启动=广播地址+广播中／已连接=音频诊断+反向串流）；提示行仅保留错误；顶部"已占用"去掉设备识别码；窗口保持 320×290（成品 322×322） |
| 9b | Windows | 用 `bitrate=0` 的 StatusUpdate 充当重绘信号，导致码率显示每秒闪一次 0 | 新增 `Message::PeerStateChanged` 专用重绘消息，不再污染遥测数据 |
| 9c | Windows | 地址卡片在启动后变高 1.2px（把下方整列推开） | 运行态分支 `.height(32)` 与 input 固有高度（16×1.3+10=30.8px）不一致；未启动分支的 row 也加 `.height(32)`，两态锁死 |
| 10 | Windows | 发布版无任何落盘日志 | 新增 `logger` 模块：`%LOCALAPPDATA%\UDP2CAL\udp2cal.log`，2 MiB 轮转；全部 `eprintln!` 改为 `logf!` |
| 11 | Android | 反向流无序号处理，乱序包解码成噪音 | 新增 `Udp2CalProtocol.ReorderBuffer`（与 Rust 端同语义：首包基准 / 迟到窗口 8 / 重排窗口 2 / 大步跳变重同步） |
| 12 | Windows | 设备 ID 可预测且无鉴权 | `generate_device_id()` 混入 `RandomState`（OS 熵）与进程号/序号；在 ARCHITECTURE.md 补充威胁模型章节 |
| 13 | Windows | 发现应答可被用于 UDP 反射放大 | 发现线程加按源 IP 的令牌桶（5 次/秒，30s 清理） |
| 14 | 仓库 | 历史遗留的 `lanmic-debug.keystore` 与 `local.properties` 仍在版本库 | `git rm --cached` 移出索引（`.gitignore` 已覆盖） |
| 15 | Android | `androidx.core` 未显式声明，仅靠传递依赖 | 显式 `implementation("androidx.core:core-ktx:1.16.0")` |
| 16 | Android | `hasNewCapture` / `Prefs.prefs` 跨线程无可见性保证 | 加 `@Volatile` |

### P3（可维护性 / 死代码 / 一致性）

| # | 模块 | 问题 | 修复 |
|---|------|------|------|
| 17 | Windows | `AudioWriter` 6 个计数器只写不读 | 收敛为 `AudioStats`（原子计数器），UI 底部显示"重采样/丢样/欠载"，每 30s 落盘 |
| 18 | Android/Windows | 死代码：`isV2Packet`、`buildAudioPacketTo`、`isPlaying`、`connectedDeviceId`、`OpusEncoder.encode`、`encodeCount` | 全部删除（含 JNI `encoderEncode`） |
| 19 | protocol | 码率字段两端都不消费，helper 仅测试使用 | 文档标注为"保留字段"，helper 标注为"规范参考实现" |
| 20 | Windows | `notice_is_error()` 靠中文子串判定配色 | 改为 `NoticeLevel::{Info, Error}` 显式声明 |
| 21 | Windows | `AUDIO_INIT_STATE` 裸 `u32` 魔数 | 改为 `enum AudioInitState`（`AtomicU8`） |
| 22 | Windows | 接收路径每包 `to_vec()` | 新增有界载荷缓冲池（64 个），音频线程解码后归还 |
| 23 | Android | 每帧 `withContext(Dispatchers.IO)` 调度抖动 | `UdpSender.send()` 改为普通函数，由消费者协程直接调用 |
| 24 | 仓库 | `.gitignore` 第 9 行 `构建产物` 缺 `#` | 修正为注释 |
| 附 | Android | 协议层无任何自动化测试 | 新增 `app/src/test/.../Udp2CalProtocolTest.kt`（17 项：编解码往返 / 非法包拒绝 / 跨端常量 / 重排时序）+ `testImplementation("junit:junit:4.13.2")` |

### 附带修复（审查中额外发现）

| 模块 | 问题 | 修复 |
|------|------|------|
| Android `AudioPlayer.ShortQueue` | 队满时只淘汰 1 个样本，立体声交错相位永久错位（声道互换） | 成对淘汰；`trimTo()` 向下取偶 |
| Android `AudioPlayer` | `isPlaying()` 无人调用 | 删除 |
| 文档 | `opt-level`、保活帧数、已删除的全局变量、`drainAck` 等 4 处漂移 | 同步 ARCHITECTURE.md / FEATURES.md |

---

## 2026-06-10 — 正式版发布：GPL 许可证合规 + 最终代码审查

### 概述

v1.1.0 正式版发布前进行最终代码审查，发现并修复 4 个问题：

| # | 模块 | 问题 | 根因 | 修复 |
|---|------|------|------|------|
| 1 | `android/` | **GPL v3 许可证与 About 页文字矛盾** | About 页显示「未经授权 禁止转载 修改 二次发布」，违反 GPL v3 允许复制/分发/修改的规定 | 统一改为「本程序为自由软件，以 GPL v3 许可证发布」|
| 2 | `windows/build.rs` | **SDK 路径硬编码** | `r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64"` | 改为 `find_rc_path()` 函数：优先环境变量 `WindowsSdkDir`+`WindowsSDKVersion`，回退扫描默认安装目录取最新版本 |
| 3 | `android/` | **Kotlin data class 的 ByteArray 字段** | `PacketHeader` 和 `DiscoverResult` 的 `ByteArray` 字段导致自动生成的 `equals()` 使用引用比较 | 标注 `@Suppress("ArrayInDataClass")` |
| 4 | `LICENSE`, `README.md` | **缺少开源许可证文件** | 项目无正式许可证 | 添加 GPL-3.0 `LICENSE` 文件，README 顶部插入许可证声明 |

### 概述

对 android、windows（Rust 接收端）、protocol 三组代码进行审查，发现并修复 5 个 Bug / 隐患。

### 审查范围

| 代码库 | 文件数 | 涉及语言 |
|--------|:------:|----------|
| `android/` | 12 | Kotlin + C (JNI) |
| `windows/` | 6 | Rust |
| `protocol/` | 1 | Rust, Kotlin (双端) |

### 修复清单

| # | 代码库 | 问题 | 根因 | 修复 |
|---|--------|------|------|------|
| 1 | `android/` | WakeLock 10 分钟超时 | `wakeLock.acquire(10 * 60 * 1000L)` 10分钟后自动释放，手机熄屏断连 | 改为 `wakeLock.acquire()` 无限期持有，finally 释放 |
| 2 | `android/` | AEC 对象泄漏 | `AcousticEchoCanceler.create()` 创建后仅设置 `enabled=true`，句柄丢弃无法释放 | 保存引用为成员变量 `aecInstance`，finally 中 `aecInstance?.release()` |
| 3 | `android/` | 模式切换 UdpSender 重建 | `isModeRestart = false` 在 finally 中早于 `pendingRestart` 执行，新协程读不到标志 | `CaptureParams` 增加 `isModeRestart` 字段，finally 中 `isModeRestart = restart?.isModeRestart ?: false` |
| 4 | `windows/` | 编码循环 Vec 高频分配 | 低性能/高品质分支每次 10ms 循环都 `Vec::with_capacity(15+plen)`，每秒数百次堆分配 | 预声明 `send_packet` 并 `clear() + extend_from_slice()` 复用 |
| 5 | `protocol/`（Kotlin）| sampleRateToHz 默认值与 Rust 不一致 | Kotlin 版 `else -> 16000`，Rust 版 `else -> 48000`，采样率推断跨平台不一致 | Kotlin 版改为 `else -> 48000` 对齐 Rust |

### 影响范围

| 问题 | 触发条件 | 严重度 |
|:----|---------|:------:|
| WakeLock 超时 | 采集运行 >10 分钟，手机熄屏 | **高** — 断连静音 |
| AEC 泄漏 | 每次启动/停止采集 | 中 — 累积内存占用 |
| 模式切换重建 | 语音↔音乐切换 | 中 — 短暂网络中断~50ms |
| Vec 高频分配 | 反向串流运行 | 低 — 微性能损耗 |
| sampleRateToHz 不一致 | 未知采样率 | 低 — 仅边界 case |

## 2026-06-09 — 全链路低延迟优化（500ms → <100ms 局域网端到端）

### 概述

端到端延迟约 500ms，经分析为多个缓冲叠加：20ms 帧长 + 160ms 抖动缓冲 + 200ms 初始音频缓冲 + 系统缓冲。实施多项低延迟优化。

### 修复清单

| # | 问题 | 根因 | 修复 |
|---|------|------|------|
| 1 | ReorderBuffer 抖动缓冲 160ms | `MAX_REORDER=8` × 20ms 帧，8帧缓冲区 | `MAX_REORDER=2`（最大抖动缓冲 40ms）|
| 2 | AudioWriter 初始缓冲 200ms | `initial_fill = actual_rate / 5` | `initial_fill = actual_rate / 25`（40ms）|
| 3 | AudioWriter 最大缓冲 1s | 缓冲上限 48000 样本 | 上限 12000 样本（250ms）|
| 4 | AudioMessage 通道过大 | `SyncChannel(200)` | `SyncChannel(50)` |
| 5 | 正向编码帧长 20ms | `opus_jni.c` 硬编码 20ms 帧 | 改为 10ms 帧（`(sampleRate * 10) / 1000`）|
| 6 | 反向编码帧长 20ms | `capture.rs` FRAME_SIZE=960（20ms × 48kHz） | 改为 10ms（FRAME_SIZE=480，FRAME_SIZE_16K=160）|
| 7 | WASAPI 环回缓冲 30ms | `init_loopback` 初始化参数 | 改为 20ms |
| 8 | 反向 AudioTrack 缓冲 80ms | `AudioPlayer.kt` BUFFER_MS=80 | 改为 40ms |
| 9 | Opus 默认复杂度 5 | `Prefs.kt opusComplexity=5` | 改为 3（更低编码延迟）|
| 10 | 默认 FEC=2（开启）| `Prefs.kt opusFec=2` | 改为 0（局域网无需FEC）|
| 11 | 默认丢包率 5% | `Prefs.kt opusPacketLoss=5` | 改为 0（局域网无丢包）|
| 12 | AudioTrack 缓冲加倍 | `finalBufSize = bufferSize.coerceAtLeast(minBufSize * 2)`，强制 2×minBufSize ≈80ms | 改为 `minBufSize`（仅保底不翻倍，降至 ~40ms）|
| 13 | AudioTrack 未启用低延迟模式 | 未设置 `PERFORMANCE_MODE_LOW_LATENCY`，系统默认高延迟缓冲 | 添加 `setPerformanceMode(LOW_LATENCY)`（API 29+）|
| 13 | AudioTrack 未启用低延迟模式 | 未设置 `PERFORMANCE_MODE_LOW_LATENCY`，系统默认高延迟缓冲 | 添加 `setPerformanceMode(LOW_LATENCY)`（API 29+）|
| 14 | 反向串流状态仅在收到音频时才更新 | WASAPI LOOPBACK 静音时不产生数据 → PC 不发包 → Android `hasAudio` 永为 false | `launchReverseAudio()` 启动即设 `reverseAudio=true`，不等首帧音频 |
| 15 | 手机 Opus 设置不同步到 PC 反向编码 | PC 反向编码器固定参数，手机调参后反向串流仍用旧设置 | 扩展 CONNECT 保活包（10 字节）携带手机端全部 Opus 参数（码率/带宽/复杂度/VBR/DTX/FEC/丢包率）；PC 端 `capture.rs` 每 1s 检查配置并热应用 `opus_encoder_ctl` 运行时调参 |

### 预期延迟（更新）

| 路径 | 优化前 | 优化后 |
|:----|:------:|:------:|
| 正向（Android→PC）| ~500ms | 50–80ms |
| 反向（PC→Android）| ~500ms | 50–80ms |

---

## 2026-06-09 — 全面改名 UDP2CAL + 广播控制 + 包名更新

### 概述

项目全面从 UDP2Mic 更名为 UDP2CAL，包名从 `com.udp2mic.app` 改为 `com.udp2cal.app`，清理所有残留旧引用。修复 UDP 广播在应用停止/已占用时仍响应的安全问题。

### 修复清单

| # | 问题 | 根因 | 修复 |
|---|------|------|------|
| 1 | 停止后 UDP 广播仍响应 | `start_broadcast_listener` 未检查 `APP_RUNNING` | 监听线程加入 `APP_RUNNING` 判断 |
| 2 | 已占用时仍可被搜索发现 | `start_broadcast_listener` 未检查 `DEVICE_READY` | 监听线程加入 `GLOBAL_DEVICE_STATE == DEVICE_READY` |
| 3 | 停止后主动广播仍进行 | `start_broadcast_state_machine` 未检查 UI 状态 | 主动广播线程加入 `APP_RUNNING` 判断 |
| 4 | 构建失败 Theme 引用错误 | `AndroidManifest.xml` 仍引用旧 `Theme.UDP2CALL` | 更新为 `Theme.UDP2CAL` |
| 5 | 多文件遗留旧名 | 注释、Prefs、CMakeLists、布局等未更新 | 全面扫描清理 |

---

## 2026-06-09 — 双向音频串流 + AEC 移植与修复

### 概述

将早期原型（`windows_cc`）的双向音频串流（PC 扬声器→手机听筒）及 AEC 功能移植到面向现代化设备的 `windows` / `android` 代码库。

### 修复清单

| # | 问题 | 根因 | 修复 |
|---|------|------|------|
| 1 | 保活重连每秒重启反向发送器 | keepalive CONNECT 携带反向端口，被误判为端口变化 | 仅端口真正变化时重启 |
| 2 | Win 重启后反向串流不恢复 | `REVERSE_ENABLED` 是进程级 static | 启动时从 config 同步 |
| 3 | 模式切换后反向串流卡死 | 端口变化需重启发送器；模式切换销毁 UdpSender | 端口变化时自动重启；模式切换保留 UdpSender |
| 4 | 音乐模式无声音 | `routeToEarpiece(false)` 改变全局音频路由模式，干扰 AudioRecord | 音乐模式跳过路由变更 |
| 5 | 音乐模式单声道（顶部扬声器） | AudioPlayer 固定走 VOICE_COMMUNICATION + 听筒路由 | 语音→听筒，音乐→扬声器 |
| 6 | 通知栏状态不更新 | `buildNotification()` 只调用一次 | 添加 `_status.collect` 协程动态更新 |
| 7 | 延迟 ~500ms | WASAPI 缓冲 100ms + AudioTrack 缓冲 300ms | WASAPI→30ms，AudioTrack→80ms（预计 ~150ms） |
| 8 | 低性能模式无声 | 立体声→单声道下混逻辑有 bug（原地修改破坏数据） | 重写为独立 mono_buf 累积+下混 |

### 协议变更

- CONNECT payload 扩展为 3 字节：2 字节反向端口 + 1 字节模式标志（0=高品质/1=低性能）
- 移除旧版纯文本发现协议支持

---

## 2026-06-07 — Android 端 UDP 乐观连接

### 现象

手机使用移动数据（未连接 WiFi）时点击"开始采集"，Opus 状态立即显示"已连接"，编码器正常工作，但实际上音频包全部发到虚空。

### 根因

两处乐观初始化导致连接状态脱离真实网络状况：
1. `CaptureService.kt` 音频引擎启动后立即标记 `connected = true`
2. 消费者协程 `p2pConnected` 初始化为 `true`

### 修复

- 启动后等待 `CONNECT_ACK` 才标记连接
- 每帧非阻塞 `drainAck()` 1ms 超时检测
- 断开时持续发 CONNECT 保活等待重连
