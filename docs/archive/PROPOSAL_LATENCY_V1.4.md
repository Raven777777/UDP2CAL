# UDP2CAL 优化方案（基于 lannet 实测，2026-10-26）

> 本文的每一条都来自 `lannet` demo 在同一链路上的实测（证据见 demo 仓库的 `README.md` §6）。
> 适用范围：UDP2CAL v1.3.0（单一时钟 / 单一口径 / 单一手段的 ASRC 架构），落地目标 v1.4.0。
> 状态：**已落地于 v1.4.0，已归档**。A1 是操作项（代码无改动）、A2/B/C 已进代码；逐条对应见
> 文末 §11 落地记录。文中数值以实际落地代码为准（与初稿有出入处已在落地记录里标出）。
> 现行真源：[../PROTOCOL_V3_RTP.md](../PROTOCOL_V3_RTP.md) §7.10（目标水位与阶跃吸收）、
> [../LATENCY_CALIBRATION.md](../LATENCY_CALIBRATION.md)（实测口径与基线）。

---

## 0. 一句话结论

症结**不在音频管线**，而在两处网络层事实：

1. **手机每 60–120 s 离信道做全频段扫描**（`com.vivo.multinlp`，每次约 1.85 s）→ 双向同时 600–1100 ms 停摆；
2. **空口 BE 队列攒批**（成对/成批到达）→ 每包约 1.1 ms 超额延迟。

由此推出四条改动：**A 网络层（关扫描 + DSCP 46，零代码收益最大）、B 目标水位 260→150、C 阶跃吸收（整帧裁剪）、D 文档/诊断配套**。

| 优先级 | 改动 | 层 | 预期收益 |
|---|---|---|---|
| **P0** | 关手机全频段扫描（A1） | 环境 | 风暴 600–1100 ms → 0 次 |
| **P0** | 双端 DSCP 46（A2） | 网络 | 超额延迟 25.6% → 6.7%，p99 38 → 17 ms |
| **P1** | `WATER_TARGET_MS` 260 → 150（B） | 协议 | 总延迟下降约 100 ms |
| **P2** | 阶跃吸收（C） | 协议 | 消除"补齐后 20–40 s 变调" |

---

## 1. 证据（同一条链路，`lannet` 120 s 长跑，100 pps × 1280 B）

| 实验 | 结果 | 说明 |
|---|---|---|
| 三承载 TCP/UDP/QUIC | 断供占比 2.6%–17.8%，同一量级 | 不是协议问题；ICMP ping 同样 5→131 ms；全程 0 丢包 |
| pps 100/60/30 | 每包超额 1.07 / 1.11 / 1.63 ms | 总量随 pps 降只是包变少，**每包代价恒定** |
| 扫描禁用前 | 每 60 s 一次 600–1100 ms 风暴，双向同时 | 与扫描时刻逐秒对齐（4/4 命中，偏差 <1 s） |
| 扫描禁用后 | 4 次长跑 **0 次 >200 ms**，最大空档 68–90 ms | 风暴根因确证 |
| DSCP 0 vs 46 | 超额 25.3/25.6% → 14.6/11.9% → 双端 6.2–18.4% / 6.7% | p95 23 → 13 ms，p99 38 → 17 ms |
| DSCP 34（AF41） | PC 下行反而劣化（超额 13.8%，p99 41.9 ms） | **不要用 34**，只信 46 |

---

## 2. 现状诊断：v1.3.0 为什么还在"漂"

1. **`WATER_TARGET_MS = 260` 是为扛 1 s 风暴设的**。风暴消失后，260 ms 里约 200 ms 是纯粹多出来的延迟。
2. 即使没有风暴，AP 侧仍会成批交付（空档 70–90 ms）。一次补齐 100 ms 会让水位冲到 `target + 100`。
3. 而**排水能力只有 ±0.5%，即 5 ms/s** → 排 100 ms 需要 **20 s**，期间 `adjust` 一直贴着 ±5000 ppm，
   折合 **±8.6 音分**——这就是听感上的"音调漂移"。
