# UDP2CAL 全流程代码审查报告（第三轮）

- 日期：2026-10-27
- 基线提交：`13d7254`（main，"docs: 审查报告追加附录——F-10 格式化已执行、T-05 不加 CI"）
- 工作分支：`review/fixes-2026-05`（20 个独立提交，每个提交一个逻辑变更）
- 审查范围：`protocol/`（`protocol.rs`、`rtp.rs`、`jni.rs`）、`windows/`（`main.rs`、`audio.rs`、
  `capture.rs`、`decoder.rs`、`config.rs`、`diag.rs`、`firewall.rs`、`logger.rs`、`build.rs`、
  `tests/`）、`android/app/src`（跨实现一致性 + 生命周期）
- 排除范围：`vendor/`（上游 audiopus 补丁副本）、`opus-src/`、`scripts/`、`target/`、生成物
- 工具链：rustc/cargo 1.99.1、rustfmt 1.10.0、clippy 0.1.99、cargo-audit；Gradle 8.9 + JDK 17
  （`ANDROID_HOME` 未设置，`android/local.properties` 有 `sdk.dir`）
- 上一轮报告：`docs/archive/CODE_REVIEW_2026-10-27.md`（F-01…F-11 已修，T-01…T-08 已知限制）

---

## 1. 阶段 0：环境探测与基线（修改前实测）

| 检查项 | protocol | windows |
|---|---|---|
| `cargo fmt -- --check` | 通过（exit 0） | 通过（exit 0） |
| `cargo clippy --all-targets --all-features -- -D warnings` | 0 警告 | 0 警告 |
| `cargo test --all-features` | 75 passed / 0 failed / doctest 0 | 21 unit + 2 integration passed / 0 failed |
| `cargo audit` | —（仅 jni-sys） | 0 vulnerabilities，"9 allowed warnings"（T-03 已登记项） |
| `cargo tree -d` | 见 §7 | 见 §7 |

项目画像：两个**平级 crate（非 workspace）**；`protocol` 是 `rlib+cdylib` 库（JNI 由 Android 调用），
`windows` 是 `iced` GUI 二进制；异步用 tokio（单 runtime，网络/UI 协程），音频路径是原生线程 +
SPSC 无锁环；Win32 FFI（WASAPI/注册表/firewall/单实例互斥）；release profile
`lto=true, codegen-units=1, panic="unwind", strip="symbols"`。

**准出门禁结论：基线绿色（编译/测试/clippy 全通过），允许做功能性修改。**

不可用工具（对应门禁降级，已在 §8 如实标注）：`cargo miri`（Windows 目标 + Win32 FFI，miri 无法运行）、
`loom`/`proptest`/`criterion`/`cargo semver-checks`/`cargo deny`/`cargo machete`（未安装）、
真机（无 Android 设备）与音频端点热插拔（无受控硬件环境）。

---

## 2. 发现的问题（总数与分级）

统计口径：以**独立缺陷**计数（同一根因的多处代码算 1 项）。行号为基线 `13d7254` 时的位置。

| 级别 | 数量 | 已修复 | 未修复/待确认 |
|---|---|---|---|
| P0（内存不安全 / 进程 abort / 静默数据损坏） | 1 | 1 | 0 |
| P1（可用性崩溃 / 功能失效 / 锁内阻塞关键路径） | 5 | 5 | 0 |
| P2（资源泄漏、错误静默、语义偏差、加固缺失） | 17 | 11 | 6 |
| P3（冗余、可读性、无依据微优化） | 23 | 3 | 20 |
| 待确认（证据不足/需产品决策） | 5 | 0 | 5 |

---

## 3. 计划表与执行状态（全部条目）

风险等级按任务书 L0–L4；`L1` 内部修复、`L2` 语义/并发/错误处理、`L3` 公共 API/unsafe/序列化。

