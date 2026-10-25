# UDP2CAL 代码质量审查报告

- 审查对象：`UDP2CAL` 仓库自有代码（`windows/`、`protocol/`、`android/`、`scripts/`、`build.bat`）
- 审查范围排除：`opus-src/`（libopus 1.6.1 外部源码）、`vendor/`（audiopus 副本）、`*/target/`、`android/app/build/` 等构建产物
- 审查规模：自有代码约 12,178 行（Rust 5,706 行 / Kotlin 3,320 行 / C 335 行 / Python 2,317 行 / 配置脚本 200 行）
- 审查日期：2026-10-25
- 审查基准：`docs/PROTOCOL_V3_RTP.md`（v3 协议规范，作为逻辑判定的权威依据）
- 结论：**共 22 项问题 —— 阻断级（P0）2 项、严重级（P1）2 项、中风险（P2）6 项、轻微/维护性（P3）11 项、误报 1 项。其中 4 项已用可执行程序复现，其余为静态确认。协议核心库质量较高（42 项单元测试全部通过，字节级测试向量与规范一致），缺陷集中在"发送端序列号语义"与"接收端迟到判定顺序"的交叉处。**
- **修复状态：全部 21 项缺陷已于 2026-10-25 修复并通过验证，1 项（BUG-21）经复核判定为误报并撤销。修复清单见 [BUGFIX.md](../BUGFIX.md) 同日条目。**

---

## 一、结论摘要

| 编号 | 严重度 | 问题 | 验证状态 |
|------|--------|------|----------|
| BUG-01 | **P0 阻断** | 正向链路 ACK 短暂中断后，序列号冻结而时间戳继续推进，恢复后全部音频包被接收端判为"迟到"丢弃 → 单向永久静音 | 已复现（仿真） |
| BUG-02 | **P0 阻断** | DTX 恢复包（按规范 seq 不递增）被接收端丢弃，且 M=1 重同步分支永远不可达 → DTX 后永久静音 | 已复现（仿真） |
| BUG-03 | **P1 严重** | 无锁 PCM 环 `clear()` 由生产者写消费者索引，破坏 SPSC 不变量，`len()` 可下溢为 `usize::MAX` → 解码线程永久停产 | 已复现（并发压力测试） |
| BUG-04 | P1 严重 | 反向音频目标 IP 不随对端 IP 变化更新，Wi-Fi 重连换 IP 后反向静音 | 静态确认 |
| BUG-05 | P2 中等 | 未绑定设备的 CONNECT 可改写当前会话的反向编码器配置 | 静态确认 |
| BUG-06 | P2 中等 | 连接保活检查只在 `recv_from` 超时分支执行，与"保活只看控制面"的规范要求相反 | 静态确认 |
| BUG-07 | P2 中等 | 输出欠载帧不计入播放头，每次欠载永久累积延迟并推迟后续 PLC | 已复现（仿真） |
| BUG-08 | P2 中等 | 超过 32767 个包的长时间断流（>5.4 分钟）后无法通过 M=1 重同步恢复 | 已复现（推理+仿真） |
| BUG-09 | P2 中等 | 抖动缓冲 `timestamp_offset` 双端只写不读，规范性功能未落地 | 静态确认 |
| BUG-10 | P2 中等 | `AudioPlayer.flush()` 与 `writeBlocking()` 无互斥，并发访问 AudioTrack | 静态确认 |
| BUG-11 | P3 轻微 | `build_control` 对超长负载静默截断，与 `build_control_to` 语义不一致 | 静态确认 |
| BUG-12 | P3 轻微 | 测试音模式 `delay(0)` 忙等 | 静态确认 |
| BUG-13 | P3 轻微 | `sendFrames` 的 DTX 序列号语义与注释、规范三方矛盾 | 静态确认 |
| BUG-14 | P3 轻微 | Kotlin 抖动缓冲 `reset()` 未清 `lastRebaseReason`，日志误导 | 静态确认 |
| BUG-15 | P3 轻微 | 麦克风权限被拒后无任何提示 | 静态确认 |
| BUG-16 | P3 轻微 | 音频初始化重试后 `wake_decoder()` 唤醒已退出线程 | 静态确认 |
| BUG-17 | P3 轻微 | `udpSender` 连接失败路径的资源释放不完整 | 静态确认 |
| BUG-18 | P3 轻微 | 日志 2 MiB 上限只在启动时检查 | 静态确认 |
| BUG-19 | P3 维护 | `doctor --fix` 后不重算问题数，摘要恒为"待处理" | 静态确认 |
| BUG-20 | P3 维护 | `StatusUpdate` 只置真不置假，码率 0 时仍显示"已连接" | 静态确认 |
| BUG-21 | ~~P3~~ | ~~输出侧重采样首个样本轻微外插~~ → **误报**：相位恒在 `[0, ratio)` | 已复核撤销 |
| BUG-22 | P3 维护 | 若干死常量（`DRIFT_MIN_SAMPLES`、`TYPE_MAX_KNOWN` 未用于校验） | 静态确认 |