4. 结论：**缓冲目标（静态）与排水能力（动态）必须配套**。有风暴时要大缓冲；没风暴时要小缓冲 + 阶跃吸收。

---

## 3. 方案 A：网络层（零代码，先做）

### A1 关掉手机全频段扫描（根治）

| 手段 | 命令 / 路径 | 说明 |
|---|---|---|
| adb | `adb shell pm disable-user --user 0 com.vivo.multinlp` | 实测有效；恢复用 `pm enable` |
| 系统设置 | 设置 → 位置信息 → Wi-Fi 扫描（关闭） | vivo 走自己的 `WifiScanner` 通道 |
| 参考 | `adb shell "dumpsys wifiscanner \| grep 'start scan'"` | 确认扫描源与节拍 |

无效手段（已试）：`settings put global wifi_scan_always_enabled 0`（vivo 不读这个开关）、
`pm revoke ... ACCESS_FINE_LOCATION`（`SYSTEM_FIXED`）。

### A2 双端 DSCP 46（EF）

**Android（发送正向 RTP 流）**：`android/app/src/main/java/com/udp2cal/app/UdpSender.kt:67`

```kotlin
val sock = DatagramSocket()
sock.trafficClass = 0xB8            // DSCP 46 (EF) << 2；被系统忽略也无副作用
```

（`:69` 的 `revSock` 只用于接收反向流，**不需要**标记。）

**Windows（发送反向 RTP 流）**：`windows/src/capture.rs:248` 的 `send_sock`

```rust
// Cargo.toml 加 socket2 = "0.6"
if let Ok(s) = socket2::SockRef::from(&send_sock).set_tos_v4(0xB8) { /* 失败只记日志 */ }
```

Windows 默认会把应用设置的 TOS 清零，两种放行办法（二选一，均需管理员）：

```powershell
# 方案 1：策略式（推荐，不需要改代码之外的系统状态）
New-NetQosPolicy -Name UDP2CAL -AppPathNameMatchCondition udp2cal.exe -DSCPAction 46

# 方案 2：允许应用自行设置（注册表，重启后生效）
reg add "HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters" /v DisableUserTOSSetting /t REG_DWORD /d 0 /f
```

接收侧（`windows/src/main.rs:1654` 的监听 socket）**不需要**标记。

**验收**：diag CSV 里 `adjust_ppm` 贴 ±5000 的时间占比 <5%（当前约 10–20%）。

---

## 4. 方案 B：`WATER_TARGET_MS` 260 → 150

| 组成 | 实测 | 说明 |
|---|---|---|
| PCM 环（被 `DECODE_LIMIT_MS=40` 钉住） | 44 ms | 节流门开一帧 → 49，再排空 |
| 声卡缓冲 | ≈10 ms | VB-Cable 无 playback 时间戳时可能为 0 |
| 抖动缓冲需要的余量 | 68–90 ms（禁用扫描后最大空档） | ×1.6 余量 ≈ 140 |
| **合计建议** | **150 ms** | 比现状低 110 ms |

落地 5 处（必须成对改，否则两端语义不一致）：

1. `protocol/rtp.rs` 的 `pub const WATER_TARGET_MS`；
2. `android/app/src/main/java/com/udp2cal/app/Udp2CalProtocol.kt` 的同名常量；
3. `docs/PROTOCOL_V3_RTP.md` §7.2 参数表 + §7.10.1 的取值理由（重写为"网络环境决定"）；
4. `docs/LATENCY_CALIBRATION.md` §5.1 已知环境基线与 §3.2 冷启动临时目标；
5. `docs/CHANGELOG.md` 顶层条目。

