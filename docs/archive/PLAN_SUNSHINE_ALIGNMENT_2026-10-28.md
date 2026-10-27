# PLAN：Sunshine 对齐 —— 已实现 / 还能做什么（2026-10-28）

**归档材料（2026-10-28 归档）**。这是一次性的对照核对与**候选清单**，**不是**路线图、
**不是**规范；未实施项在实施前必须按当时的代码重新审查（行号会漂）。
现行真源仍是 [../PROTOCOL_V3_RTP.md](../PROTOCOL_V3_RTP.md) 与
[../LATENCY_CALIBRATION.md](../LATENCY_CALIBRATION.md)。

> 定位：外部调研 [RESEARCH_SUNSHINE_LOW_LATENCY.md](RESEARCH_SUNSHINE_LOW_LATENCY.md)
> 与本项目（音频串流，无视频）的逐项核对结论 + **候选改进清单**。
> 本文**不改变任何规范**，也**不修改任何代码**（核对当轮未动一行代码）。
> 已落地的借鉴不要在这里重复看，见协议 §8.7[^1]。

[^1]: `docs/PROTOCOL_V3_RTP.md:1253-1300`「8.7 传输层调优（借鉴 Sunshine 的实践）」——
    高精度发送节拍、套接字缓冲 1 MiB、PCM 环最新优先、节拍迟到统计、抖动缓冲满覆盖最旧槽、
    Opus 带内 FEC 重建，以及 4 条「刻意不做」。

## 1. 结论

1. Sunshine 的五条主干里，本项目**已有 4 条的等价物**（细节见 §2），差异只在音频特有的
   粒度上（10 ms 一帧一包 vs 5 ms；10 ms 网格 vs 1 ms 槽位）。
2. 视频专属项（B 帧/GOP/IDR/NVENC/1 帧 VBV/零拷贝纹理/帧率自适应/RS-FEC 独立包）
   **全部不适用**，协议 §8.7 已记录为刻意不做。
3. 真正还剩的活集中在 **4 类 19 项**（§4），其中 7 项是「改默认值 / 一行调用」级别，
   不需要动协议；另有 1 处**规范与实现不一致**（§3）。
4. 最便宜且真有问题的一处不是"学 Sunshine"，而是本项目的默认值：**正向 Opus 带内 FEC
   默认关闭**（`android/app/src/main/java/com/udp2cal/app/Prefs.kt:70-76`，
   `opus_fec` 默认 0、`opus_packet_loss` 默认 0），而接收端 LBRR 重建路径早已完备
   （`windows/src/main.rs:757-760`、`windows/src/decoder.rs:76-101`、协议 §8.7 表末行）——
   默认配置下这条机制常态空转。

## 2. 已有等价实现（核对过，不必再做）

