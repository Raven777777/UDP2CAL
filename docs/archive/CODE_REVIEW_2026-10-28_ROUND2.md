# UDP2CAL 全流程审查报告（2026-10-28 第二轮）

> 归档材料。现行为真源仍是代码与 [PROTOCOL_V3_RTP.md](../PROTOCOL_V3_RTP.md)；
> 修复根因摘要见 [BUGFIX.md](../BUGFIX.md)，版本记录见 [CHANGELOG.md](../CHANGELOG.md)。
> 上一轮报告：[CODE_REVIEW_2026-10-28.md](CODE_REVIEW_2026-10-28.md)（23 项，已修 8）。

## 范围与基线

- 基线：`main` / `00a4290`（`docs: record 2026-10-28 full review and verification`）
- 范围：`protocol/`、`windows/`、Android Kotlin/JNI 包装、`scripts/`（构建脚本）
- 排除：`target/`、`vendor/`、`opus-src/`、Android 生成目录
- 方法：4 个只读 reviewer 并行审查（Rust protocol / Rust windows / Kotlin Android / Python
  构建），主审逐条复核证据后**只实施可验证的修复**；语义改动一律暂缓并记录理由
- 铁律：安全优先、证据驱动、最小爆炸半径（一次一个逻辑变更；机械/语义分离；无计划外改动）

## 基线状态

| 门禁 | 命令 | 结果 |
|---|---|---|
| protocol fmt | `cargo fmt --check` | 通过 |
| protocol Clippy | `cargo clippy --all-targets --features jni -- -D warnings` | 0 警告 |
| protocol tests | `CARGO_TARGET_DIR=target/review-verify cargo test --features jni` | 80 passed / 0 failed |
| Windows fmt | `cargo fmt --check` | 通过 |
| Windows Clippy | `cargo clippy --all-targets -- -D warnings` | 0 警告 |
| Windows tests | `cargo test` | 44 单测 + 2 集成 passed |
| Android JVM tests | `JAVA_HOME=<jdk-17.0.21.21-hotspot> ./gradlew testDebugUnitTest --offline` | BUILD SUCCESSFUL（15 s） |

环境：cargo/rustc 1.99.1、Python 3.14.7、Microsoft JDK 17.0.21.21、Gradle 8.9、NDK 27.0.12077973。

## 本轮修复（4 处，均在改后重跑门禁）

### R1 — `scripts/udp2cal/deps.py` `heal_daemon_registry()` 重写（+ `scripts/udp2cal/android.py` 注释）

**依据**（python/构建 reviewer，runId `del_mvr0e69w_313e`）：

1. `registry.bin` 的字符串是「长度前缀 + UTF-8」，**没有终止符**，原实现
   `_REGISTRY_PATH_RE = r"[A-Za-z]:\\[^\x00]*"` 依赖 NUL 截断不成立。实测 8.9 的 799 B 文件：
   uid 串 → `0x31` + `C:\Program Files\Microsoft\jdk-17.0.21.21-hotspot` → `00 00 00 11` →
   `0x1d` + `C:\Users\21248\.gradle\daemon` → `0x01`。`daemonRegistryDir` 之后紧跟 `0x01`，
   会被并入路径产生幻影路径。
2. 原名称过滤 `jdk|jbr|jre` 恰好漏掉 `config.py:71` 的 `%USERPROFILE%\.jdks\*`
   （`ms-17.0.11` / `corretto-17.0.10` / `temurin-17.0.11` / `Zulu\zulu-17`），且正则写死
   `[A-Za-z]:\\` 不匹配正斜杠或 UNC。
3. 触发条件（`javap` 反查 `DaemonCompatibilitySpec.jvmCompatible`）：`javaHome.exists() &&
   Jvm.forHome()`——**目录被彻底删除时只判不兼容、不抛异常**；只有「目录还在但 `bin/java`
   缺失」才抛 `JavaHomeException`。
4. 原实现连 `registry.bin.lock` 一起删（破坏 `FileLockManager` 跨进程互斥），且不校验删除
   结果却恒报成功。