**已实现**（原为可选）：环境变量覆盖 `UDP2CAL_WATER_TARGET_MS`（仅 Windows 端），允许区间
`WATER_TARGET_MIN_MS = 80` ～ `WATER_TARGET_MAX_MS = 600`，越界钳位，进程内首次读取后缓存
（`protocol::rtp::water_target_ms()`），目标经 `DriftController::with_target()` 注入。
Android 端保持常量 150，另用上述两个常量把允许区间文档化。两端**必须同值**。

**验收**：稳态 `water_ms` 中位 150±20 ms、标准差 <25 ms、`adjust_ppm` 贴 ±5000 占比 <5%、
PLC <2 次/分、端到端总延迟 ≤190 ms。

---

## 5. 方案 C：阶跃吸收（整帧裁剪）

**问题**：一次 100 ms 的补齐，PI 需要 20 s 才排完，期间一直贴钳位 → 变调。

**判据**（在 `windows/src/main.rs` 解码循环里，已有 `jb_water`/`pipe_ms`）：

```
若 单次 tick 内 jb.water_ms() 增长 > STEP_TRIM_GROWTH_MS（80 ms）
或 jb + pipe > target + STEP_TRIM_EXCESS_MS（target + 100 ms）
则 裁剪 n = ceil((jb + pipe − target) / FRAME_MS) 帧
   n ≤ PLC_MAX_MS / FRAME_MS = 12 帧；且每 60 s 最多 3 次（STEP_TRIM_MAX_PER_MIN）
```

（与初稿的差别：第一条只看抖动缓冲的增长，第二条看**总量**；`n` 用**总量**减去 target 再
向上取整到整帧，不是只看抖动缓冲。）

**实现位置**：

- `protocol/rtp.rs`：`JitterBuffer` 增加 `pub fn skip_frames(&mut self, n: u32) -> u32`（`advance()` n 次、清空对
  应槽位；`next_ts >= highest_ts` 即停，不越过已到达媒体；不计 PLC、不推 `epoch`；计 `stats.step_trims`）；
- `protocol/rtp.rs`：新增 `StepAbsorber` / `StepDecision`（纯判定器，只看数字不碰缓冲；首次 `decide`
  只登记基准；`can_trim = jb.is_playing()` 为 false 时不下结论）；Kotlin 侧同构；
- `windows/src/main.rs`：在 `jb.pull` 之前调用，命中则 `skip_frames` 并继续正常播放（**不**产生 PLC 帧），日志前缀 `[Step]`；
- `android/.../Udp2CalProtocol.kt`：同构 `skipFrames()` + `CaptureService.kt` 的 `reversePlayLoop()`
  在 `jitter.pull` 之前、`linkLock` 内调用；
- `windows/src/diag.rs`：CSV 增加一列 `trims`（累计跳过的整帧数）。

**为什么不回到样本级丢插**：v1.3.0 已按规范 §六 风险3 删掉 slip；阶跃只能用**整帧级**硬重同步，
样本级回落会引入接缝（每秒一次 1 ms 淡变）。

**代价**：一次 10–120 ms 的时间跳（上限 12 帧），不做淡入淡出（v1.4.0 未实现交叉淡化）；
每分钟 ≤3 次的限额做保护，超限只记录不裁剪。

---

## 6. 明确不做

- 不做样本级丢/插（v1.3.0 已删，别加回来）；
- 不做跨设备绝对时钟同步（前馈已经吃掉 ±10 ppm 晶振差）；
- 不压低 `JITTER_START_MS` / `PLC_LEAD_MS` 去换延迟（会把延迟换成 PLC 破音）；
- 不动 `DECODE_LIMIT_MS`（它是环门，调大会死锁、调小会碎帧）;
- 不改 `PI_TI_S`（<5 s 会把网络抖动变成变调）。

---

## 7. 验收清单（环回 + 实机）