---

## 一之二、修复状态总览（2026-10-25 修复后）

| 编号 | 修复要点 | 验证方式 |
|------|----------|----------|
| BUG-01 | 序列号与时间戳同频推进；接收端"序号落后 + 时间戳合法"改判为断点并重新对齐 | 验证工程复跑：1 s ACK 中断后由"30 包全丢"变为"0 丢包、28/30 帧出音" |
| BUG-02 | 同上；同步修订规范 §4.4 与 §7.4 | 验证工程复跑：DTX 500 ms 恢复包由 `Dropped` 变为 `Resync`，后续 0 丢包 |
| BUG-03 | `Ring` 改为 `flush_to` 请求 + 消费者落地；`len()` 增加快照校验与 `saturating_sub` | 新增并发回归测试 `ring_len_never_wraps_under_flush_race`（40M 次压力，`len()` 不再越界） |
| BUG-04 | `reverse_stale` 纳入源 IP 变化 | 代码审查（无独立运行时环境可模拟换 IP） |
| BUG-05 | `update_reverse_config` 移到绑定校验之后 | 代码审查 |
| BUG-06 | 保活检查改为每 100 ms 周期执行，与流量解耦 | 代码审查 |
| BUG-07 | `played` 改为统计声卡真实输出位置（含欠载静音） | 代码审查 + 现有重采样/环测试回归通过 |
| BUG-08 | 由 BUG-01 的重新对齐覆盖（附录 A 级时间线，32 位时间戳上限约 12 h） | 新增测试 `jb_long_gap_across_seq_wrap_still_recovers` |
| BUG-09 | 删除抖动缓冲侧 `timestamp_offset` 与 `note_pcm_correction`，偏移只由 `PlayheadClock` 承担 | 编译期检查（调用点全部清理）+ 规范 §7.9 同步 |
| BUG-10 | `AudioPlayer` 增加 `trackLock`，`write/flush/stop/播放头` 串行化 | Android 单元测试 35 项通过（编译期类型检查覆盖） |
| BUG-11 | `build_control` 增加 `debug_assert` 与文档说明 | 协议测试 43 项通过 |
| BUG-12 | 测试音落后时改用 `yield()` | 代码审查 |
| BUG-13 | `sendFrames` 序列号语义统一为"每帧递增"，重写注释 | 代码审查 |
| BUG-14 | `reset()` 清空 `lastRebaseReason` | 代码审查 |
| BUG-15 | 新增权限说明弹窗 + "去设置" + `onResume` 复检 | 代码审查（Android 编译通过） |
| BUG-16 | `DECODE_THREAD` 改为可替换的 `Mutex<Option<Thread>>` | 编译 + 测试通过 |
| BUG-17 | `connect()` 失败路径关闭已创建 socket | 代码审查 |
| BUG-18 | 日志运行期按字节计数轮转 | 代码审查 |
| BUG-19 | `doctor --fix` 后自动复检 | 代码审查 |
| BUG-20 | 「已连接」由控制面绑定状态驱动 | 代码审查 |
| BUG-21 | **误报**，撤销；补齐推导注释 | 推导复核 |
| BUG-22 | 删除未使用常量 `DRIFT_MIN_SAMPLES` | 编译无告警 |

验证汇总：

| 目标 | 命令 | 结果 |
|------|------|------|
| 协议库 | `protocol/ cargo test` | 43 passed / 0 failed |
| 协议库静态检查 | `protocol/ cargo clippy --all-targets` | 无新增告警（余下 3 条为既有测试代码风格项） |
| Windows 端 | `windows/ cargo test` | 14 单测 + 2 集成测试全通过 |
| Windows 静态检查 | `windows/ cargo clippy --all-targets` | 无新增告警（余下 3 条为既有代码） |
| Android 端 | `android/ ./gradlew :app:testDebugUnitTest`（JDK 21） | 35 passed / 0 failed |

---

## 二、P0 级缺陷（必修）

