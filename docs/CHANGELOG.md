# 版本历史

> 按版本倒序排列。单个 Bug 的根因分析与修复细节见 [BUGFIX.md](BUGFIX.md)。

---

## v1.5.0 — 2026-10-26 · 协议单一实现：Android 改调 Rust（JNI），删掉 1059 行 Kotlin 副本

> v1.4.0 及之前，协议行为在 `protocol/rtp.rs` + `protocol/protocol.rs` 与
> `android/.../Udp2CalProtocol.kt` 里各写了一遍。两份实现靠人眼对照，每次调参（如 v1.4.0 的
> 阶跃吸收）都要改两处并人工比较，稍不留神就变成"一端能连、另一端静音"。本版把 Android
> 变成**同一份 Rust 实现的调用方**，桌面 JVM 单测与真机跑的是同一份机器码。
> **线格式未改**：与 v1.4.0 完全互通，旧客户端不受影响。

**新增**
- `protocol/jni.rs`：`#[cfg(feature = "jni")]` 的 JNI 表面，71 个
  `Java_com_udp2cal_app_native_ProtocolNative_*` 符号；crate 新增 `cdylib`，一份代码同时产出
  Android 的 `libudp2cal_protocol.so` 与桌面的 `udp2cal_protocol.dll`。
  - 纯函数：控制头 / CONNECT / CONNECT_ACK / DISCOVER_REPLY / RTP 头 / 码率钳位与吸附；
  - 对象：`JitterBuffer`、`StepAbsorber`、`RateEstimator`、`DriftController` 均为 `jlong` 句柄
    （`Box::into_raw` + `Mutex`，`close()`/`finalize()` 释放）；
  - 时钟桥：`clockSync(System.nanoTime())` 记下 `(Instant, java_nanos)` 基准，`arrivalNanosAt` 与
    `RateEstimator::observe` 双向换算，不需要 `GetJavaVM`；
  - 常量：`constants()` 导出 43 个整数、`constantsReal()` 导出 8 个浮点。
- `android/.../native/ProtocolNative.kt`（218 行）：`external fun` 声明 + `System.loadLibrary`。
- 两个防漂移测试：`nativeConstantsMatchKotlinConstants`、`nativeRealConstantsMatchKotlinConstants`
  把 Rust 导出的常量与 Kotlin `const val` 逐项比对。

**变更**
- `android/.../Udp2CalProtocol.kt`：1059 行 → 492 行，只剩常量、DTO 打包/解包与句柄转发；
  `generateDeviceId` / `deviceIdToString` 保留在 Kotlin（`SecureRandom` 与格式化不属于协议行为）。
- `protocol/rtp.rs`：补上 `last_rebase_reason`（Kotlin 早就有、Rust 缺，统一后必须对齐），
  `ARRIVAL_TRACE` 改为 `pub`（常量导出需要）。
- `protocol/protocol.rs`：`DiscoverReply::decode` 改为与 Kotlin 既有行为完全一致（`port != 0`、
  名字 ≤254 字节、UTF-8 宽松解码、空名回落 `"UDP2CAL PC"`）——统一实现不能顺手改可见行为。
- `android/app/build.gradle.kts`：新增 `cargoBuildAndroidProtocol`（NDK `clang.exe` 交叉编译）、
  `syncRustJniLibs`（`.so` 进 `build/rustJniLibs/<abi>/`，`preBuild` 依赖）、`cargoBuildHostProtocol`
  （桌面 dll，`java.library.path` 指向 `protocol/target/release`）。
- `android/app/proguard-rules.pro`：新增 `-keep com.udp2cal.app.native.ProtocolNative`——
  JNI 符号名由**类名+方法名**拼成（`Java_com_udp2cal_app_native_ProtocolNative_*`），
  R8 一改名就是运行期 `UnsatisfiedLinkError: native method not found`（`minifyEnabled = true` 时必现）。