| 优先级 | 位置 | 问题（含证据） | 类别 | 风险 | 影响范围 | 修改方案 | 验证方式 | 决策 | 状态 |
|---|---|---|---|---|---|---|---|---|---|
| P0 | android `CaptureService.kt:1354-1372` | `onDestroy` 只 join `oldPlay/oldCapture`，唯一调用原生 `jbInsert` 的 `reverseRxJob` 不在 join 集合 → `jitter.close()`（`jni.rs:787` `drop(Box::from_raw)`）与仍在执行的 `jbInsert` 竞争 = 原生 UAF；`withTimeoutOrNull(1000)` 超时同样在协程存活时释放 | 并发/内存安全 | L2 | 服务停止路径 | join 补 `oldRx`（顺序：capture→rx→play，前者收尾关 socket 才能唤醒阻塞 `receive`）；join 失败则不归还句柄 | Kotlin 编译 + 单测 | 直接修 | ✅ `eca6f8f` |
| P1 | `windows/src/capture.rs:686-704` | `StereoResampler::process` 的 `drain(0..consumed*2)` 末端越界：退出条件允许 `phase` 越过 `frames`，ratio>2（≥144 kHz 回环端点）必 panic → 捕获线程死亡、反向永久静音 | 越界/panic | L1 | 反向音频 | `consumed = (phase as usize).min(frames - 1)` | 新增定向测试（旧码 FAILED） | 直接修 | ✅ `09f4f3c` |
| P1 | `windows/src/audio.rs:142-148` | `Ring::apply_pending_flush` 只夹上界 `target.min(head)`，消费者已越过 target 时把 tail 写回更早位置 → 重播 ≤10 ms 已播样本 + `len()` 虚高（`main.rs:440`/`:459` 可达） | 逻辑错误 | L1 | 播放时间线 | tail 只允许前移：`cur.max(target.min(head))` | 新增定向测试（旧码 FAILED） | 直接修 | ✅ `6d69b05` |
| P1 | `windows/src/decoder.rs:71-80` | `conceal()` 用 libopus 内部 `frame_size`（来自"上次被解析"而非"被接受"的包）；20/40/60 ms 包被 `decode()` 拒绝后仍改了 `frame_size` → PLC 产出 960–5760 样本，而调用方按一帧 10 ms 入环，`pull()` 只推进 480 → 环被多灌、延迟棘轮 | 逻辑错误 | L1 | 丢包恢复 | 恒定返回 480，不足补静音 | 新增定向测试（旧码实测 5760） | 直接修 | ✅ `22eba9e` |
| P1 | `windows/src/main.rs:757-763`、`:1826-1837` | 持 `REVERSE_SENDER` 槽位互斥锁执行 `old.stop()`（内部 `join()`）；`reverse_sender_alive()` 需同一把锁 → 控制面串在线程 join 后面；两处调用点在唯一 async 接收任务与 UI 线程 | 并发/死锁风险 | L2 | 控制面、UI | 槽位 take 后放锁再 stop；新增 `REVERSE_RESTART` 串行化跨线程重启 | 全量 test + 代码审视 | 直接修 | ✅ `848b0d8` |
| P1 | `protocol/jni.rs:429-444` | `buildControl` 不校验 `payload_len`/`msg_type`：>512 时 debug 下 `debug_assert` panic 逃出 `extern "system"` = 进程 abort；release 下静默截断并返回"看着合法"的坏包 | FFI/panic | L1 | Android 全部控制包 | 抽 `build_control_args_ok()` 守卫，非法返回 null | 新增单测（旧行为 FAILED） | 直接修 | ✅ `df91afd` |
| P2 | `windows/src/capture.rs:810-811` | `GetService().ok()?`/`Start().ok()?` 在 `Self` 构造前提前返回 → `CreateEventW`（`:772`）句柄泄漏；`run_loop` 每 500 ms 重试 = 2 句柄/秒无界增长 | 资源泄漏 | L1 | 环回端点异常 | 显式 match，失败分支 `CloseHandle` + 日志 | 编译 + 代码审视 | 直接修 | ✅ `4c09280` |
| P2 | `windows/src/capture.rs:925-975` | `parse_mix_format` 未校验 `wf.nBlockAlign`，而读取侧按 `frame_count*channels` 造切片 → 偏小越界读（UB）、偏大读到相邻帧 | 内存安全/数据 | L1 | 环回采集 | 校验 `nBlockAlign == channels*bytes_per_sample` | 新增定向测试 | 直接修 | ✅ `dcf0ea5` |
| P2 | `windows/src/capture.rs:864-885` | `frames == 0` 分支是唯一不调用 `ReleaseBuffer` 的读取路径（`AUDCLNT_S_NO_CURRENT_DATA` 也走这里），破坏 WASAPI Get/Release 配对契约 | 资源泄漏 | L1 | 环回采集 | 补 `ReleaseBuffer(frames)` | 编译 + 代码审视 | 直接修 | ✅ `31ef5dc` |
| P2 | `windows/src/capture.rs:180-196` | 持 `reverse_config()` 互斥锁写日志（同步文件 I/O），发送线程每 500 ms 抢同一把锁 → 磁盘慢时反向 RTP 断流 | 锁粒度 | L1 | 反向发送 | 锁内只比较+赋值，`logf!` 移到锁外 | 全量 test + 代码审视 | 直接修 | ✅ `178685c` |
| P2 | `windows/src/main.rs:468-469` | `device_rate as u64` 作除数未 `.max(1)`（`audio.rs:246/267` 都做了）→ 0 时解码线程除零 panic | 除零 | L1 | 解码线程 | `.max(1)` | 编译 + 代码审视 | 直接修 | ✅ `8e03fbb` |
| P2 | 全仓 | 无 `set_hook`/`catch_unwind`；release 是 GUI 子系统、stderr 被丢弃 → 线程 panic 后 `APP_RUNNING` 仍为 1、DISCOVER_REQ 照旧回 ACK（幽灵设备），日志无记录 | 可观测性 | L1 | 全部后台线程 | `logger::install_panic_hook()`（try_lock 防重入） | 编译 + 代码审视 | 直接修 | ✅ `02d5bfb` |
| P2 | `windows/src/main.rs:362-364` | `let _ = Builder::spawn(...)` 吞掉线程创建失败 → `AudioInitState` 停在 Pending，超时后照常启动引擎：UI 显示运行中、永远没有声音 | 错误静默 | L1 | 启动路径 | 检查返回值，失败落日志 + `DeviceFailed` | 编译 + 代码审视 | 直接修 | ✅ `8c974e8` |
| P2 | `windows/src/config.rs:105` | 注册表 `listen_port` 无区间校验，下游三处 `as u16` 截断（`main.rs:730/820/934`）→ 广播/防火墙用回绕端口而 bind 用原值失败 | 整数/数据一致性 | L1 | 配置加载 | `sanitize_port()` 归一 + 常量 `DEFAULT_LISTEN_PORT` | 新增单测 | 直接修 | ✅ `883ea11` |
| P2 | `windows/src/main.rs:1068/1086/1136` | 三处 `let _ = self.config.save()` 吞掉注册表写入失败（T-02）→ 设置看似生效、重启回退、无日志 | 错误静默 | L1 | 配置持久化 | `AppState::persist_config()`：日志 + 界面提示 | fmt/clippy/test | 直接修 | ✅ `06cc793` |
| P2 | `windows/src/logger.rs:37-44` | 轮转失败时把 `bytes` 清零却不截断文件 → 计数与真实大小脱钩，日志随后无上限增长，与模块承诺的 2 MiB 上限矛盾 | 资源/错误处理 | L1 | 日志 | 失败时原地截断（Windows 追加句柄不能 `set_len`，需换句柄） | 新增 2 个单测（`set_len` 方案实测 FAILED） | 直接修 | ✅ `be81b0f` |
| P2 | `protocol/jni.rs:146-150` | `ensure_payload_capacity` 只增不减：一次超大 `jbInsert` 永久抬高该 handle 的缓冲（真实包 ≤ `OPUS_MAX_PAYLOAD`，多出部分无用） | 内存 | L1 | 抖动缓冲句柄 | 双向归一 + 超额容量 `shrink_to` | 新增单测（旧实现 FAILED） | 直接修 | ✅ `614db6f` |
| P3 | `windows/src/main.rs:2053` | `format!("{}:{}")` 拼出 `::1:44044` → IPv6 监听地址必然绑定失败，错误只报"端口可能被占用" | 功能 Bug | L1 | 网络绑定 | `bind((ip, port as u16))` | fmt/clippy/test | 直接修 | ✅ `1f79765` |
| P3 | `protocol/protocol.rs:80` vs `:362` | `OPUS_FLAGS_RESERVED_MASK` 定义后全仓无引用（含 JNI/Kotlin），`validate` 硬编码 `!0x0f` → 同一语义两份真相 | 冗余/魔数 | L0 | 协议校验 | 引用常量（不删 `pub` 项，避免破坏 API） | 既有用例覆盖 + 全量 test | 直接修 | ✅ `2a811bd` |
| P3 | `windows/src/main.rs:2132-2140` | `if packet_count > 0 {…} else {…}` 两分支等价；合并后 `packet_count` 变成无人读取 | 死代码 | L0 | 码率统计 | 删除计数器与分支 | clippy `-D warnings` 自证（先合并即报 unused） | 直接修 | ✅ `f678acc` |