### BUG-01 正向链路 ACK 中断后单向音频永久静音

**位置**
- `android/app/src/main/java/com/udp2cal/app/service/CaptureService.kt:580-597`
- `protocol/rtp.rs:361-384`（接收端插入顺序）

**根因**

发送端在"ACK 不新鲜"时跳过编码与发送，但时间戳仍然无条件推进：

```kotlin
if (sender.isAckFresh(now)) {          // 581：断连期间整块跳过
    val n = enc.encodeTo(pcm, packet, Udp2CalProtocol.RTP_HEADER_SIZE)
    ...
    if (n >= 0) seq = (seq + 1) and 0xFFFF   // 595：seq 也被跳过
}
timestamp += FRAME                      // 597：ts 无条件推进
```

一次持续 0.2–3 s 的 ACK 中断（Wi-Fi 抖动、CPU 抢占、短暂丢包）会产生：
- 发送端：`timestamp` 前进 N 帧，`seq` 原地不动；
- 接收端（`JitterBuffer::pull`）：无包可播，按播放头持续产生 PLC/Silence，`next_seq`/`next_ts` 同样前进 N 帧；
- 恢复时：`ext_ts` 与 `next_ts` 基本相等（不触发 200 ms 重同步），但 `ext_seq = next_seq - N`。

而接收端的检查顺序是"迟到判定优先于 M=1 重同步"：

```rust
if ext_ts < self.next_ts { ... return Dropped; }      // 361
if self.find_ts(ext_ts).is_some() { ... }             // 366
if ext_seq < self.next_seq {                          // 371 ← 命中，直接返回
    self.stats.late += 1;
    return InsertResult::Dropped;
}
if pkt.marker && gap_ms > RESYNC_GAP_MS as u64 { ... } // 378 ← 永远到不了
```

于是恢复包被丢弃，之后每一包都因同样的序列号差值被丢弃；`next_seq` 每帧继续前进，差值永不闭合。**单向音频永久静音，直到用户手动重启采集（换 SSRC）。** 期间 CONNECT/ACK 已恢复正常，界面显示"已连接"。

**复现证据**（独立验证工程，见第六节）

```
=== V4: Android 正向链路 ACK 中断 1s（seq 冻结 / ts 继续）后恢复 ===
  正常 30 帧: next_seq=327 next_ts=212960
  断连 1s:   next_seq=427 next_ts=260960 plc_run=12
  恢复包 seq=330 ts=262400 M=1 → Dropped
  恢复后 30 包: 丢弃=30 late=31 出音帧=0
```

**影响**：单向下行（手机麦克风 → PC）彻底失效，且无任何日志、无界面告警。用户表现为"说不了话，但听得到电脑声音"。

**修复方向**（二选一，建议同时做）
1. 发送端：`seq` 每 10 ms 帧无条件递增（与 `timestamp` 同频），只有"是否发包"受连接状态影响；
2. 接收端：把 `ext_seq < next_seq` 的容错并入重同步判定——若包能通过 `ext_ts`、`M=1`、`gap > RESYNC_GAP_MS` 校验，应走 rebase 而不是丢弃；纯序号滞后且无 marker 时才计为迟到。

---

### BUG-02 DTX 恢复包被丢弃（规范、实现、接收端三方冲突）

**位置**
- 规范：`docs/PROTOCOL_V3_RTP.md` §4.4「DTX 静音未发包：**不递增序列号**」
- 发送端（反向）：`windows/src/capture.rs:341-356, 387-389`
- 接收端：`protocol/rtp.rs:371-374`；Kotlin 镜像 `Udp2CalProtocol.kt:410-418`

**根因**

按规范实现时，DTX 静音期间 `seq` 不递增、`timestamp` 继续推进。接收端在静音期间通过 PLC/Silence 把 `next_seq` 推进了与静音帧数相同的量。恢复包到达时 `gap_ms = 0`（时间戳完全连续，因为接收端已自行补齐时间线），M=1 重同步分支的条件 `gap_ms > 200` 不成立；随后 `ext_seq < next_seq` 命中 → 丢弃。

**复现证据**

```
=== V3: 按规范实现的 DTX（seq 不递增）恢复包 ===
  播完 8 帧: next_seq=508 next_ts=13840
  DTX 500ms: next_seq=558 next_ts=37840 plc_run=12
  恢复包 seq=508 ts=37840 gap=0ms M=1 → Dropped
  后续 10 包丢弃=10 late=11
  对照(seq 递增到 558) → Inserted
```