**改动**：正则改 `[A-Za-z]:[\\/][^\x00]*` + `_strip_control_tail()` 砍掉结尾控制字节；判定条件
改为「`path.is_dir()` 且 `(path/"bin"/exe).is_file()` 为假」；排除 `gradle_home` 本身与 daemon
注册表目录（`_is_under()` 或末段 `daemon`）以免把 `daemonRegistryDir` 当成失效 JDK 每次构建重清；
只删 `registry.bin`（保留 `.lock`）；删除失败 `warn` 且不计入 `healed`。

**验证**（一次性脚本，未入库）：合成注册表 + 真实 8.9 注册表副本

| 场景 | 期望 | 实测 |
|---|---|---|
| A 有效 JDK | 不清理、保留 | `healed=0 kept=True` |
| B 半卸载（`bin/` 在、`java.exe` 缺失） | 清理 | `healed=1 kept=False` |
| C 目录彻底删除 | 不清理、保留 | `healed=0 kept=True` |
| D 幻影 `0x01` 尾（路径有效） | 不清理、保留 | `healed=0 kept=True` |
| E 空壳残留目录（无 `bin/`） | 清理 | `healed=1 kept=False` |
| G 真实 `~/.gradle/daemon/8.9/registry.bin` 副本 | 不清理（`daemonRegistryDir` 不得误判） | `healed=0 kept=True` |

另：修复后 `python scripts/build.py doctor` 正常通过，`~/.gradle/daemon/*/registry.bin` 全部保留（未被误删）。

**回滚**：`git checkout -- scripts/udp2cal/deps.py scripts/udp2cal/android.py`

### R2 — `windows/src/main.rs` 补齐「RTP 空闲 >1 s 重置抖动缓冲」契约

**依据**（protocol reviewer，runId `del_mvr0e69v_nkib`，F1）：`protocol/rtp.rs:582-586` 的迟到判定
先于 `:606` 的 M=1 重同步判定，因此回退时间线的 marker 包永远无法 resync；而 `pull` 在 PLC/静音
分支调用 `advance()`（`protocol/rtp.rs:828-829` `self.next_ts += FRAME_SAMPLES as u64`），
`next_ts` 无界前推。实测（`start_ms=30`）：播放 4 帧后空转 6000 次 pull，`next_ts=2 880 000`，
插入 M=1 ts=0/480/960 全部 `Dropped`，`late=3`、`resyncs=0`、此后零播放，只能外部 `reset()` 恢复。

规范已把修复责任放在应用层：`docs/PROTOCOL_V3_RTP.md:1079-1081`「RTP 连续空闲超过 1 s 后，下个
通过校验的包到达时先 `JitterBuffer.reset()`」；Android 已实现
（`CaptureService.kt:76` `REVERSE_IDLE_RESET_MS = 1_000L`，`:972-978` 在 `insert` 前 reset）；
Windows 只有 500 ms 的 `rtp_idle`（`windows/src/main.rs:691`）且仅复位 `drift`
（`:701`、`:707`），**从未 reset 抖动缓冲**——发送端时间线回退时整条反向音频永久静音。

**改动**：新增常量 `RTP_IDLE_RESET_MS: u64 = 1_000`（`windows/src/main.rs:137`）+ 接收循环
`last_rtp_at: Option<Instant>`；在 `jb.insert()` 前若空闲超限则先 `jb.reset()`，且 `logf!` 在锁外。
不选择修改 `protocol/rtp.rs` 的共享语义：Android 侧已依赖「应用层 1 s 重置」这一契约，单改
缓冲内部行为会同时改变两端播放时序，属语义改动，需真机听感验证（见「保留风险」）。

**验证**：`cargo fmt --check` 通过；`cargo clippy --all-targets -- -D warnings` 0 警告；
`cargo test` 44 + 2 passed。**未执行**：端到端断流→时间线回退→恢复的真机/驱动验证。

**回滚**：`git checkout -- windows/src/main.rs`（与 R3 同一文件，见下）

### R3 — `windows/src/main.rs` 配置保存失败提示被立即清除