### 未修复 / 待确认条目

| 编号 | 位置 | 问题 | 为什么没改 | 建议 |
|---|---|---|---|---|
| U-01 | `windows/src/audio.rs:619`、`:726-728` | 输出流错误回调 `|_| {}` 全吞；无退出条件的 `loop { park() }`；`AudioWriter` 无 `Drop` → 流错误后 UI 仍显示可用，重入 `start_audio`（`main.rs:372`）会再起一条流/线程 | 修复需要"流错误 → iced Message → UI 状态机 + 线程退出协议"的跨层改造（L2 设计变更），且需真实音频设备热插拔才能验证 | 下一轮：加 `AtomicBool` 健康标志 + 错误回调写入 + `AudioWriter::Drop` |
| U-02 | `windows/src/main.rs:1950-1952` | 对端每次 CONNECT 改 `reverse_port` 都触发完整 WASAPI 重启，无限流/无冷却（已绑定对端即可控） | 需要产品决策：多长间隔算抖包 | 建议 ≥1 s 冷却 + 端点抖动时只重建 socket 不重建端点 |
| U-03 | `protocol/jni.rs`（83 个导出） | 无 `catch_unwind` panic 边界 | 83 处包装属大改；且会改变错误呈现（null vs abort）语义，L3 | 至少在 5 个 handle 创建 + `jb*` 热点导出上加边界 |
| U-04 | `protocol/jni.rs:731/738/745/752/759` | `Handle` 是无标签 `jlong`，`(h as *const T).as_ref()` 无类型/对齐校验；跨类型 drop 会堆破坏 | 引入 magic word/注册表属 L3，且要同步 Kotlin 侧约定 | 加 `debug_assert` magic 头（release 保持零成本） |
| U-05 | `protocol/jni.rs:883/927`、`:1307-1315`、`:1296`、`:1407` | 哨兵二义：`jbPull` 无效句柄返回 0 == `Pull::WAIT`；`reSigmaPpm`/`deSigmaPpm` 用 0.0 表示错误而 0.0 是合法读数（`reFeedforward` 用 NaN，不一致） | 改哨兵值改变 Kotlin 侧语义，L3 | 统一用 NaN 哨兵并同步 Kotlin |
| U-06 | `protocol/jni.rs:130-139`、`:222`、`:659`、`:719-726` | `read_device_id` 只拒"太短"（16 字节也通过，与文档不符）；`new_string` 注释错误；`writeRtpHeader` 返回值被 Kotlin 丢弃；`lock` 标 `unsafe fn` 但体内无 unsafe 操作 | 纯注释/文档级，价值低 | 下轮批量更正 |
| U-07 | Kotlin `Udp2CalProtocol.kt:307/412/453/492/536`、`CaptureService.kt:1360`、`AudioPlayer.kt:155-163`、`UdpSender.kt:226-241` | `handle` 非 volatile 但多线程读写；`runBlocking` 在 `onDestroy` 阻塞主线程 ≤1 s（ANR 风险）；`AudioPlayer` 在 `trackLock` 外替换 track；`UdpSender.close()` 超时后旧 ACK 线程仍活而我重连可起第二个消费者 | 需要真机验证与线程模型设计（本轮已修其中同根因的 P0 部分） | 单独一轮 Android 并发加固 |
| U-08 | Kotlin `Udp2CalProtocol.kt:161-162`、`Prefs.kt:42-44/61-63`、`DiscoveryManager.kt:81-89` | `ConnectPayload.isValid()` 只有 8 项中的 3 项；缺 `OPUS_FLAGS_RESERVED_MASK`/`OPUS_COMPLEXITY_MAX`；`opusComplexity` getter 不钳制（存 11 则永久连不上）；`opusBitrateKbps` 把 0 映射成 256；JNI 失败伪造成全零 CONNECT；`is_valid_device_id` 无 JNI 导出 | 跨三实现（Rust/JNI/Kotlin）的一致性变更，需产品确认（尤其"自动码率=0"是否回归），L2/L3 | 用 `constants()` 导出补齐常量 + 校验原因串导出 |
| U-09 | `protocol/rtp.rs:938-1080`、`:148`、`:112` | `StepAbsorber::last_water_ms` 写而不读；`RATIO_UPDATE_SAMPLES` 全仓无引用（`pub`）；`STEP_TRIM_GROWTH_MS` 仅 JNI 导出给 Kotlin | 删 `pub` 常量 = SemVer 破坏；私有字段删除收益极低 | 与 Android 侧常量对齐时一并处理 |
| U-10 | `windows/src/*.rs` | 无 `#![deny(unsafe_code)]`（T-08），unsafe 未隔离到专用模块 | 需逐模块确认 unsafe 必要性 | 至少给 `audio/capture/main` 之外的新模块加 `forbid` |
| U-11 | `windows/src/capture.rs` 间隙补偿 | `if gap_ms > FRAME_MS*2` 的 20 ms 死区使短丢包后媒体时间系统性短缺 | 可能是有意迟滞，证据不足 | 待确认后决定是否改为按丢包数补偿 |
| U-12 | `android/.../UdpSender.kt:117-121` | 每帧新建 `DatagramPacket`（100/s） | **无 profile 依据不做微优化**（任务书 §4 要求） | 需要基于真机 trace 的优化 |