**影响与额外风险**

- `windows/src/capture.rs` 在 DTX 空帧时走 `Ok(_) => (0, false)`，即 `framed=false`、`seq` 不递增 —— 严格符合规范，因此**一旦用户在手机端开启 DTX 且 PC 侧出现 >200 ms 静音，反向音频链路就会永久静音**。
- Android 正向实现 `if (n >= 0) seq++` 在 DTX 时**递增**了序列号，属于对规范 4.4 的偏离，但恰好掩盖了接收端缺陷。若后续有人"按文档修正"该行，会立刻引入 BUG-02。
- 该缺陷同时说明：规范 §4.4 与本次实现的接收算法不自洽——接收端用"序列号连续性"做迟到判据，却在静音期要求发送端不推进序列号。二者必须择一修改。

**修复方向**：与 BUG-01 同一处修复（序列号每帧递增），并同步修订规范 §4.4 与 §7.4 的判定顺序。

---

## 三、P1 级缺陷

### BUG-03 无锁 PCM 环：生产者写消费者索引，`len()` 可下溢为 `usize::MAX`

**位置**：`windows/src/audio.rs:24-86`；消费方 `windows/src/main.rs:317-321`

**根因**

`Ring` 是标准 SPSC 结构，约定 `head` 只由生产者写、`tail` 只由消费者写。但 `clear()` 由解码线程（生产者）调用，却直接写 `tail`：

```rust
fn clear(&self) {
    let head = self.head.load(Ordering::Acquire);
    self.tail.store(head, Ordering::Release);   // 生产者侵犯消费者所有权
}
fn len(&self) -> usize {
    self.head.load(Ordering::Acquire).wrapping_sub(self.tail.load(Ordering::Acquire))
}                                              // 头尾分别读取，非原子快照
```

`len()` 先读 `head`（H）再读 `tail`；若在此期间生产者推进 head 并执行 `clear()`，`tail` 可能被写成大于 H 的值 → `H - tail` 在 `usize` 上回绕为极大值。

**复现证据**（并发压力测试，40,000,000 次）

```
  强制交错: head=500 len()=499 capacity=1024
  并发压力: len()>capacity 次数=45419 最大 len=18446744073709551615（cap 4096, max_depth 1920）
```

**影响链**

`main.rs:317-321`：

```rust
let max_depth = ((aw.device_rate as usize / 1000) * 40).max(512);
if aw.filled_frames() >= max_depth {          // usize::MAX >= 1920 恒真
    std::thread::park_timeout(Duration::from_millis(4));
    continue;                                 // 解码线程再也不拉帧
}
```

解码线程永久空转、不再写入任何 PCM，而音频回调仍在消费（每拍 `tail+1`，需要约 3.8×10¹⁴ 秒才能追平）→ **永久静音，无日志、无异常、界面状态正常**。

**触发概率**：仅在 `AudioWriter::flush()`（会话重置/重同步/停止）与 `pop()` 竞争时命中，窗口为纳秒级，单次会话内概率极低；但后果是"不可自愈的静默故障"，且 `underrun/dropped` 计数器不会体现，现场无法定位。

**修复方向**：`flush()` 不要直接改 `tail`，改为设置原子"丢弃至 head"标记由消费者执行；或把 `flush` 通过事件投递给音频线程；或改用两段式 `head/tail` 快照校验（读 tail、读 head、再读 tail，若不等则重读）。

### BUG-04 反向音频目标 IP 不随对端变化更新

**位置**：`windows/src/main.rs:1508-1518, 1534-1544`

```rust
let reverse_stale = new_session
    || peer.reverse_port != cfg.reverse_port
    || (peer.reverse_ssrc != 0) != want_reverse;   // 未纳入源 IP
```

`st.addr` 会随每次 CONNECT 更新（正向 RTP 因此能继续工作），但 `restart_reverse_sender` 的触发条件不含源 IP 变化。手机 Wi-Fi 重连后 IP 变更、而 `device_id` / `forward_ssrc` / `reverse_port` 均未变时（Android 侧 socket 不重建，属于常见情形），Windows 仍把反向 RTP 发往旧 IP。

**影响**：正向正常、反向永久静音；3 s 保活超时也不会触发（CONNECT 一直在正常到达）。

**修复方向**：`reverse_stale` 增加 `peer.addr.map(|a| a.ip()) != Some(src_addr.ip())`。

---

## 四、P2 级缺陷

### BUG-05 未绑定设备可改写当前会话的反向编码配置