**依据**（windows reviewer，runId `del_mvr0e69v_pu3h`，#4）：`persist_config()`（`:1178-1184`）
在保存失败时写 `self.notice = "配置保存失败..."`，而两个调用点紧接着 `self.notice.clear()`
（`:1201-1203`、`:1292`）——注册表写入被拦截时用户永远看不到提示，与函数注释声明的意图相反。

**改动**：`persist_config()` 改为返回 `bool`；`ToggleAutoStart` 只在成功时清 `notice`；
`Start` 用 `let saved = self.persist_config();` 并在结尾 `if saved { self.notice.clear(); }`。

**验证**：同上（fmt / clippy / test 全绿）。**未执行**：注入 `Config::save()` 失败的可视化验证
（需改注册表权限，属人工步骤）。

**回滚**：`git checkout -- windows/src/main.rs`

### R4 — `scripts/udp2cal/runner.py` `cmd /c` 引号拼接

**依据**（python/构建 reviewer，#6）：原 `build_command()` 返回 `["cmd", "/c"] + parts`，
`subprocess` 会用 `list2cmdline` 把含空格的参数各自加引号，得到
`cmd /c "C:\..\sp ace\gw.bat" --flag "a b"`。cmd.exe 规则：命令串中**超过两个引号**时先剥掉首尾
各一个再解析，于是在 bat 路径的空格处断开。

**实测（修复前）**：`'C:/Users/21248/AppData/Local/Temp/sp' is not recognized as an internal or
external command`（参数含空格时必现；bat 路径含空格但参数无空格时侥幸可用）。

**改动**：`build_command()` 在 shell 场景返回已拼好的字符串 `cmd /c "{}"`（`{}` 为
`subprocess.list2cmdline(parts)`），即规范写法 `cmd /c ""bat" "arg with space""`；`run`/`capture`
改用 `_display(cmd)` 生成日志行，错误提示改用 `cmd[0]`。类型注解改为 `Union[str, List[str]]`。

**验证**（一次性脚本 + 真实 wrapper）

| 用例 | 结果 |
|---|---|
| `build_command(["cargo","--version"])` | `['cargo', '--version']`（未变） |
| 含空格目录的 `.bat` + `"arg with space"` via `run()` | `rc=7`（bat 自身退出码），`GOT:[--flag "arg with space"]` |
| 同上 via `capture()` | `rc=7`，参数完整 |
| 非 bat（`cmd /c echo plain`） | `rc=0`，输出 `plain` |
| 真实 `android/gradlew.bat --version --offline` | `rc=0`，`Gradle 8.9` |

另：`python -m py_compile` 通过；`python scripts/build.py doctor` 通过。

**回滚**：`git checkout -- scripts/udp2cal/runner.py`

## 保留风险（本轮未修，附理由）

### P2