### 待确认清单

| 编号 | 问题 | 无法确认的原因 | 建议调查路径 |
|---|---|---|---|
| T3-01 | Android `UdpSender.kt:27-40,202-205` 要求 ACK 源 **IP 字符串** 等于配置 IP 且源端口等于配置端口，而 Windows 绑 `0.0.0.0` 并从同一 socket 回 ACK → 多网卡主机源 IP 可能是另一网卡、或呈 `::ffff:192.168.1.5` → ACK 全丢，手机永远"等待连接" | 需要双网卡真机 + 抓包 | 抓包确认 ACK 源端点；Rust 侧改为按设备 ID 校验而非源 IP 字符串 |
| T3-02 | `reverseRxLoop`（`CaptureService.kt:949`）只比来源 IP 不比端口（Windows 侧 `is_bound_peer_source` 比完整端点） | 加固不对称是否有意（NAT/端口重映射场景）未知 | 与 T3-01 一起定策略 |
| T3-03 | `AudioPlayer.start()`（`AudioPlayer.kt:81-82`）不重置 `writtenFrames`（`stop()`/`flush()` 重置）→ 重建后 `pendingFrames()` 瞬时虚高，一次 trim 突发 | 需要真机音频切换复现 | 与 `stop()` 对齐补 `writtenFrames = 0` |
| T3-04 | `writeOrRecover`（`CaptureService.kt:995-1012`）在协程已取消后仍 `player.recover()` 重建 AudioTrack | 取消语义与设备行为强相关 | 在 `recover()` 前补 `ensureActive()` |
| T3-05 | `audio.rs:127` `len()` 用 `saturating_sub` 而 push/pop 用 `wrapping_sub` | 仅 32 位下 24.8 小时后可达，x64 不可达，无法实测 | 标记注释即可 |