- `CaptureService.kt`：`onDestroy()` 与反向播放循环结束时 `close()` 原生句柄（jitter/drift/rate/step），
  句柄状态在 Rust 堆上，不归还就是泄漏。

**构建前提**
- Android 构建现在需要 Rust 工具链 + `rustup target add aarch64-linux-android`（见 [BUILD.md](BUILD.md)）。
- 交叉编译用 `clang.exe` + `-C link-arg=--target=aarch64-linux-android29` +
  `-C link-arg=--sysroot=<ndk>/toolchains/llvm/prebuilt/windows-x86_64/sysroot`；NDK r23+ 没有
  `ndk/<ver>/sysroot` 这个目录。

**审查修复（同一版本的收尾回合）**
- 拉取缓冲容量被"上一次收包长度"绑架：`read_bytes` 精确缩容后，VBR 下紧跟一个短包就会
  把后续更长的帧判成 `WAIT`（时间轴不前进、播放线程空转 → 断音）。改为 `ensure_payload_capacity()`
  在 insert/pull 两侧都补齐到 `OPUS_MAX_PAYLOAD`，并加回归用例
  `jitter_pullSurvivesVariablePayloadLengths`（1/100 字节交替；去掉修复后此用例必红，已实测）。
- `DriftController.close()` 未清零句柄且 `finalize()` 会二次 `close()` → 双释放（发生在 finalizer
  线程，堆损坏无栈可查）。四个句柄类统一成 `if (handle != 0L) { destroy; handle = 0L }`。
- `new_string` 改走 `NewString`（UTF-16）而不是 `NewStringUTF`（Modified UTF-8）：对端设备名
  含 emoji 等补充平面字符时，后者在 CheckJNI 下会 abort。
- `buildControl` 补 `payload` 为 null 的检查（ART 对 null 数组取长度会解引用空指针）。
- 时钟桥从 `OnceLock` 改为可覆盖的 `Mutex<Option<_>>`：`clockSync` 若晚于任何一次导出调用，
  旧写法会把基准永久钉成 `java_nanos = 0`，让 `arrivalNanosAt` 返回"库加载后经过的纳秒"，
  进而毒化前馈速率估计（音质劣化但看起来一切正常）。未同步时现在返回 `-1`。
- `instant_from_nanos` 正方向改用 `checked_add`（原 `Instant + Duration` 在越界时 panic，
  跨 `extern "system"` 就是 abort）。
- `dcUpdate` 的 `device_frames` 加 `max(0)` 守卫；`jbArrivalNanosAt` 的 `ts` 同理。
- `writeRtpHeader` 改为返回 `Boolean`，目标数组过小时不再静默写旧头。
- `CaptureService.onDestroy()` 与反向播放循环退出时 `close()` 原生句柄（jitter/drift/rate/step），
  并让 `[Step]`/`[Latency]` 日志打印 `drift.targetMs()`（Rust 侧可被环境变量覆盖）。
- `protocol/jni.rs` 的 `*_ref` 返回 `Option<&'static _>`（句柄由 `Box::into_raw` 保证存活到 destroy）。
- Gradle：`ndkVersion = ndkVersionStr`（避免两处写死后不一致）；NDK 工具链按主机选择
  `windows-x86_64`/`linux-x86_64`/`darwin-x86_64` 并在缺失时明确报错；`cargoExe()` 增加 PATH 扫描与
  找不到时的明确提示；交叉编译改用 `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`（不覆盖用户 `RUSTFLAGS`）。
- `proguard-rules.pro` 保留 `ProtocolNative`（见上）。

**验证**
- `cd protocol && cargo test --offline` → 67 passed（含新增的 `last_rebase_reason` 与 DISCOVER 解码用例）。
- `./gradlew :app:testDebugUnitTest` → **62 passed / 0 failed**，全部跑在 Rust 实现上
  （含 9 个 `skip_frames`/`StepAbsorber` 与两个常量一致性测试）。