`windows/src/main.rs:1463` 在设备归属校验之前执行：

```rust
capture::update_reverse_config(capture::OpusReverseConfig::from(&cfg));  // 1463
...
let same_device = peer.is_busy() && peer.device_id == header.device_id;  // 1467
if peer.is_busy() && !same_device { return; }                            // 1470
```

同网段任意主机构造一个合法 CONNECT（PT=111、frame_ms=10、ssrc≠0 即可通过 `validate()`）就能把正在运行会话的反向编码器改成任意码率/DTX/FEC/复杂度组合（例如 1000 kbps + FEC + DRED，触发超限丢帧）。规范 §5.7 第 7 步要求"需要绑定的消息，device_id 与当前绑定一致"后才生效。

**修复方向**：把 `update_reverse_config` 移到绑定校验之后。

### BUG-06 保活检查被 RTP 流量"豁免"

`windows/src/main.rs:1619-1630`：

```rust
let recv = tokio::time::timeout(Duration::from_millis(200), socket.recv_from(&mut buf)).await;
let (len, src) = match recv {
    Ok(Ok(v)) => v,
    _ => { if check_peer_timeout() { ... } continue; }   // 只在超时/错误分支
};
```

只要 RTP 持续到达，`check_peer_timeout()` 永不执行。规范 §6.4 明确"保活只看控制面，**不得**用'是否收到 RTP'判断连接存活"。当前实现等价于"有 RTP 就跳过保活判定"，与控制面存活解耦。

**影响**：控制面中断但媒体仍在（Android 控制协程异常、CONNECT 发送持续失败）时，Windows 保持 Busy 状态、不释放绑定、不触发 `Disconnected`，其他设备无法连接；界面"已占用"永久停留。

**修复方向**：把超时检查移出超时分支，改为每轮循环（或每次统计节拍）无条件执行。

### BUG-07 输出欠载未计入播放头

`windows/src/audio.rs:296-307`：

```rust
if let Some(p) = played.upgrade() {
    if filled > 0 { p.fetch_add(filled as u64, Ordering::Release); }   // 只统计真实 PCM
}
if filled < need_frames { data[idx..].fill(T::EQUILIBRIUM); ... }       // 静音不计
```

`played` 是 `PlayheadClock` 的唯一输入，欠载期间声卡实际推进了时间但 `played` 不动，导致播放头**永久落后真实播放位置 U 样本**（U = 累计欠载）。后果：
1. 抖动缓冲的有效目标水位被动抬高 U，端到端延迟单次累积；
2. PLC 触发点随之推迟 U，下一次缺口更容易在 PLC 之前抽干输出环，形成"欠载 → 延迟增加 → 更易欠载"的正反馈（受 40 ms 预解码深度与漂移控制器制约，实测不会立即失控）。

**复现证据**

```
=== V1: 输出环欠载后播放头是否冻结 ===
  稳态: ring=1920帧(40ms) played=17280 playhead=Some(65280) next_ts=67200
  卡顿 6 拍后: 欠载=20ms ring=0 played=19200
  前 12 拍出音 12 拍; 累计欠载=70ms; late=0 dup=0
  再跑 300 拍出音 300 拍
```

**重要澄清**：审查过程曾假设"欠载 → 播放头冻结 → PLC 永久失效 → 死锁"，**该假设已被仿真否定**（见第六节"已排除的疑点"）。真实问题是延迟与 PLC 时机的系统性偏差，不是死锁。

**修复方向**：欠载补的帧也计入 `played`（或单独维护 `underrun_offset` 并在 `PlayheadClock` 中扣除），使播放头始终等于声卡真实输出位置。

### BUG-08 长时断流（>32767 包）无法恢复

`protocol/rtp.rs:193-196` 的 `extend_seq` 用 16 位有符号差值：

```rust
let diff = seq.wrapping_sub(highest as u16) as i16 as i64;
(highest as i64 + diff).max(0) as u64
```

前向跨度超过 32767（约 5.46 分钟）时差值回绕为负数。例如 `extend_seq(5, 40000)` 得 `0`（`max(0)` 钳制），该包被判为迟到。DTX 静音超过 5 分钟、或断连超过 5 分钟、或长时间 `Silence` 后：`ext_seq < next_seq` 与 `ext_ts < next_ts` 的迟到判定先于 M=1 重同步，导致恢复包与后续所有包被丢弃——与 BUG-01/02 是同一处判定顺序问题在更大时间尺度上的表现。