---

## 4. 已修改明细（每项：改了什么 / 为什么安全 / 验证证据）

统一门禁：每个提交都跑 `cargo fmt -- --check` + `cargo clippy --all-targets --all-features -- -D warnings`
+ `cargo test --all-features`（所在 crate）；Kotlin 改动跑 `./gradlew --offline :app:compileDebugKotlin`
与 `:app:testDebugUnitTest`（JDK 17）。

1. **`eca6f8f`（P0，Android）** `onDestroy` join 集合补 `reverseRxJob`，顺序 `capture→rx→play`；
   join 未在 1 s 内完成则**不**释放原生句柄（宁可泄漏不与仍在执行的原生调用竞争）。
   安全理由：句柄的归还条件从"时间到"变为"所有使用者已退出"；顺序依据是 `captureJob` 收尾里的
   `udpSender.close()` 才能唤醒阻塞在 `receive()` 上的接收协程。
   证据：`:app:compileDebugKotlin` 通过；`:app:testDebugUnitTest` BUILD SUCCESSFUL（含 JNI 一致性用例）。
   未执行：真机停止服务（无设备）。
2. **`09f4f3c`（P1）** `consumed = (phase as usize).min(frames - 1)`。
   证据：新测试 `resampler_high_ratio_never_overruns` 覆盖 144k/176.4k/192k/384k × 三组帧长；
   还原旧码后 FAILED（`core/src/slice/index.rs:1009` panic），修复后 ok。
3. **`6d69b05`（P1）** `tail` 只前移。证据：新测试 `flush_never_rewinds_consumer_position`；
   旧码 FAILED（"flush 使 tail 回退：12 → 8"）。不变量：`tail` 仅消费者写，`cur <= head` 保持。
4. **`22eba9e`（P1）** `conceal()` 恒返回 480（不足补 0）。证据：新测试用 audiopus Encoder 生成
   20/40/60 ms 包 → `decode()` 拒绝 → `conceal()` 必须 ==480；旧码实测 `Some(5760)`。
5. **`848b0d8`（P1）** 槽位 take 后放锁再 `stop()`；新增 `REVERSE_RESTART` 维持"同时只有一个发送线程"。
   证据：全量 test + 代码审视。**残留**：`join()` 仍在调用线程执行，阻塞时长未缩短（已记入提交信息）。
6. **`df91afd`（P1）** `build_control_args_ok()`；只拦 `0..=255` 之外的 `msg_type`（不按已知类型白名单，
   否则 Kotlin `TYPE_DATA=0` 与测试里传 9 会被误伤）。证据：新单测；退化守卫后 FAILED。