- `./gradlew :app:assembleDebug` → APK 内含 `lib/arm64-v8a/libudp2cal_protocol.so`
  （ELF aarch64、Android 29、NDK r27）。
- `cd windows && cargo build --offline` → 无变化（Windows 端本就依赖 `protocol` crate）。

---

## v1.4.0 — 2026-10-26 · 环境为前提的延迟回收：DSCP EF + 目标水位 150 + 阶跃吸收

> v1.3.0 把 `WATER_TARGET_MS` 定在 260 ms，是因为手机每 60–120 s 做一次全频段扫描
> （`com.vivo.multinlp`，每次约占无线 1.85 s），造成双向 600–1100 ms 停摆。关掉扫描后稳态
> 空档只剩 68–90 ms，260 ms 里约 110 ms 是纯多出来的延迟。本版把环境前提写进发布流程，
> 并给降下来的水位配一个**阶跃吸收**兑底。两端仍需成对升级。

**环境前提（A1，纯操作项，代码无改动）**
- 关闭 vivo 全频段扫描：`adb shell pm disable-user --user 0 com.vivo.multinlp`
  （恢复用 `adb shell pm enable com.vivo.multinlp`）。实测 4 次长跑 0 次 >200 ms，
  最大空档 68–90 ms。**不关扫描，本版的 150 ms 目标不可达。**

**新增**
- DSCP EF 标记（双端 TOS `0xB8` = DSCP 46）：Android `UdpSender.kt` 给正向 RTP 的
  `DatagramSocket` 设 `trafficClass = 0xB8`（`DSCP_EF_TOS`）；Windows `capture.rs` 给反向发送
  socket 加 `socket2::SockRef::from(&send_sock).set_tos_v4(0xB8)`（`DSCP_EF_TOS: u32`），
  失败只告警。接收侧不标记；Windows 侧还需 `New-NetQosPolicy -DSCPAction 46` 或注册表
  `DisableUserTOSSetting=0` 才会真正放行。实测空口超额延迟 ~25%（p99 38 ms）→ ~7%
  （p99 17 ms）
- 阶跃吸收（方案 C）：`JitterBuffer::skip_frames` / Kotlin `skipFrames` 做整帧硬重同步，
  只推进播放头 + 清槽，**不产生任何 PLC 样本**，且不得越过已到达的最新媒体
  （`next_ts >= highest_ts` 即停）；新判定器 `StepAbsorber` / `StepDecision`（只看数字不碰
  缓冲，首次 `decide` 只登记基准，`can_trim = jb.is_playing()`）；新常量
  `STEP_TRIM_GROWTH_MS = 80`、`STEP_TRIM_EXCESS_MS = 100`、`STEP_TRIM_MAX_PER_MIN = 3`
  （超限只记录不裁剪）；新统计 `JitterStats.step_trims` / `stepTrims`（不计入 `plcFrames`）
- 诊断：`UDP2CAL_ASRC_DIAG` 的 CSV 新增末列 `trims`（累计跳过的整帧数），共 16 列
- `UDP2CAL_WATER_TARGET_MS`（仅 Windows）：现场覆盖目标水位，允许 80–600 ms，越界钳位，
  进程内首次读取后缓存（`protocol::rtp::water_target_ms()`），经
  `DriftController::with_target()` 注入

**变更**
- `WATER_TARGET_MS` 260 → **150**（两端同值）：关扫描后断供幅度从 ~200 ms 降到 68–90 ms，
  ×1.6 安全系数 ≈ 140 ms → 取 150 = 解码环 44 + 声卡 ~10 + 抖动余量 ~96；剩下的单次台阶
  交给阶跃吸收，不再靠水位全量预留（原来 ±0.5% 的排水速度排 200 ms 要 40 s，那 40 s 就是变调）