| # | 位置 | 问题 | 为何不修 |
|---|---|---|---|
| P2-1 | `protocol/rtp.rs:582-586` vs `:606` | 迟到判定先于 M=1 重同步，回退时间线无法恢复 | 已在应用层按规范补齐（R2）；改缓冲内部语义会同时改变 Android 播放时序，须双端同评 + 真机听感验证 |
| P2-2 | `protocol/rtp.rs:534-543`、`:624` | `water_ms()` 由无界的 `highest_ts` 决定；一个 M=1 丢失的远跳包即可造成 ~460 ms 掉音并把漂移 P 项顶到钳位（8.6 cents） | 需新增「媒体时间窗」并改变 `insert` 接纳策略，属语义改动，影响 `DriftController` 行为 |
| P2-3 | `windows/src/capture.rs:947-951`、`978-982`、`1001-1006` | `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` 已在 `Initialize()` 前写入 `flags`，`SetEventHandle` 失败后仅关句柄不清标志 → `Start()` 返回 `AUDCLNT_E_EVENTHANDLE_NOT_SET`，「10 ms 超时轮询」回退分支形同虚设 | 需重构 WASAPI 客户端创建/初始化流程；失败路径只在特定驱动出现，本机无法复现验证（`cargo test` 覆盖不到） |
| P2-4 | `windows/src/main.rs:2234-2254`、`1274-1279` | 音频初始化 3 s 后仍 `Pending` 被当作成功；初始化卡死时 UI 显示运行、无输出，且 Stop/Start 不会重试 | 直接把 `Pending` 判为失败可能把「慢机器上最终会成功」的初始化变成硬错误，需实测后再定阈值/语义 |
| P2-5 | `android/.../service/CaptureService.kt:903-916` vs `:1094`、`AudioPlayer.kt:178-192` | UI 线程 `stopCapture` 持 `linkLock` → `flush()` → `synchronized(trackLock)` 与播放协程的阻塞 `track.write` 构成锁反转，Stop 可卡住 UI | 需真机/ANR 复现与播放线程改造，JVM 单测无法覆盖 |
| P2-6 | `android/.../native/OpusDecoder.kt:86-90` | `reset()` 先 `destroy` 再 `decoderCreate`，失败后 handle 恒 0，`decode`/`plc` 恒 -1 → 反向永久静音且不再自愈 | 需真机验证 `decoderCreate` 失败路径；且 `CaptureService.kt:344-347` 只在会话开始调用 `start()` 的早退逻辑要一起改 |
| P2-7 | `android/app/build.gradle.kts:213` | `releaseSigning ?: debug` 静默回落 debug 签名；`checkReleaseBuilds=true` 但 `abortOnError=false`（Lint 非阻断） | 属发布流程/签名配置决策，需项目维护者确认签名方案，非纯代码缺陷 |

### P3

protocol（`protocol/rtp.rs` / `jni.rs` / `protocol.rs`）
- `rtp.rs:624` 序号窗闸门会丢弃「时间戳合法但扩展序号超前」的真实包，与「时间戳是播放顺序唯一权威」的注释矛盾。
- `rtp.rs:663` 只支持精确 480 样本栅格；20 ms 帧的发送端会被静默 `Wait` 卡死，无统计信号。
- `jni.rs:854-884` `jbInsert` 绕过 `rtp::parse`，与 Windows 路径校验口径不一致（长度仍由 `insert` 校验，非内存安全问题）。
- `jni.rs:703` `extendTs` 把 `jlong` 截成 `u32`；`jni.rs:1341` `reObserve` 缺 `.max(0)`（`jbArrivalNanosAt` 有），负值会让 `RateEstimator` 永久冻结——当前 Kotlin 调用方不会传负值，属 API 加固。
- `rtp.rs:189` `ms_to_samples` 用 `u32` 乘法，`ms ≥ 8 947 848` 时 debug panic / release 回绕（目前仅测试调用，但函数是 `pub`）。
- `protocol.rs:181-187` `build_control` 超长负载在 release 下静默截断（debug 才 `debug_assert`）。
- `jni.rs:379` `decodeControlHeader` 不回传实际可用长度，Kotlin 侧需自行再校验。

windows
- `audio.rs:311-318` / `631-632` / `657-662`：`clock_ns` 与 `clock_frames` 两个原子量可被读成「新时刻 + 旧帧数」，撕裂约一个回调；对估计器的影响未量化（reviewer 标注未验证）。
- `tests/reverse_max_bitrate.rs:131-142`：若 `encode()` 全失败，`max_len` 保持 0，断言空转通过。实测当前 `max_len=1275`、`decoded=600`，故目前未空转，但缺「至少成功一次」的显式断言。
- `main.rs:2041-2043`：`handle_control()` 无 `APP_RUNNING` 闸门，CONNECT 与 Stop 竞争时可留下 BUSY + 已发 ACK（`desired_reverse_ssrc()` 返回 0，故不会启动反向采集）。