7. **`4c09280` / `dcf0ea5` / `31ef5dc` / `178685c`（P2，capture.rs）** 句柄泄漏、`nBlockAlign` 校验、
   `ReleaseBuffer` 配对、锁内日志。证据：`parse_mix_format` 新旧行为各有用例（8 通过 / 4 与 16 拒绝）；
   其余为编译 + 代码审视（`ReleaseBuffer` 与句柄泄漏路径需真实端点异常才能触发）。
8. **`8e03fbb` / `8c974e8` / `02d5bfb`（P2，main.rs + logger.rs）** 除零兜底、spawn 失败上报、panic hook。
   证据：fmt/clippy/test；失败路径在正常环境下不可达（描述为"编译 + 代码审视级"）。
9. **`883ea11`（P2，config.rs）** `sanitize_port()` + 常量。证据：新单测（1/80/44044/65535 保留；
   0/65536/70000/u32::MAX → 44044）。
10. **`06cc793`（P2，main.rs）** `persist_config()`。证据：fmt/clippy/test。
11. **`be81b0f`（P2，logger.rs）** 轮转失败原地截断。证据：2 个新单测；`set_len(0)` 首版实测 FAILED
    （文件保留 14 字节），证明 Windows 追加句柄不能截断，改为换句柄。
12. **`614db6f`（P2，jni.rs）** 复用缓冲双向归一。证据：新单测；旧实现 FAILED。
13. **`1f79765` / `2a811bd` / `f678acc`（P3）** IPv6 绑定、常量去重、死计数器删除。
    证据：fmt/clippy/test；`f678acc` 的"死"由 clippy `unused_assignments` 自证。

---

## 5. unsafe 审计结果（逐块四分类）

`protocol` crate 根有 `#![deny(unsafe_code)]`（`protocol.rs:26`），唯一例外是 `jni.rs`
（`#![allow(unsafe_code)]`）。`windows` crate 无 crate 级 `deny`（T-08，见 U-10）。

| 区域 | 分类①合法且注释完备 | ②合法但缺注释 | ③存疑 | ④确认 UB |
|---|---|---|---|---|
| `protocol/jni.rs`（17 个内部 `unsafe fn` + 83 个导出：60 手写 + 23 由 `jb_scalar!` 生成） | 绝大多数（模块头 4 条契约 `jni.rs:33-52` 成立） | `lock`（`:719-726`）标记 `unsafe` 但体内无 unsafe 操作；导出函数普遍无 panic 边界注释（U-03） | `*_ref` 的 `(h as *const T).as_ref()` 无类型/对齐校验（U-04）——当前所有使用都在调用作用域内，可证安全 | 0 |
| `windows/src/main.rs`（Win32：`CreateMutexW`/`EnumWindows`/`GetLastError` 等） | 是（`CreateMutexW` 后立即读 `GetLastError`，无中间 Win32 调用；mutex 句柄有意泄漏；无别名/生命周期问题） | — | — | 0 |
| `windows/src/capture.rs`（WASAPI） | 是（`ComGuard` 配对 `CoInitializeEx`/`CoUninitialize`；`GetBuffer`/`ReleaseBuffer` 本轮补齐最后一处不配对；null 指针已挡；SILENT 分支不 deref） | — | `nBlockAlign` 未校验导致按 `channels` 造切片越界读（本轮已修 `dcf0ea5`） | 0（修复前为潜在 UB，未被触发的输入构造验证过） |
| `windows/src/audio.rs`（cpal/SPSC 环） | 是（`push/pop` 内存序正确：data Relaxed + head/tail Release/Acquire；容量全可用无保留槽；无 `unsafe impl Send/Sync`） | — | — | 0 |
| `windows/src/firewall.rs` | 是（参数以 argv 传递，不经 shell） | — | — | 0 |

工具说明：`cargo miri` 在本环境**不可用**（Windows 目标 + Win32 FFI 不受支持），
`clippy::undocumented_unsafe_blocks` 未纳入 CI。因此 unsafe 结论来自逐块人工审计 + 定向测试，
不是 miri 证明——**如实标注为未执行**。

---

## 6. 性能优化结果

**本轮未做性能改动，也未运行基准。** 原因：任务书要求性能优化必须有 profiling / criterion 依据，
本轮所有改动均为正确性/鲁棒性修复（越界、重播、锁内阻塞、错误静默、资源泄漏），
没有以"更快"为目标的改动。明确拒绝的一项无依据微优化：`UdpSender` 每帧新建 `DatagramPacket`（U-12）。