- 验收口径改为：`water_ms` 中位 150±20 ms、标准差 <25 ms、`adjust_ppm` 顶满占比 <5%、
  PLC <2 次/分、端到端总延迟 ≤190 ms

**文档**
- `PROTOCOL_V3_RTP.md`：§7.2 参数表（含阶跃三常量与水位覆盖区间）、§7.10.1 重写为
  “由网络环境决定”、新增 §7.10.2 阶跃吸收、§8.6 DSCP 标记、§7.13 统计、常量汇总与 §11 验收同步
- `LATENCY_CALIBRATION.md`：§2 日志示例与判读、§3.2 目标水位 A/B 做法（改用环境变量）、
  §3.4 判定表、§5 诊断列（`trims`）、§5.1 基线与结论（260 → 150，含 v1.4.0 口径）
- `archive/PROPOSAL_LATENCY_V1.4.md`：状态改为“已落地”，新增 §11 落地记录（A1/A2/B/C/测试逐条对应）；v1.5.0 起归档

**测试**
- Rust（`protocol`）新增 9 项：`skip_frames` × 3、`StepAbsorber` × 5、水位常量 × 1；
  既有调度阈值序测试补上 `DECODE_LIMIT_MS < WATER_TARGET_MS` 断言
- Kotlin（`Udp2CalProtocolTest`）新增 8 个对应用例，常量断言改为 150

---

## v1.3.0 — 2026-10-26 · ASRC 重构：单一时钟、单一口径、单一手段

> 按 [archive/PLAN_ASRC_REFACTOR.md](archive/PLAN_ASRC_REFACTOR.md) 的 P0–P5 落地。协议**行为**有三处变化：
> 目标水位口径改为总水位（`JITTER_TARGET_MS` 60 → `WATER_TARGET_MS`，含声卡缓冲）、
> 目标值按实机测量定为 **260 ms**（总延迟约 275 ms，见下「实机微调」）、
> 解码节流改由 `rtp::JitterBuffer::pull` 内部按解码缓冲判定。
> 两端必须成对升级：`JitterBuffer::pull` 签名从「播放头时间戳」改为「两级水位」。

**架构：一个时钟、一个观测量、一个执行手段（P4）**
- 删除 `PlayheadClock` 全套（base / 余数累加 / `offset` / `offset_frac` / `note_pcm_correction` /
  `note_ratio_correction`）：不再存在「设备帧 → 媒体时间戳」的换算链，也不再需要记住偏移
- 删除第二级有界回落：`Slip`、`slip_drop` / `slip_insert`、升余弦淡化几何、8 个 `SLIP_*` 常量、
  Kotlin `PcmSlip` 全部移除。重采样比率成为唯一执行手段
- 调度判据全部改为水位：`ring_ms >= DECODE_LIMIT_MS` → 暂停生产；
  `pipe_ms <= PLC_LEAD_MS` → 补 PLC。`played_frames` 不再参与任何播放判定
- 水位口径统一为 `water_ms = jb.water_ms() + ring_ms + device_latency_ms`（P1 的遗留不一致：
  控制目标不含声卡、PLC 截止点却扣了声卡，等效把 60 ms 目标悄悄抬到 70–80 ms）
- 诊断口径重建：`audible_ts = jb.next_ts() − (ring+device frames) × current_ratio()`

**漂移校正：前馈管速率 + 反馈管相位（P3）**
- 新增 `RateEstimator`：`(highest_ts, 到达时刻)` 观测对 → 每 1 s 一个桶只留**最小单向时延**
  （NTP min-filter 同源，排队不影响）→ 30 s 窗最小二乘拟合出钟差 `r̂`（钳 ±2%）与 σ（ppm）。
  桶数不足 6 个不发布估计；`resync`/epoch 变化与 `highest_ts` 同处 `reset()`