| Sunshine 机制 | 本项目的等价物 | 证据 |
|---|---|---|
| 编码管线压到 1 帧（无 B 帧/GOP/前瞻） | 10 ms 固定帧、一帧一包、Opus 无重排序概念 | `protocol/rtp.rs:33-34`（`OPUS_FRAME_MS=10`/`OPUS_FRAME_SAMPLES=480`）、`windows/src/capture.rs:39` |
| 采集→编码「最新帧胜出」 | Windows：40 ms 有界装配缓冲，超限 `drain` 丢最旧；Android：**零队列**就地装配 | `windows/src/capture.rs:43`（`MAX_FRAME_BACKLOG = FRAME_INTERLEAVED * 4`）、`windows/src/capture.rs:601-609`（`pcm_buf.drain(..excess)` + `dropped_samples`）、`android/.../service/CaptureService.kt:60-63` |
| 绝对时刻调度、错过作废整组 | `slot + gap` 递增，迟到超半帧才重锚，且重锚不计入迟到统计 | `windows/src/capture.rs:730`（注释即引用 Sunshine 语义）、同文件 `ReverseStats.late_*`/`reanchors`；Android `CaptureService.kt:843-851` |
| 高精度定时器 | `CreateWaitableTimerExW(CREATE_WAITABLE_TIMER_HIGH_RESOLUTION)`，**不**调 `timeBeginPeriod` | `windows/src/hires.rs:44-51`、`windows/src/hires.rs:1`（注释） |
| 发送端自己整形（1 ms 槽位） | 音频一帧一包 → 用 10 ms 网格替代；实测迟到 21/300/1000 µs | 协议 §8.7 表首行 + `docs/PROTOCOL_V3_RTP.md:1274-1278` |
| `SO_SNDBUF`/`SO_RCVBUF` 1 MiB | 反向发送、主接收、Android 两端 | `windows/src/capture.rs:414-416`、`windows/src/main.rs:2236-2238`、`android/.../UdpSender.kt:51`、`:102`、`:109` |
| DSCP EF 打标 | `IP_TOS = 0xB8`，失败仅告警 | `windows/src/capture.rs:36`、`:402-408`；`android/.../UdpSender.kt:48`、`:96` |
| 队列满丢最旧（`drop_oldest`） | Windows PCM 环"最新优先"、抖动缓冲覆盖最旧槽 | `windows/src/audio.rs:108-113`（`push_latest`）、`protocol/rtp.rs:629-631` |
| 应用层冗余 | Opus 带内 FEC（LBRR）双端重建，实测 FEC 重建误差 0.0064 vs PLC 0.297 | 协议 §8.7 表末行；`windows/src/main.rs:757-760`、`android/.../CaptureService.kt:1282-1310` |
| 抖动/漂移闭环 | PI 漂移控制器 + 设备速率前馈 + 阶跃吸收 + self-designed ASRC（比 Sunshine 的 `AUTOCONVERTPCM` 强） | `protocol/rtp.rs:146`、`:161`、`:1567-1615`、`:1300-1480`；`windows/src/audio.rs:471-482`（48 抽头多相 FIR） |
| PLC 期限 | 12 帧 / 120 ms 上限 | `protocol/rtp.rs:123-131`、`:695-712` |
| 空闲重置契约 | 双端 1 s（本轮补齐 Windows） | `windows/src/main.rs:142`、`:2332-2346`；`android/.../CaptureService.kt:76`、`:971-978` |
| 无 ABR（静态预算） | 五档 64–512 kbps，会话中不探测 | `protocol/protocol.rs:87-99`；协议 §4.7 |

## 3. 规范与实现不一致（1 项，独立于 Sunshine）

**协议 §4.7 要求：编码结果 > 1275 字节时必须「丢弃该帧 + 计数 + **降低码率 / 关闭 FEC 冗余**」**
（`docs/PROTOCOL_V3_RTP.md:266-270`）。实现只做了前两步：

- Windows 反向：`windows/src/capture.rs:648-663` —— 超限整帧丢弃、`REVERSE_STATS.oversize` 自增，
  随后 `len = 0`（不发包）；**没有任何降码率或关 FEC 的动作**，只在 30 s 汇总行里打印
  （`windows/src/main.rs:1368`）。
- Android 正向：`android/.../native/OpusEncoder.kt:14-15` 注释「超限的一帧由 JNI 直接丢弃，绝不截断」，
  全仓 `grep oversize` 只命中 Windows 侧，**Android 连计数都没有**。

触发概率：CBR 512 kbps 的 10 ms 帧约 640 B，远低于 1275 B；只有 `vbr=1` 且 VBR 尖峰、
或开 FEC 后码率顶到上限时才可能达到。因此**影响低但不为零**，属"规范承诺了没实现"。

候选做法：窗口内 `oversize` 增长即 `set_bitrate(bitrate*0.9)`（`windows/src/capture.rs:301`
的 `set_bitrate` 已支持运行中生效），连续增长则 `set_inband_fec(false)`；Android 侧需在
`opus_jni.c` 里同样计数并回传。**成本中**，需要双端一起改，且要防止降档震荡（滞回 + 最小驻留）。

## 4. 还能做的（按性价比排序，本轮全部未实施）

### 4.1 改默认值 / 一行调用（零协议改动）