已识别但未执行（需基准）的性能项：Opus 解码在 jitter 锁内（`main.rs:483` 包住 `:497-513`）、
`logf!` 从 async 接收循环调用（`:2078`、`:1858/1865/1879`）、`Ring` 的 `len()` 计算方式。

---

## 7. 安全检查结果

| 检查项 | 结论 |
|---|---|
| 注入（SQL/命令） | 无 SQL；`firewall.rs` 用 argv 而非 shell 拼接——通过 |
| 路径遍历 | 无用户可控文件路径（日志/配置路径来自 `LOCALAPPDATA`）——通过 |
| 反序列化 DoS | 线格式为定长二进制 + 显式长度校验（`parse_control` 要求 `15+payload_len` 精确、`demux` 有上下界、jitter `insert` 复校长度）——通过 |
| 网络输入 panic/越界 | 三方审计一致：不可信 UDP 输入构造不出 panic/死锁/UAF/越界（`pull` 在 `out.len()<len` 返回 Wait；`pcm_mut` 钳位）——通过 |
| 机密/密码学 | 无密钥/TLS/口令逻辑；日志不含敏感字段——通过 |
| 供应链 | `cargo audit`：0 vulnerabilities，9 allowed warnings（T-03 已知项，RUSTSEC-2026-0253/0186/0386 + `clipboard-win` yanked，均需 major 升级，按红线不自动执行）；两个 `Cargo.lock` 均已提交；`build.rs`/vendored `audiopus*` 为既有点，本轮未新增依赖——通过（维持现状） |
| FFI 边界 panic | 修复了 `buildControl`（唯一的可达 panic 径路，`df91afd`）；其余导出仍无 `catch_unwind`（U-03） |
| 敏感路径日志 | 本轮新增日志只含错误码/路径，无用户内容 |
| 新引入 unsafe | **0**（新增代码零 unsafe，`protocol` 保持 `deny(unsafe_code)`） |

---

## 8. 测试 / 构建 / clippy 结果（基线 vs 终态）

| 指标 | 基线 `13d7254` | 终态（本分支 HEAD） |
|---|---|---|
| protocol `cargo fmt --check` | 通过 | 通过 |
| protocol clippy 警告数 | 0 | 0 |
| protocol tests | 75 passed / 0 failed | **77 passed / 0 failed**（+2：`build_control_args_reject_out_of_range`、`payload_scratch_is_normalized_both_ways`）；doctest 0 |
| protocol `cargo check --no-default-features` | 通过 | 通过 |
| windows `cargo fmt --check` | 通过 | 通过 |
| windows clippy 警告数 | 0 | 0 |
| windows tests | 21 unit + 2 integration / 0 failed | **29 unit + 2 integration / 0 failed**（+8：resampler、flush、conceal×2、block_align、sanitize_port、logger rotate×2） |
| windows `cargo build --release` | 未记录 | **通过**（2m25s，lto 全量重建） |
| Android `:app:compileDebugKotlin` | 未记录 | 通过（JDK 17） |
| Android `:app:testDebugUnitTest` | 未记录 | **BUILD SUCCESSFUL** |
| `cargo audit` | 0 vulns / 9 allowed warnings | 0 vulns / 9 allowed warnings（无新增） |
| 既有失败新增 | — | 无 |

回归测试原则遵守情况：**没有修改任何既有断言、没有删除任何测试、没有接受快照**；
新增测试均为"失败即复现缺陷"的定向用例（并在提交信息里记录了旧实现的实际失败输出）。

未执行的门禁（环境不支持，如实标注）：`cargo miri`、`loom`、`proptest`、`criterion`、
`cargo semver-checks`、`cargo deny`、`cargo machete`、真机/端点热插拔测试。

---

## 9. 仍然存在的风险

1. **T3-01（高）** Android 对 ACK 源 IP 的字符串严格匹配 + Windows 绑 `0.0.0.0`：多网卡主机可能
   "连不上且无诊断"。这是本轮唯一可能造成"完全不可用"的未修项。
2. **U-01（中）** 输出音频流错误被吞、`AudioWriter` 无 `Drop`：设备被抢占/拔出后 UI 仍显示运行中。
3. **U-02（中）** 已绑定对端可反复触发 WASAPI 端点重建（本地资源抖动）。
4. **U-03/U-04（中）** JNI 导出无 panic 边界、句柄无类型标签：一旦触发即 abort 或堆破坏。
5. **U-07（中）** Kotlin `runBlocking` 在 `onDestroy` 阻塞主线程 1 s（ANR）与 `handle` 非 volatile。
6. **U-08（中）** 三端校验强度不一致 → 手机侧"永远等待连接"且无原因；`opusComplexity` 越界值会永久连不上。
7. **T-03（低，继承）** 9 条 audit 告警需 major 升级（tokio/memmap2 等），本轮按红线未动。