- `DriftController` 从「死区 + 线性满幅 + 逐次步进」改为**抗饱和 PI**：
  `PI_KP_PER_MS=0.00005`、`PI_TI_S=10`（≥5 s 才不把抖动积成变调）、`PI_MIN_UPDATE_MS=5`；
  计时改用**声卡已消费帧数**（断流时媒体时间不前进，用它会冻结积分）；低通与限速都按真实 `dt` 缩放
- 合成 `ratio = base_ratio × (1+ff) × (1+Δu)`：频率误差被前馈吃掉，反馈只修相位，
  工作点永远在 0 附近，稳态无静差

**重采样器：两点线性插值 → 多相 FIR（P2）**
- `audio.rs` / `StreamResampler.kt` 改为 **32 相 × 48 抽头 Kaiser 窗 sinc**（`CUTOFF=0.42`、`β=10`，
  每相按 DC 归一），每声道保留 49 样本历史，跨块相位连续
- 删除 `|ratio−1| < 1e-9` 的直通分支：直通与插值两条路径的群延迟不同（≈0.49 ms），
  切换本身就是一次跳变；恒定群延迟无害，且 12 kHz 幅度响应从 −3 dB 抖动降到 <0.1 dB
- 新增验收测试：`resampler_12k_amplitude_is_flat`（±0.005 比率下 12 kHz 幅度波动 <0.1 dB）

**实机微调（P0 采样，2026-10-26）**
- `JitterBuffer::water_ms()` 改用 `highest_ts − next_ts`：旧的「从 `next_ts` 起连续可解帧数」
  在队首乱序时瞬间塌到 0（3.4 万样本中 39 次），每次都会让 PI 误判「缓冲空了」而反向拉满比率
- `RateEstimator` 新增 `RATE_JUMP_MAX_SAMPLES`（2 s）：`highest_ts` 突跳（手机重连/断流补齐）
  而 `epoch` 未必变，旧窗口继续回归会出 ±2000 ppm 假前馈；超限直接整窗作废重算
- `WATER_TARGET_MS` 70 → **260**：网络长期到达率刚好 100 帧/s（发送端无锅），但会成批交付
  （中位 9 帧 = 90 ms）且每隔几十秒**断供 ~200 ms**（断供期只能靠 PLC 顶），而比率排水
  只有 5 ms/s。三轮对比：目标 70 时 PI 永久顶满 +0.5%（+3684 ppm ≈ 常年 6 音分变调）、
  水位 120–306 ms 慢锯、PLC 3–8 次/分；目标 210 时 PLC 1.7 次/分但尖峰仍能到 500 ms；
  目标 260 时水位中位 277 ms、max 328 ms、PLC 0.4 次/分、反馈量中位 −46 ppm、从未贴近钳位
- 环境结论：总延迟里约 230 ms 是本环境（Wi-Fi 省电 + AP 缓冲）必须的抖动缓冲；减小它只能
  在网络侧动（关手机 Wi-Fi 省电 / 5 GHz / USB 网络共享），不要靠压低目标去「赢」延迟

**诊断（P0）**
- 新增 `windows/src/diag.rs`：`UDP2CAL_ASRC_DIAG=<周期 ms>` 时把 `ring_ms/pipe_ms/jb_ms/water_ms/
  adjust_ppm/ff_ppm/sigma_ppm/buckets/played_s/highest_ts/next_ts/plc/underrun` 写入
  `%LOCALAPPDATA%\UDP2CAL\asrc_diag.csv`（最多 300 s），供实机调参用

**日志**
- 10 s 延迟行改为：`[Latency] 总延迟≈Nms（水位 W = 抖动 A + 解码 B，目标 260）声卡 C ms
  比率 ±x.xxx%（反馈 ±a ppm / 前馈 ±b ppm，σ c ppm，d 桶）`
- 30 s 统计行去掉「有界回落」，改为反馈/前馈量与估计器精度