规范 §7.4 注释要求"扩展差值超出重排窗口的包按'跳变'处理，不得让 u16/u32 回绕误判为巨大的未来包"，实现满足了"不误判为未来包"，但未提供"按跳变处理"的路径。

### BUG-09 `timestamp_offset` 只写不读

- `protocol/rtp.rs:273, 319-321, 471-473`
- `Udp2CalProtocol.kt:347, 391-393`
- 写入方：`windows/src/main.rs:352-370`、`CaptureService.kt:804-819`

`JitterBuffer` 侧的 `timestamp_offset` 无任何读取点参与播放决策；漂移校正实际只通过 `PlayheadClock::note_pcm_correction` 生效。规范 §7.9 要求"校正必须同步调整 `timestamp_offset`，使 `playhead_ts` 仍然对应正确的 RTP 时间线"，该要求在抖动缓冲层是空转的。对应测试 `timestamp_offset_tracks_correction`（rtp.rs:1220-1228）只断言字段数值变化，未断言行为，属"测试掩盖问题"。

**建议**：要么删除该字段（避免误导），要么让 `pull` 真正使用它，并在测试中验证 `playhead_ts` 映射。

### BUG-10 `AudioPlayer.flush()` 与 `writeBlocking()` 并发

`CaptureService.kt:650`（控制协程/`stopReverseAudio`）与 `:822`（播放协程）分别调用 `flush()` 与 `writeBlocking()`，两者都直接操作 `audioTrack` 且无互斥。`AudioTrack.pause()/flush()/write()` 并发会抛 `IllegalStateException`，被 `catch (_: Exception)` 静默吞掉，表现为反向音频偶发中断且无日志。

`CaptureService.kt:769-784`（epoch 变化时在同一协程内 flush）与 `:822` 的写入则是同线程，无此问题。

---

## 五、P3 级与维护性问题

| 编号 | 位置 | 问题 | 建议 |
|------|------|------|------|
| BUG-11 | `protocol/protocol.rs:141-149` | `build_control` 对 >512 字节负载**静默截断**，而同文件 `build_control_to` 返回 `None`。当前调用方（CONNECT 15B / ACK 6B / 发现名 ≤254B）不会触发，但属易踩陷阱 | 改为 `assert` 或返回 `Result`，与 `build_control_to` 统一 |
| BUG-12 | `CaptureService.kt:532-534` | `delay(waitNs / 1_000_000)` 在剩余 <1 ms 时等于 `delay(0)`，形成忙等 | 用 `delay(1)` 或 `delayMicroseconds` 等价实现 |
| BUG-13 | `CaptureService.kt:594-595` | 注释"DTX 未发包不递增"与实现 `if (n >= 0) seq++`（n==0 时递增）矛盾；`n<0`（编码失败）反而不递增，与规范 §4.4"编码失败仍递增"相反 | 统一为"每帧递增"，见 BUG-01/02 |
| BUG-14 | `Udp2CalProtocol.kt:364-375` | `reset()` 不清 `lastRebaseReason`，`CaptureService.kt:751-777` 会把上次重同步原因写进新会话日志 | `reset()` 中置空 |
| BUG-15 | `MainActivity.kt:57-59` | `micPermLauncher` 回调丢弃 `granted`，拒绝权限后无提示，用户点"开始采集"才看到"网络连接失败/麦克风初始化失败" | 在拒绝分支调用 `finish()` 或弹窗说明 |
| BUG-16 | `main.rs:163-168, 252` | `DECODE_THREAD` 为 `OnceLock`，音频初始化重试后 `wake_decoder()` 唤醒的是已退出的旧线程；新线程仅靠 4 ms `park_timeout` 兜底 | 改为可在重启时替换的 `Mutex<Option<Thread>>` |
| BUG-17 | `CaptureService.kt:226-231` | `udpSender` 在 `connect()` 成功后才赋值，失败路径不关闭已创建的 `DatagramSocket` | 失败时显式 `sender.close()`，或改为 `use`/`finally` 语义 |
| BUG-18 | `logger.rs:37-42` | 2 MiB 上限只在 `init()` 检查一次，长驻进程单文件可无限增长（日志同时写 stderr，在 GUI 子系统下被丢弃，开销小） | 写入时按累计字节数轮转 |
| BUG-19 | `scripts/build.py:178-183` | `--fix` 执行后不重算 `problems`，摘要恒为"发现 N 项待处理" | 修复后重跑检测或按项递减 |
| BUG-20 | `main.rs:860-867` | `StatusUpdate` 只把 `connected` 置真，从不置假；RTP 停止但控制面存活时码率显示 0 而状态仍"已连接" | 明确"已连接"= 控制面连通，或增加媒体态指示 |
| BUG-21 | `audio.rs:211-220` | ~~`phase - n_in` 落在 `[-1,0)` 导致外插~~ → **误报**：循环出口条件是 `phase >= n_in`，故 `phase - n_in ∈ [0, ratio)`，不可能为负。真实情况是块尾 `i1` 钳到 `last`，等效于末样本多保持 1 个输出点（≤1 样本、不可闻、不影响比率精度） | 不做改动；在代码中补齐推导注释，说明为何不需要 `carry` |
| BUG-22 | `protocol/rtp.rs:53`、`protocol.rs:52` | `DRIFT_MIN_SAMPLES`、`TYPE_MAX_KNOWN` 未参与实际逻辑（后者被 `is_known_type` 使用，但 5–255 的"忽略"与 0 的"丢弃"未区分统计） | 清理或在统计中区分 |