---

## 10. 后续建议（按优先级）

1. 先解决 **T3-01**：把 ACK 校验改成"设备 ID + 端点首见锁定"而不是"源 IP 字符串相等"；
   同时导出 `ConnectPayload::validate()` 的原因串给 Android 显示。
2. **U-03/U-04**：给 5 个句柄创建 + `jb*` 热点导出加 `catch_unwind` 与 magic 头（release 零成本）。
3. **U-01**：音频流健康状态上报 + `AudioWriter::Drop`，让"没声音"可被 UI 与日志看见。
4. **U-02**：反向端点重建加 ≥1 s 冷却与去抖。
5. **U-07**：Android 并发加固（`@Volatile` handle、`onDestroy` 不阻塞主线程）。
6. **U-11/T3-03/T3-04**：补齐后按真机回归。
7. 工程化：加 CI（`fmt --check` + `clippy -D warnings` + `test` + `cargo audit --locked`）、
   `rust-version`/`resolver` 显式化（T-04/T-05 仍未做，属产品决策）、把 `undocumented_unsafe_blocks`
   与 `-D warnings` 一起纳入。

---

## 11. 回滚指引（批次 ↔ 提交）

分支 `review/fixes-2026-05` 相对 `main` 的 20 个提交，**每个都可单独 `git revert`**；
互相独立的依赖只有两处（见下）。

| 批次 | 提交 | 内容 | 依赖 |
|---|---|---|---|
| A | `09f4f3c` | capture.rs 重采样越界 | 无 |
| B | `6d69b05` | audio.rs flush 只前移 | 无 |
| C | `22eba9e` | decoder.rs PLC 恒 480 | 无 |
| D | `4c09280` | capture.rs 事件句柄泄漏 | 无 |
| E | `dcf0ea5` | capture.rs nBlockAlign 校验 | 无 |
| F | `31ef5dc` | capture.rs 0 帧 ReleaseBuffer | 无 |
| G | `178685c` | capture.rs 锁外日志 | 无 |
| H | `848b0d8` | main.rs 反向 stop 移出锁 | 无 |
| I | `8e03fbb` | main.rs device_rate 兜底 | 无 |
| J | `02d5bfb` | logger.rs panic hook | 无 |
| K | `8c974e8` | main.rs spawn 失败上报 | 无 |
| L | `883ea11` | config.rs 端口归一 | **M 的注释引用它**（回滚 L 需同时调整 M 的注释） |
| M | `1f79765` | main.rs IPv6 绑定 | 依赖 L 的端口合法性保证（`as u16` 安全论证） |
| N | `2a811bd` | protocol.rs 常量去重 | 无 |
| O | `df91afd` | jni.rs buildControl 入参校验 | 无 |
| P | `614db6f` | jni.rs 缓冲归一 | 无 |
| Q | `06cc793` | main.rs 配置保存失败上报 | 无 |
| R | `be81b0f` | logger.rs 轮转兜底 | 无 |
| S | `f678acc` | main.rs 死计数器删除 | 无 |
| T | `eca6f8f` | Android onDestroy join 顺序 | 无 |

整体回滚：`git checkout main` 即可（`main` 未被修改）。部分回滚示例：
`git revert --no-commit be81b0f && git commit`（只撤销 R）。

---

## 12. Diff 自审清单

- [x] 无计划表之外的改动（唯一"计划外"是 `f678acc` 中发现 `packet_count` 变死代码，已并入该提交而非新开）
- [x] 注释/文档与代码同步（`logger.rs` 模块注释与 `append` 文档已随 `be81b0f` 更新；
      `ensure_payload_capacity` 文档换成双向语义；`validate` 注释无需改）
- [x] 测试同步补充（protocol +2、windows +8，均为回归用例）
- [x] 未引入新 panic 路径（唯一新增 `debug_assert` 无；`unwrap()` 未新增）
- [x] 未引入新分配热点（`truncate_in_place` 只在轮转失败分支、`shrink_to` 只在异常长度分支）
- [x] 未引入新 unsafe（0 处）
- [x] 序列化/HTTP/日志格式无意外变化（日志文案新增 `[Panic]`/`[Config]`/`[Logger]` 前缀，不改变既有行格式）
- [x] feature 组合与 MSRV 未被破坏（`--all-features` 与 `--no-default-features` 均通过；仍无 `rust-version` 字段，本轮未提升 MSRV 依赖）