**测试**
- Rust（protocol）56 项、Windows 20 项、Kotlin 50 项全绿；
  新增 PI 闭环仿真（`plant_step`）、估计器合成观测（`feed_estimator`）、调度阈值序断言

---

## 未发布 — 2026-10-25 · 漂移校正换代：受控重采样 + 有界回落

> **已被上一条的 v1.3.0 ASRC 重构取代**：本节引入的「有界回落」（`SLIP_*`、PcmSlip）
> 已整条删除，两点线性插值也已换为多相 FIR。保留本节仅为追溯当时的产品变更
> （码率、预缓冲门槛、发送端节拍整形），这三项仍然有效。
>
> 协议**行为**有三处变化：预缓冲门槛 40 → 30 ms、发送端新增 1 帧节拍整形、
> **码率上限 1000 → 512 kbps**。两端仍需成对升级。
> 规范同步修订：`PROTOCOL_V3_RTP.md` 4.7 / 7.2 / 7.7 / 7.9 / 7.10 / 7.14 / 8.1 / 九。

**码率与带宽（产品变更）**
- **取消「自动分配码率」档**。它的语义只是"用上限"，却额外强制开 VBR、关 VBR 约束，
  对用户来说是不可见的副作用；移除后 VBR 只由 DTX 联动强制
- 码率默认 **256 kbps**；上限 `BITRATE_MAX_KBPS` 1000 → **512**，两端同值
  （接收端用它钳制对端下发的值，跨端不一致会出现"界面显示 512、实际跑 1000"）
- 手动码率从 4 个 Chip（512/768/960/1000）改为**五档滑块**
  `64 / 128 / 256 / 384 / 512 kbps`（`BITRATE_STEPS_KBPS`）
- 新增 `snap_bitrate_kbps()` / `snapBitrateKbps()`：把任意值吸附到最近档位
  （等距取较小档，先钳制后吸附）。`Prefs` 读出时即吸附，历史配置里的
  768/960/1000 自愈为 512，旧的"自动(0)"自愈为默认 256
- 音频带宽从 5 个单选按钮改为**五档滑块**（NB/MB/WB/SWB/FB），版式与码率滑块一致
- 回归测试：`bitrate_clamped_to_512`、`bitrate_snaps_to_manual_steps`（Rust 与 Kotlin 各一套）

**设置即时生效（Android）**
- 根因：`OpusEncoder.update()` 写好了却**从未被调用**，所有编码参数只在编码器创建时读一次，
  改设置必须重开采集才生效（文档里"毫秒级热更新"的说法与实际不符）
- 发送循环每 100 ms 对齐一次 Prefs，参数变化时调 `enc.update()`：
  码率 / 复杂度 / 带宽 / DTX / VBR / VBR 约束 / FEC / 丢包率 / 信号提示，下一帧生效、不断流
- 语音↔音乐、测试音开关无法热改（`AudioRecord` 音源、AEC、`AudioTrack` 语音模式都在
  创建时定型），改为**自动重建采集链路**（复用既有 restart 通道，约 1 s 重连），
  不再需要用户手动停止再开始
- `encoderUpdate` 补 `OPUS_SET_DRED_DURATION(fec ? 50 : 0)`：创建路径有、更新路径漏了，
  导致运行期关 FEC 后 DRED 冗余仍在发（单帧体积不降）
- 编码器构造改为取**当前 Prefs 快照**并与 `appliedCfg` 共用同一份对象：重建链路时
  用户可能刚改过设置，用旧入参会把新设置丢掉
- 状态卡片的「带宽」一行以前恒为 `—`：`CaptureStatus.opusBandwidth` 声明了却
  **从未赋值**。现在运行时显示已生效值、未运行时显示已配置值；并把原来的
  「采样率」标签改成「输入采样率」，与编码带宽明确区分（前者是固定 48 kHz 输入）

**根因**