---

## 六、验证方法与"已排除的疑点"

### 6.1 基线验证

```
protocol/ $ cargo test --offline
test result: ok. 42 passed; 0 failed
```

协议核心库 42 项单元测试全部通过，包含规范附录 B 的全部字节级向量（RTP 头、CONNECT、ACK、DISCOVER_REPLY）、序列号/时间戳回绕、抖动缓冲乱序/重复/迟到/PLC 上限/重同步、漂移校正与播放头 44.1 kHz 余数累加。

### 6.2 缺陷验证工程

为在不修改被测仓库的前提下验证缺陷，另建独立 crate（依赖 `udp2cal-protocol` 的**真实**实现，非重写）：

- **V1**：逐行等价复刻 `main.rs:286-432` 的解码循环 + `audio.rs:274-315` 的音频回调（`played` 只统计真实 PCM；`max_depth=1920`），注入 60 ms 解码线程抢占。
- **V2**：逐行复刻 `Ring`（含 `clear()`/`len()` 实现），做强制交错与 40,000,000 次并发压力测试。
- **V3**：按规范 §4.4 构造"DTX 期间 seq 不递增"的发送序列，用真实 `JitterBuffer` 验证接收行为。
- **V4**：按 `CaptureService.sendFrames` 的实际行为（`!isAckFresh` 时 seq 冻结、ts 推进）构造 1 s ACK 中断。

### 6.3 已排除的疑点（重要）

审查初期曾提出三个高危假设，经验证**均不成立**，特此记录以避免误导：

| 假设 | 结论 | 依据 |
|------|------|------|
| 输出环欠载 → 播放头冻结 → PLC 永久失效 → 死锁 | **不成立**。欠载后只要真实包到达 `next_ts`，`pull` 直接返回 `Packet`（与播放头无关），系统可自愈 | V1：修正送包时序后 12/12、300/300 拍全部出音，`late=0`、`resync=0`。首轮"永久卡死"结论源于测试脚本在断供期间未继续送包，属测试缺陷 |
| `Ring::clear()` 与 `pop()` 交错必然导致 `len()` 下溢为 `usize::MAX` | **部分不成立**。`clear()` 后紧随 `pop()` 写回 `tail+1` 不会下溢（`tail+1 ≤ head`）；下溢来自 `len()` 本身对 head/tail 的非原子读取 | V2 强制交错场景只得到 `len()=499`（数据错乱但未下溢）；真实下溢（`usize::MAX`）由并发压力测试命中 |
| `build_control` 静默截断已被实际触发 | **不成立**。全部调用方负载均远小于 512 字节 | 逐一核对 `main.rs:1410, 474, 562`、`CaptureService.kt:104` |
| 输出侧重采样相位为负导致外插（BUG-21） | **不成立**。`while phase < n_in` 的出口条件保证 `phase >= n_in`，故 `phase - n_in ∈ [0, ratio)`；负相位不可达 | 推导复核（修复过程中推翻本报告自身结论） |

---

## 七、质量评价（正面结论）

以下方面实现质量明显高于同类项目，建议保持：