| ID | 项 | 依据 | 验证方式 |
|---|---|---|---|
| S1 | 正向 Opus 带内 FEC 默认打开（`opus_fec` 0→1，`opus_packet_loss` 0→5~10） | `android/.../Prefs.kt:70-76`（默认 0，注释「局域网不需要FEC」）；下发通道已存在 `opus_jni.c:46-47`；接收端重建已就绪 `windows/src/main.rs:757-760` | Windows `[RTP]` 行 `FEC=` 由 0 变正、`PLC=` 不涨（协议 §8.7 判读法） |
| S2 | 提 `reverse-capture` 线程优先级（`AvSetMmThreadCharacteristicsW("Audio")` 或 `THREAD_PRIORITY_HIGHEST`） | 线程创建处 `windows/src/capture.rs:331-335` 只设 `name`；全仓无 `SetThreadPriority`/MMCSS | `[Pacer] 30s` 的 `min/avg/max` 与 `重锚=`（`docs/LATENCY_CALIBRATION.md:167-176` 判读法） |
| S3 | Android 接收/控制/ACK 线程优先级 | 只有两处音频线程设了 `THREAD_PRIORITY_URGENT_AUDIO`：`CaptureService.kt:556`、`:1070`；`reverseRxLoop`（`:934+`）、`controlLoop`（`:444+`）、ACK 线程（`UdpSender.kt:207`）无设置 | 真机长跑看 `反向延迟` 行抖动与 ACK 新鲜度 |
| S4 | 正向 socket 补 `receiveBufferSize` | `UdpSender.kt:102` 只设 `sendBufferSize`；ACK 由**同一 socket** 接收，而 `receiveBufferSize` 只给了反向 socket（`:109`）→ 控制包突发时可能丢 ACK，进而误判断连 | 长跑看 `isAckFresh` 是否抖动（`UdpSender.kt:163`） |
| S5 | ACK 线程异常退避 | `UdpSender.kt:208-216` 异常后 `continue` 无 sleep；对照 `CaptureService.kt:965-968` 的 `delay(10)` | 断网/策略拦截下 CPU 占用 |
| S6 | `complexity` 默认 10 → 6~8 | `android/.../Prefs.kt:41-42`（默认 10） | 低端机 `[Pacer]` 迟到与 `encode_us` |
| S7 | `REVERSE_IDLE_RESET_MS` 分级 | `CaptureService.kt:76`（1 s）、`:971-978`（超时即 `jitter.reset()` → `dec.reset()` + `player.flush()`，一次可闻接缝）；建议改为 2~3 s 或仅当 `occupancyFrames()==0` 才 reset | 断流恢复时的破音次数 |

### 4.2 可观测性（不改协议）

| ID | 项 | 依据 |
|---|---|---|
| S8 | Windows `[RTP]` 行补 `loss_rate`（用 `highest_seq − next_seq − 空槽数` 推算） | 现有字段只有 `迟到/重复/溢出/重同步/PLC/FEC/水位`：`windows/src/main.rs:1353-1362`；`protocol/rtp.rs:376-392` 无 `loss_rate` |
| S9 | 积压/丢样进入告警口径 | `dropped_samples`（`capture.rs:605-607`）、`oversize`（`:655`）已有计数，只出现在 `[Pacer]`/`[Reverse]` 汇总行末尾 |
| S10 | Android 把 `jbLate/jbOverflow/jbPlc` 接到状态卡 | 原生 API 齐备 `android/.../native/ProtocolNative.kt:190-215`，目前只用于日志（`CaptureService.kt:1175`、`:1254-1264`） |
| S11 | 启动日志打印一次 `encoder.lookahead()` | getter 已存在：`vendor/audiopus/src/coder/encoder.rs:380`；水位预算目前没有这个已知量 |

### 4.3 缓冲/时钟语义（改共享协议代码，需双端回归）

⚠️ 这三项与 2026-10-28 二审的 P2-1/P2-2 同源（`docs/archive/CODE_REVIEW_2026-10-28_ROUND2.md`），
应作为**一次**协议层改动一起做，不能零散推。

| ID | 项 | 依据 |
|---|---|---|
| S12 | 补 0.2–1 s 的"够不到 resync、又没到 1 s 空闲阈值"窗口 | `protocol/rtp.rs:828-830`（`advance()` 无钳制，PLC/Silence 分支都推 `next_ts`）、`windows/src/main.rs:142` 只处理 >1 s；建议再加判据 `next_ts − highest_ts > PLC_MAX_MS` 即 `reset()` |
| S13 | `StepAbsorber` 在窗口次数上限内"不作为" | `protocol/rtp.rs:1050-1056`（`limited`/`limited_logged` 窗口逻辑）；上轮审查 F2 实测：积压 460 ms、`contiguous_frames=0` 时未见裁剪动作 |
| S14 | PI 误差项改用 `occupancy_ms`（不含空洞），`water_ms` 只作水位阈值 | 两个量都已存在：`protocol/rtp.rs:534-541`（`water_ms` 含空洞）与 `:549-552`（`occupancy_ms`）；短时乱序会抬高 `water_ms` 让 PI 误调 |

### 4.4 成本高（需协议版本窗口 / 全链路耦合）