旧实现把时钟漂移当成"离散事件"处理：水位越过阈值就丢/插 5 ms，间隔 500 ms。
三个后果无法回避——每次编辑都是一次音频删除/重复，5 ms 的接缝只在尾部淡出 1 ms、
其余 4 ms 是硬切；它只在水位**已经偏离之后**才动作，水位长期在 120–180 ms 徘徊；
低水位一侧又因为"PCM 已提前写进声卡"而误判，形成永久 1% 的削/补。
听感就是持续细微的"咔嗒"。

**P1：受控重采样（主力）**
- 新增 `DriftController`：水位偏差经 150 ms 低通后映射到重采样比率的 ±0.5%，
  每 10 ms 更新、单次步进 ≤0.0001（1%/s 斜坡），死区 ±5 ms、满幅点 ±20 ms
- `AudioWriter` 的重采样比率改为 `源/设备 × (1 + adjust)`，相位跨块连续
- 新增 `PlayheadClock::note_ratio_correction`：比率造成的媒体时间偏移是连续量
  （u=0.5% 时每帧只有 2.4 个样本），用 f64 累积、只把整数部分入账；
  逐帧取整会吃掉近 17% 的校正量，表现为水位与播放头缓慢背道而驰
- Android 侧镜像实现 `StreamResampler`（交错立体声、相位连续）

**P0：有界回落（安全网）**
- 高带：水位 > 目标 +30 ms 且连续 5 次评估都在带内 → 丢 1 ms
- 低带：水位 < 目标 −10 ms → 插 1 ms
- 两次之间至少间隔 1 s 音频样本时间（每秒最多 1 ms），绝不连续触发；
  启动宽限 2 s；只在真实 PCM 帧上做
- 新增 `slip_drop` / `slip_insert`：48 样本升余弦交叉淡化，接缝两侧一阶连续。
  旧的"尾部淡出"被删除——淡出到零再接下一帧本身就是一个台阶
- 回落方向反了会被测出来：`drift_slip_low_band_inserts` /
  `drift_slip_rate_is_one_ms_per_second` / `slip_drop_is_crossfaded_not_cut`

**播放头与延迟**
- 播放头改用**可听位置**：`audible = played_frames − device_latency_frames()`。
  回调把 PCM 填进 WASAPI 缓冲时 `played` 就前移了，这段数据还要过引擎缓冲 + DMA
- `device_latency_us` 从 `OutputCallbackInfo::timestamp()` 的 `playback − callback` 实时读取
- 抖动缓冲新增 128 条到达记录，「总延迟估算」= `now − arrival(playhead_ts) + 声卡延迟`
- 状态栏显示 `延迟 N ms`；日志每 10 s 打印构成分解；新增
  [LATENCY_CALIBRATION.md](LATENCY_CALIBRATION.md) 说明如何实测校准一次

**P2：发送端与门槛**
- 发送端 1 帧节拍整形（Android `sendFrames` / Windows `reverse-capture`）：
  发送时刻钉在 10 ms 网格，落后 >4 帧重置而非补发；测试音循环原先自带一套节拍，
  与新整形叠加会导致半速，已删除
- `JITTER_START_MS` 40 → 30 ms（3 帧）：前提是水位由比率闭环维持，不再需要旧模型的安全垫
- Windows 输出显式请求 10 ms 缓冲（`BufferSize::Fixed`），驱动拒绝时回落默认值；
  cpal 0.15.3 的 `buffer_size_to_duration` 有 100× 换算误差，请求值不可信，
  因此延迟一律以时间戳实测为准
- Android AudioTrack 缓冲 80 ms → 2 帧（20 ms），`PERFORMANCE_MODE_LOW_LATENCY` 常态化
  （minSdk 29），反向路径接入重采样

---

## 未发布 — 2026-10-25 · 第三轮质量审查修复（22 项）

> 审查报告：[CODE_REVIEW_2026-10-25.md](archive/CODE_REVIEW_2026-10-25.md)。
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