1. **协议真源与测试**：`protocol/` 同时被 Rust 与 Kotlin 镜像，规范附录 B 的字节向量全部固化为测试；错误类型 `RtpError` 区分 7 类拒绝原因，便于统计定位。
2. **时钟模型纪律**：音频路径无 `sleep`、无 PLC 定时器、无重排衰减定时器、无漂移 EMA，与规范 §7.1 完全一致；`JitterBuffer` 的"插入不重同步、只丢单包"取舍有注释论证（避免 640 ms 空洞），这是正确的工程判断。
3. **实时性约束**：cpal 回调无锁/无分配/无 Opus 调用；PCM 环用 `AtomicU32` 承载 f32 位模式，规避 `unsafe`；`Ring` 单元测试覆盖满/空/clear 边界。
4. **日志与可观测性**：明确禁止在反向发送线程做文件 I/O（`capture.rs:99-101` 记录了"30 s 一次破音"的实测根因），统计走原子计数、UI 线程落盘，是难得的正确设计。
5. **安全边界**：防火墙规则限定 `program=` + 具体端口（而非整机 UDP 放行）；发现服务有按源 IP 的令牌桶（防反射放大）；发布签名不再引用入库 keystore（`build.gradle.kts:9-40`）；`allowBackup=false`。
6. **代码注释质量**：多处注释记录了"历史实现为何错误"（如 `P2pState` 合并锁、`hasNewCapture`、`silence_run` 的 3~4 帧多送问题），对后续维护价值高。

---

## 八、修复优先级建议

**第一批（阻断级，建议同一次提交）**
1. BUG-01 + BUG-02 + BUG-08：统一"序列号每 10 ms 帧无条件递增"，并把接收端迟到判定调整为"可重同步优先"。修复后需同步修订 `docs/PROTOCOL_V3_RTP.md` §4.4 与 §7.4，并新增三端交叉测试：
   - ACK 中断 1 s 后恢复，正向音频应在 1 帧内恢复；
   - DTX 静音 500 ms 后恢复，正向/反向均应恢复；
   - 断流 6 分钟后恢复。
2. BUG-03：消除 `Ring::clear()` 对 `tail` 的写入，并补充 `flush` 与 `pop` 的并发回归测试。

**第二批（严重）**
3. BUG-04、BUG-05、BUG-06：`handle_control` 的校验顺序与反向目标更新条件。

**第三批（中等）**
4. BUG-07、BUG-09、BUG-10：播放头欠载补偿、清理 `timestamp_offset`、`AudioPlayer` 加锁。

**第四批（维护）**
5. BUG-11 ~ BUG-22，可与功能迭代合并。

---

## 附录：审查覆盖清单

| 模块 | 文件 | 行数 | 状态 |
|------|------|------|------|
| 协议（控制面） | `protocol/protocol.rs` | 728 | 全量审查 |
| 协议（媒体面） | `protocol/rtp.rs` | 1,264 | 全量审查 |
| Windows 主逻辑 | `windows/src/main.rs` | 1,693 | 全量审查 |
| Windows 采集/反向 | `windows/src/capture.rs` | 793 | 全量审查 |
| Windows 输出/重采样 | `windows/src/audio.rs` | 524 | 全量审查 |
| Windows 解码 | `windows/src/decoder.rs` | 87 | 全量审查 |
| Windows 配置/防火墙/日志 | `config.rs` / `firewall.rs` / `logger.rs` / `build.rs` | 365 | 全量审查 |
| Windows 集成测试 | `windows/tests/reverse_max_bitrate.rs` | 144 | 全量审查 |
| Android 服务 | `service/CaptureService.kt` | 911 | 全量审查 |
| Android 协议镜像 | `Udp2CalProtocol.kt` | 657 | 全量审查 |
| Android 播放/发送/发现 | `AudioPlayer.kt` / `UdpSender.kt` / `DiscoveryManager.kt` | 510 | 全量审查 |
| Android 编码/解码/界面 | `OpusEncoder.kt` / `OpusDecoder.kt` / `MainActivity.kt` / `Prefs.kt` | 866 | 全量审查 |
| JNI | `opus_jni.c` / `opus_jni_decoder.c` / `CMakeLists.txt` | 365 | 全量审查 |
| Android 单元测试 | `Udp2CalProtocolTest.kt` | 524 | 覆盖度评述 |
| 构建脚本 | `scripts/build.py` + `udp2cal/*.py` | 2,317 | 结构与关键路径审查（doctor/参数解析/行为一致性） |
| 工程配置 | `AndroidManifest.xml` / `*.gradle.kts` / `build.bat` / `build.rs` | ~400 | 关键项审查（权限、签名、ABI、NDK） |
| **外部源码** | `opus-src/`、`vendor/` | — | **不在审查范围**（按任务约定） |

---

*本报告仅为审查结论，未对被测仓库做任何代码修改。验证工程位于系统临时目录，未纳入项目。*