| 指标 | 现状（v1.3.0） | 目标 | 采集方式 |
|---|---|---|---|
| 稳态 `water_ms` 中位 | 275–284 | 150±20 | `UDP2CAL_ASRC_DIAG=20` CSV |
| 稳态 `water_ms` 标准差 | 31–64 | <25 | 同上 |
| `adjust_ppm` 贴 ±5000 占比 | 10–20% | <5% | 同上 |
| PLC 次数 | 0.4–1.1 次/分 | <2 次/分 | 同上 `plc` 列 |
| 总延迟（arrival → audible） | 275–285 ms | ≤190 ms | `[Latency]` 日志 |
| 30 min 单向漂移 | — | 无 | CSV 线性拟合斜率 ≈0 |
| 环回 A/B 的 PLC 率 | 基线 | 不劣化 | `LATENCY_CALIBRATION.md` §3.2 |

---

## 8. 风险与回退

1. **环境可能变**：换路由器/换手机/对方装新 App 都可能重新引入扫描源。
   → 已提供 `UDP2CAL_WATER_TARGET_MS`（仅 Windows，80–600 钳位）现场覆盖；文档写明了
   “目标值必须 ≥ 实测最大空档 ×1.5–1.6”（`PROTOCOL_V3_RTP.md` §7.10.1）。
2. **AP 可能无视 DSCP**：只是白标，无副作用；用 diag CSV 的 `adjust_ppm` 分位数判断是否真的生效。
3. **方案 C 参数过激**：会出现"跳音"。用每分钟 ≤3 次的上限 + `trims` 统计观察，超限则只记录不裁剪。
4. **Windows 侧 TOS 被清零**：必须配 QoS 策略或 `DisableUserTOSSetting=0`，否则 PC→手机方向的标记无效（但也不会有害）。

---

## 9. 附录：常用命令

```bash
# 诊断打点（20 ms 一行，写 %LOCALAPPDATA%\UDP2CAL\asrc_diag.csv，最长 300 s）
set UDP2CAL_ASRC_DIAG=20

# 临时覆盖目标水位（仅 Windows 接收端，允许 80–600 ms，越界钳位）
set UDP2CAL_WATER_TARGET_MS=210

# 确认扫描源与节拍
adb shell "dumpsys wifiscanner | grep 'start scan'"

# 关闭 / 恢复 vivo 全频段扫描
adb shell pm disable-user --user 0 com.vivo.multinlp
adb shell pm enable com.vivo.multinlp

# Windows 放行应用设置 TOS（管理员）
reg add "HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters" /v DisableUserTOSSetting /t REG_DWORD /d 0 /f
```

CSV 列含义见 `docs/LATENCY_CALIBRATION.md` §5 与 `windows/src/diag.rs` 表头。

---

## 10. 落地顺序建议

1. 先做 **A1 + A2**（A1 不改代码、A2 只需一个套接字选项，立刻可验证）：观察 diag CSV 的 `adjust_ppm`、`plc` 是否明显好转。
2. 再做 **B**（改常量 + 文档），实测 150 ms 是否还稳；不稳就退回 170/190，不要一步回到 260。
3. 最后做 **C**（阶跃吸收），因为它是 B 的保护措施；没有 B 就不需要 C。

---

## 11. 落地记录（v1.4.0）

本节记录提案与实际落地代码的逐条对应。**以本节与代码为准**，上文保留当时的论证。

### A1 关掉 vivo 全频段扫描（纯操作项，代码无改动）

- 关闭：`adb shell pm disable-user --user 0 com.vivo.multinlp`；恢复：`adb shell pm enable com.vivo.multinlp`。
- 该扫描每 60–120 s 占用无线 1.85 s，造成双向 600–1100 ms 停摆；关闭后 4 次长跑 0 次 >200 ms，
  稳态最大空档 68–90 ms。
- 它是 v1.4.0 其余优化（B/C）的**前提**：不关扫描，150 ms 目标不可达。代码里没有对应改动。

### A2 DSCP EF 标记（双端 0xB8）

- Android：`android/app/src/main/java/com/udp2cal/app/UdpSender.kt`，正向 RTP 的 `DatagramSocket`
  设 `trafficClass = 0xB8`（常量 `DSCP_EF_TOS = 0xB8`）。