android
- `AudioPlayer.kt:312` `stop()` 无条件 `routeToEarpiece(false)`，改写全局路由偏好。
- `DiscoveryManager.kt:96` `Character.digit` 遇非十六进制返回 -1 → 0xEF；`getOrCreateDeviceId`（`:83-84`）只校验 `length==16`，接受全零 device_id。
- `UdpSender.kt:211-216` ACK 线程 `continue` 忙等无退避；`UdpSender.kt:246-256` `close()` `join(200)` 后仍把活线程置 null，重连后旧线程可抢占新 socket（双消费者）。
- `StreamResampler.kt:38`/`:84`/`:116`：`out` 数组按 `ratio>=1` 分配、只校验输入上限，44.1k→48k 上采样时 `outLen++` 可越界（当前调用方恒传 480 帧，未触发）。
- `CaptureService.kt:407-415`/`719-723`：`restartCapture` 的 `finally` 重启可穿越用户 Stop。
- `AudioPlayer.kt:260-272`：`flush()` 假定 `playbackHeadPosition` 归零（未真机验证）。
- 测试缺口：`AudioPlayer`/`UdpSender`/`DiscoveryManager`/`CaptureService` 生命周期零覆盖。

scripts
- `scripts/build.py:203` `doctor()` 恒返回 0：发现阻塞项也无法让 CI/调用方感知。
- `scripts/udp2cal/deps.py` 模块 docstring 声称「所有改动都先写 `.bak`」，`heal_daemon_registry` 无备份（注册表是可重建缓存，故未加备份，但描述与实现不一致）。
- `scripts/udp2cal/runner.py:157` `proc.wait()` 无超时；`_terminate()` 的 `taskkill` 已有超时。
- `heal_daemon_registry` 仍会遍历并清理**所有** Gradle 版本目录（8.9/8.11.1/9.x），没有开关——注册表按版本存放且是可重建缓存，判定条件收紧后风险可接受，但如需「零副作用」模式应加开关。

## 未执行 / 环境不支持

- WASAPI 真机行为：`SetEventHandle` 失败分支、驱动热插拔、VB-Cable 路由（无对应驱动/设备可复现）。
- Android 真机/模拟器：锁反转 ANR、`AudioTrack` 路由、`playbackHeadPosition` 归零假设、JNI 编解码失败路径。
- 端到端双端串流（Windows ↔ Android 反向音频、断流恢复、时间线回退）。
- `assembleDebug`/`assembleRelease`、Lint 报告、RustSec/`cargo-deny` 等供应链扫描（本轮未跑）。
- Python 侧无测试套件：R1/R4 用一次性脚本验证，脚本未入库（`%TEMP%/verify_heal.py`、`%TEMP%/verify_runner.py`）。

## 终态与下一轮建议

本轮**改动 4 个文件、未提交**：

| 文件 | 变更 |
|---|---|
| `scripts/udp2cal/deps.py` | `heal_daemon_registry` 重写（101 行） |
| `scripts/udp2cal/android.py` | 调用处注释改引 `DaemonCompatibilitySpec.jvmCompatible` |
| `scripts/udp2cal/runner.py` | `build_command` cmd 引号修复 + `_display` 助手 |
| `windows/src/main.rs` | `RTP_IDLE_RESET_MS` 空闲重置 + `persist_config()` 返回值语义 |

整体回滚：`git checkout -- windows/src/main.rs scripts/udp2cal/{deps,android,runner}.py`

建议下一轮（按性价比排序）：

1. **P2-2 / protocol F2**（`water_ms` 无界）与 **P2-1 / F1** 一起评估：给 `insert` 加媒体时间窗后，
   两端的 1 s 空闲重置契约可以保留为双保险；需要真机听感 + `DriftController` 回归。
2. **P2-3 / P2-4（windows）**：WASAPI 客户端初始化重构 + 音频初始化超时语义，需有对应驱动/慢机器环境。
3. **P2-5 / P2-6（android）**：先做播放线程与 `linkLock` 的职责划分（把 `flush()` 移出 UI 线程），
   再改 `OpusDecoder.reset()` 为「先建后删」。
4. **P3 机械项**：`ms_to_samples` 饱和化、`jbInsert` 走 `rtp::parse`、`build_control` 返回 `Option`、
   `doctor()` 返回非零、`reverse_max_bitrate` 增加「至少成功一次」断言、`runner.capture` 等待超时。
5. 构建脚本补最小测试（`build_command` 的三种形态即可覆盖 R4 的回归）。