| ID | 项 | 说明与依据 |
|---|---|---|
| S15 | 丢包反馈闭环（`TYPE_STATS` 1 Hz → FEC/码率档） | 控制面只有 5 种消息（`protocol/protocol.rs:58-64`）、ACK 仅 10 字节（`:68`）。Sunshine 也放弃了这条腿（其 `LOSS_STATS` 只打日志），本项目优势是统计已就绪，改造成本低于 Sunshine |
| S16 | 正向码率会话中协商 | 反向已支持运行中改（`windows/src/capture.rs:301` `set_bitrate`），正向（决定接收体验的方向）完全没有 |
| S17 | 目标水位自适应 / 可配 | 目前只有环境变量 `UDP2CAL_WATER_TARGET_MS`（`protocol/rtp.rs:90-120`，`OnceLock` 只读一次）；Android 侧无常量之外的通道 |
| S18 | 5 ms 帧 / `OPUS_APPLICATION_RESTRICTED_LOWDELAY` | 480 样本假设贯穿采集/RTP/抖动缓冲/重采样（`protocol/rtp.rs:33-34`、`CaptureService.kt:68`、`AudioPlayer.kt:37`）；需双端版本窗口 |
| S19 | 动态抖动缓冲容量 | `protocol/rtp.rs:44`（`MAX_REORDER_PACKETS=64`）写死在 `[PacketSlot; 64]`（`:429-431`），改动态容量要动 JNI 内存布局 |
| S20 | AAudio/Oboe + `setBufferSizeInFrames` | `AudioPlayer.kt:37`/`:71-79` 缓冲一次定死；`setBufferSizeInFrames` 只有 getter（`:216`） |

## 5. 不建议做（与 Sunshine 的差异是刻意的）

- **NACK/重传**：10 ms 帧 + 局域网 RTT，重传来不及；协议 §4 已列为非目标。
- **RS-FEC 独立包**：100 包/秒下冗余包带宽代价远高于视频场景；协议 §8.7 已给出用 LBRR 替代的理由。
- **GSO/USO 批发送、`SO_PRIORITY`**：一帧一包、≤1287 B，无合并收益；Windows 无 `SO_PRIORITY`。
- **32 槽整队清空**：本项目 64 槽逐条丢最旧（`protocol/rtp.rs:624-631`），整队清空会一次丢 640 ms。
- **帧率只降不升 / 空闲补帧**：采集节拍由声卡决定，无帧率概念。
- **逐包加密**：与延迟无关的取舍，协议 §1 已列为非目标。

## 6. 若继续推进，建议顺序

1. **S1 + S4 + S5 + S6**（Android 默认值与 socket/线程，一批小改动，用现有 diag 与日志即可验收）
2. **S2 + S3**（线程优先级，各一两行）
3. **S8 + S9 + S10 + S11**（可观测性，先能看见再谈自适应）
4. **§3 规范不一致**（超限降码率/关 FEC，双端一起）
5. **S12 + S13 + S14**（协议层一次性解决，需真机听感验收）
6. **S15 + S16**（跨端版本窗口，收益只在无线干扰剧烈场景）

## 7. 证据与未验证

- 依据：三路只读代码调研（Windows 发送路径 / 接收与协议层 / Android 全侧）逐项核对，
  结论均带 `文件:行`；本文表格中的关键行号已由本人二次核对（`capture.rs:43/414-416/601-609/648-663`、
  `rtp.rs:44/534-552/624-631/828-830/1050-1056`、`hires.rs:44-51`、`main.rs:2236-2238/1353-1368`、
  `Prefs.kt:70-76`、`UdpSender.kt:102/109/205-216`、`CaptureService.kt:556/1070`）。
- **未执行**（本轮不修改代码，无验证对象）：任何真机/端到端验证、`cargo`/`gradle` 门禁重跑、
  MMCSS 在具体 Windows 版本上的实际收益、Android `minBufSize` 的设备相关实际值、
  Opus 固有 lookahead 的实测值。
- **推论未实测**（标注以免当成结论）：
  1. Windows 40 ms 装配缓冲"一旦积压到上限就长期停在上限"——由"读取速率≈发送速率"推出，
     未在真机波形上确认；
  2. Android 正向 socket 缺 `receiveBufferSize` 会导致 ACK 被丢从而误判断连——机制上成立，
     未构造丢包场景验证；
  3. `REVERSE_IDLE_RESET_MS` 1 s 对 AP 攒批偏激进——来自 `docs/LATENCY_CALIBRATION.md:191-200`
     的实测（单次到位突发中位 90 ms、每 1.2 s 一次），但 1 s 阈值是否真造成破音未单独测量。