- Windows：`windows/src/capture.rs`，反向发送 socket 用
  `socket2::SockRef::from(&send_sock).set_tos_v4(0xB8)`（常量 `DSCP_EF_TOS: u32 = 0xB8`），
  失败只告警、不影响功能。
- Windows 侧需 `New-NetQosPolicy ... -DSCPAction 46` 或注册表 `DisableUserTOSSetting=0` 才会真正
  放行；接收侧不标记。说明见 `PROTOCOL_V3_RTP.md` §8.6。

### B 目标总水位 260 → 150

- `protocol/rtp.rs`：`pub const WATER_TARGET_MS: u32 = 150`；
- `android/app/src/main/java/com/udp2cal/app/Udp2CalProtocol.kt`：`WATER_TARGET_MS = 150`（同值），
  另加 `WATER_TARGET_MIN_MS = 80` / `WATER_TARGET_MAX_MS = 600` 作为允许区间文档化；
- Windows 新增运行时覆盖：`WATER_TARGET_ENV = "UDP2CAL_WATER_TARGET_MS"`、`water_target_ms()`
  （80–600 钳位，进程内首次读取后缓存）、`DriftController::with_target(device_rate, target_ms)`。
- 文档同步：`PROTOCOL_V3_RTP.md` §7.2 / §7.10.1 / 常量表 / §11.4，`LATENCY_CALIBRATION.md`
  §2 / §3.2 / §3.4 / §5.1。

### C 阶跃吸收（step trim）

- `protocol/rtp.rs`：`JitterBuffer::skip_frames(n)`、`StepAbsorber` / `StepDecision`、
  `JitterStats.step_trims`；Kotlin 侧同构：`skipFrames(n)`、`StepAbsorber` / `StepDecision`、
  `stepTrims`。两端常量同值：`STEP_TRIM_GROWTH_MS = 80`、`STEP_TRIM_EXCESS_MS = 100`、
  `STEP_TRIM_MAX_PER_MIN = 3`。
- 接入点：`windows/src/main.rs` 解码循环（`jb.pull` 之前）、
  `android/.../service/CaptureService.kt` 的 `reversePlayLoop`（`jitter.pull` 之前、`linkLock` 内）；
  日志前缀 `[Step]`。
- 诊断：`windows/src/diag.rs` 的 `UDP2CAL_ASRC_DIAG` CSV 新增末列 `trims`（累计跳过的整帧数）。

### 与初稿数值的出入（以代码为准）

1. `n` 用**总水位**（`jb + pipe`）减 `target` 后向上取整到整帧，上限 12 帧（`PLC_MAX_MS / FRAME_MS`）；
   初稿写的是 `water - WATER_TARGET_MS`。
2. 第二条判据是总水位 > `target + STEP_TRIM_EXCESS_MS`（含 `pipe`），初稿写的是 `pipe_ms > target + 100`。
3. 裁剪帧数上限就是 `PLC_MAX_MS / FRAME_MS = 12`；没有额外的“5 ms 淡入淡出”（v1.4.0 未实现交叉淡化）。

### 测试

- Rust（`protocol` crate）新增 **9** 个单元测试：`skip_frames` × 3（推进时间线且不计 PLC /
  不越过已到达媒体 / 未进入 PLAYING 不动作）、`StepAbsorber` × 5（稳态噪声不触发 / 单批增长触发 /
  超预算触发 / 每分钟限额 / 仅 PLAYING 生效）、水位常量 × 1（`WATER_TARGET_MS == 150` 且覆盖值
  落在允许区间）；既有的调度阈值序测试补上了 `DECODE_LIMIT_MS < WATER_TARGET_MS` 断言。
- Kotlin（`Udp2CalProtocolTest`）新增 **8** 个用例，与上述 skip/step 一一对应；常量断言改为 150。
