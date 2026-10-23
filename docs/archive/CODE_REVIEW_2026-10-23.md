# UDP2CAL 代码质量审查报告

**审查对象**：`windows/`（Rust + iced，1 968 行）、`android/`（Kotlin 2 275 行 + JNI C 292 行）
**审查日期**：2026-10-23
**审查方式**：静态阅读 + 编译器/Clippy 实跑 + 独立验证工程（不改动原仓库源码）
**结论**：架构清晰、工程纪律良好（Rust 端编译零告警、关键热路径有明确的零分配优化痕迹），但存在 **3 个 P0 级缺陷**（1 个未定义行为、1 个音频数据污染、1 个发布签名安全问题）与 **10 个 P1 级功能性缺陷**。主要集中在「跨组件格式契约未校验」和「错误路径不可观测」两类问题。

> **归档说明**：本文档是对 v1.1.2 的一次全量代码审查记录。审查发现的 43 项问题已于同日全部修复，修复内容与验证结果见 §5「修复记录」。当前代码状态请以仓库主干为准。

---

## 一、审查范围与方法

| 项目 | 内容 |
|------|------|
| 审查文件 | `windows/src/*.rs`（7 个）、`android/app/src/main/java/**/*.kt`（12 个）、`android/app/src/main/cpp/opus_jni*.c`（2 个）、构建与清单配置 |
| 依赖协议层 | `protocol/protocol.rs`（498 行，被 Windows 端直接 `pub use`） |
| 构建验证 | `cargo check`：**通过，0 告警**；`cargo clippy --all-targets`：**5 条告警（全部为风格级）**，详见附录 A |
| 行为验证 | 自建独立 Rust 工程，以 `#[path]` 引入 `protocol.rs` 原文件，实测重排缓冲的 6 组时序，见 P1-4 与附录 C |
| 交叉验证 | 用 JDK 25 实跑 `java.util.Formatter`，**排除**了 1 个疑似缺陷（见附录 B） |
| 未覆盖 | 未做真机/真机声卡运行测试，因此涉及具体硬件格式协商、`netsh` 命令行解析项标注为「待实测」 |

> 本次审查**未修改任何源码**，仅新增本报告文件。

---

## 二、缺陷清单

### 2.0 风险分布

| 等级 | 数量 | 定义 |
|------|------|------|
| **P0** | 3 | 未定义行为、内存安全、发布安全，或必然产生错误数据 |
| **P1** | 10 | 功能可感知缺陷（音频异常、状态错误、无法恢复） |
| **P2** | 18 | 健壮性、资源泄漏、可观测性、平台合规 |
| **P3** | 12 | 可维护性、死代码、风格 |

**合计 43 项**：Windows 端 25 项、Android 端 17 项、跨端协议层 1 项。

---

### P0-1 反向捕获缓冲区按「立体声 float32」硬编码解释，未校验 WASAPI 混音格式

- **位置**：Windows 端 `windows/src/capture.rs:262-276`（取格式）、`capture.rs:308-313`（解释缓冲）
- **证据**：

```rust
// capture.rs:262  取到混音格式后只用于 Initialize，随后立即释放，从未解析
let fmt = match client.GetMixFormat() { Ok(f) => f, Err(_) => return None };
...
CoTaskMemFree(Some(fmt as _));      // 275/276 行：格式信息就此丢弃

// capture.rs:308-313  直接按 f32 + 2 声道解释
let num_frames = packet_size as usize;
let total_samples = num_frames * 2;                                  // 硬编码 2 声道
let src = std::slice::from_raw_parts(data_ptr as *const f32, total_samples);
```

- **影响**（按设备混音格式分三种）：
  1. `WAVEFORMATEX` 为 **16-bit**（VB-Cable 常见默认、部分板载声卡）：每帧实占 `2ch × 2B = 4B`，而代码按 `2ch × 4B = 8B` 读取 → **越界读取一倍长度**，属未定义行为，可能崩溃或读到相邻堆内存被当作音频输出。
  2. **5.1 / 7.1 声道**：`total_samples` 只取前 1/3 或 1/4 的有效样本，且声道交错错位 → 输出断续、变调。
  3. **float32 立体声**（绝大多数默认配置）：行为正确，因此该缺陷在开发机上不会暴露。
- **建议**：解析 `WAVEFORMATEXTENSIBLE`（`wFormatTag` / `nChannels` / `wBitsPerSample` / `SubFormat`），据此分支转换 f32/i16/f32-多声道 → 立体声；`GetMixFormat` 结果应与 `IAudioClient` 生命周期解耦并保留。

---

### P0-2 WASAPI 静音标志被忽略，静音期读入未初始化缓冲

- **位置**：`windows/src/capture.rs:295-316`
- **证据**：

```rust
let mut flags: u32 = 0;
let hr = capture.GetBuffer(..., &mut flags, ...);
if hr.is_err() || packet_size == 0 { return None; }
// flags 此后从未被读取，直接 memcpy 内容
```

- **依据**：`IAudioCaptureClient::GetBuffer` 契约规定，当 `flags & AUDCLNT_BUFFERFLAGS_SILENT (0x2)` 置位时，**缓冲区内容未定义**，调用方必须视为静音。
- **影响**：环回捕获在「系统无声音输出」的瞬间可能返回 SILENT 且 `packet_size > 0`，代码将未定义内容当 PCM 编码上送 → 手机侧听到随机噪声/爆音。这与「静音时应安静」的直觉相反，且难以复现定位。
- **建议**：`if flags & 0x2 != 0 { 输出等长零样本 }`，再判断 `packet_size`。

---

### P0-3 Release 签名使用仓库内提交的密钥库，口令为 `android`

- **位置**：`android/app/build.gradle.kts:38-45`；密钥文件 `android/lanmic-debug.keystore`（已入库）
- **证据**：

```kotlin
create("release") {
    storeFile = file("../lanmic-debug.keystore")
    storePassword = "android"
    keyAlias = "lanmic"
    keyPassword = "android"
}
```

- **影响**：
  1. 任何拿到仓库的人都能用**同一签名**、同一 `applicationId`（`com.udp2cal.app`）构建可覆盖安装的恶意 APK —— 用户从第三方渠道"升级"时系统会接受，且无法通过签名不一致察觉。这是发布级安全事故，而非代码风格问题。
  2. 密钥入库后即无法安全轮换：一旦更换密钥，存量用户必须卸载重装（Android 不允许签名变更的覆盖安装）。
  3. 文件名为 `-debug.keystore`，说明这原本是调试用密钥，被直接提升为 release 密钥。
- **建议**：`signingConfigs` 从环境变量/CI Secret 读取；密钥移出版本库并加入 `.gitignore`；如已公开发布过，评估是否需要在下一个大版本换钥并公示。

---

### P1-1 错误信息全部收集但从未渲染；绑定失败后界面仍显示"运行中"

- **位置**：`windows/src/main.rs:334`（字段）、`441-505`（写入）、`526-782`（`view()`）
- **证据**：`grep status_text src/main.rs` 共 10 处，`view()` 中仅 1 处使用，且只用于字符串嗅探：

```rust
// main.rs:540
(red, if self.status_text.contains("断开") { "● 已断开" } else { "● 等待连接" })
```

即 `"端口号无效，请输入 1-65535"`（462）、`"端口号不能为 0"`（467）、`"绑定失败"`（1039）、`"Opus解码器初始化失败"`（1056）、`"音频初始化失败"`（1063）、`"已连接手机端"`（1007）**全部不会显示给用户**。

- **叠加缺陷**：`udp_receiver_stream` 绑定失败时只发一条 `StatusUpdate` 就 `return`（`main.rs:1033-1042`），**没有把 `is_running` 置回 false**。结果：按钮仍显示"停止"、`APP_RUNNING=1` 继续每秒广播、发现回复仍指向一个无人监听的端口 —— 手机能搜到、能连上、但永远没有声音，且界面停在"● 等待连接"，用户无从判断。
- **建议**：状态栏增加错误行渲染 `status_text`；绑定失败时走统一的失败收尾（`is_running=false`、`APP_RUNNING=0`、`reset_to_ready()`）。

---

### P1-2 设备 ID 非幂等：每次 `save()` 都会重新生成并写入新 ID

- **位置**：`windows/src/config.rs:37-49`、`66-75`
- **证据**：

```rust
pub fn get_device_id(&self) -> u64 {          // &self，不写回 self
    if self.device_id == 0 {                  // 首次运行
        let id_bytes = udp2cal_protocol::generate_device_id();
        u64::from_be_bytes(id_bytes)          // 每次调用都是新的随机值
    } else { self.device_id }
}

pub fn save(&self) -> ... {
    key.set_value("device_id", &self.get_device_id())?;   // 72 行：写的是"又一个新的"
}
```

`Config::get_device_id()` 取 `&self`，不缓存结果；`ToggleAutoStart` / `ToggleReverse` / `ToggleRunning` 任一路径调用 `save()` 都会生成**新的随机 ID** 落库。而 `main.rs` 的广播线程每秒 `Config::load()` 一次（`main.rs:204,206,269,271`），`my_device_id()`（`main.rs:104-106`）在每次 ACK/绑定前也会调 `load()`。

- **影响**：一次会话内 PC 的"设备身份"会随用户操作跳变；`send_connect_ack` 送出的 `device_id` 可能与广播中的不一致。当前 Android 端不校验 ACK 的 device_id，所以尚未引发可见故障 —— 但这正是"预埋故障"，一旦补上校验就会立刻表现为连不上。
- **建议**：启动时一次性 `read → 若无则 generate → 立即 persist`，并把结果缓存在 `Config` 实例上。

---

### P1-3 开机自启注册表项未加引号

- **位置**：`windows/src/config.rs:86`
- **证据**：`run_key.set_value("UDP2CAL", &exe.to_string_lossy().to_string())`
- **影响**：安装路径含空格（`C:\Program Files\UDP2CAL\udp2cal.exe`）时，`CreateProcess` 的解析存在歧义，Windows 会用 `C:\Program.exe` 之类的截断路径尝试启动 → 自启静默失效或启动错误程序。属经典的 Unquoted Service Path 类问题（此处危害限于自启失败）。
- **建议**：`format!("\"{}\"", exe.display())`；同时校验 `set_value` 结果，失败时把 UI 的"开机自启"开关回滚（当前 `ToggleAutoStart` 无论注册表写成功与否都保存配置，开关会"假亮"）。

---

### P1-4 重排缓冲把"重复包/迟到包"误判为跳变，立即播放并回退序号

- **位置**：`protocol/protocol.rs:262-283`（`MAX_REORDER = 2`）
- **实测**（独立工程引入原文件，见附录 C 复现命令）：

```
[2] 重复包(seq=1 二次到达) -> ["0", "1", "1", "2"]      ← 同一帧被播放两次
[4] 双乱序(1,2,0)          -> ["2", "0"]                ← 乱序输出，且缓冲中的 1 被丢弃
[6] 序号回绕 254,255,0,1   -> ["254","255","0","1"]     ← 回绕正常（排除项）
```

- **根因**：

```rust
let dist = seq.wrapping_sub(self.next_seq);
if dist as usize >= MAX_REORDER {     // 迟到包 dist 变成 253~255，同样命中此分支
    self.clear();                     // 清空已缓冲的合法包
    self.next_seq = seq.wrapping_add(1);  // 序号被"倒回"
    f(sample_rate, payload);          // 迟到/重复包被立即播放
    return;
}
```

代码只区分了"超前太多"与"正好/超前 1"，**没有区分"落后"**。UDP 局域网虽少见重传，但重复包、乱序包在 Wi-Fi 重传、多路径下并非不可能。
- **影响**：单次重复包 → 该 10 ms 音频被解码两次（听感为轻微"哒"声）；两包以上乱序 → 顺序错乱 + 缓冲包丢失。
- **建议**：用有符号距离判定：

```rust
let dist = seq.wrapping_sub(self.next_seq) as i8;
if dist < 0 { return; }        // 迟到/重复：直接丢弃（可选：计入统计）
if dist as usize >= MAX_REORDER { /* 真正的跳变 */ }
```

---

### P1-5 音频回调线程阻塞在 `std::Mutex` 上（优先级反转）

- **位置**：`windows/src/audio.rs:171-199`（CPAL 回调）vs `audio.rs:105-117`（生产者加锁）
- **证据**：回调内 `buf_strong.lock()`；生产者在持锁状态下执行 `buf.extend(&self.resample_buf)`，必要时还执行 `buf.drain(..excess)`（O(n) 搬移）。
- **影响**：回调线程被写线程阻塞超过一个缓冲周期（40 ms 初始缓冲）即产生欠载爆音。代码注释已意识到 `shrink_to` 在锁内重分配的危害（113-115 行），但 `extend` + `drain` 仍在锁内。
- **建议**：改为无锁 SPSC 环形缓冲（`ringbuf` crate 或自建 `AtomicUsize` 头尾指针 + `UnsafeCell`），或至少把 `drain` 改成"读指针前移"的环形结构，避免在锁内移动内存。

---

### P1-6 `effective_ratio` 前 3 秒恒为 1.0，采样率不匹配时开头必然变速

- **位置**：`windows/src/audio.rs:69`（唯一更新点，位于 `elapsed > 3s` 分支内）、`audio.rs:242-244`
- **证据**：

```rust
if now.duration_since(self.last_ratio_update) > Duration::from_secs(3) {
    ...
    self.effective_ratio = (source_rate as f64 / self.device_rate as f64) / self.drift_ratio;
}
...
effective_ratio: 1.0,   // 初值，注释自认："首次 write 时若 source_rate != device_rate 会在 3s EMA 前使用此值"
```

- **触发条件**：输出设备不支持 48 kHz，走 `with_max_sample_rate()`（如 44.1 kHz，`audio.rs:138-144`），而手机仍送 48 kHz。
- **影响**：前 3 秒以 1.0 比率"重采样"（等于不重采样）→ 8.8% 的持续加速与音调偏高，3 秒后突然回归正常，听感为明显的"变调 → 跳回"。
- **建议**：`write()` 首次调用时若无历史测量，直接令 `effective_ratio = source_rate as f64 / device_rate as f64`（漂移修正叠加在其上）。

---

### P1-7 未读取 `supported_config.sample_format()`，一律按 f32 建流（待实测确认）

- **位置**：`windows/src/audio.rs:126-144`、`169`
- **证据**：`supported_config` 携带 `sample_format()`，但代码直接 `device.build_output_stream::<f32>(...)`，从未 match 格式；且 `.find(|c| c.channels() == 1)` 取的是枚举顺序中的**第一个单声道配置**，其格式不可控。
- **影响**：若选中的配置仅声明 i16，CPAL 会返回 `BuildStreamError`，最终表现为 `AUDIO_INIT_STATE = 2` → "音频初始化失败"，完全无声。是否触发取决于 WASAPI 的格式枚举顺序，故标注待实测。
- **建议**：`match supported_config.sample_format() { F32 => build::<f32>, I16 => build::<i16>, U16 => ... }`，或在 `find` 时加入 `c.sample_format() == SampleFormat::F32` 条件。

---

### P1-8 反向接收协程在 socket 关闭后空转 100% CPU，且反向串流永久失效

- **位置**：`android/.../service/CaptureService.kt:633-682`、`UdpSender.kt:85-90`、`CaptureService.kt:466-476`
- **证据**：

```kotlin
// 热重连：关闭旧 UdpSender（连同 reverseSock）
udpSender?.close()
udpSender = UdpSender(targetIpFromPrefs, targetPortFromPrefs)
...
// 反向协程持有的是旧的 revSock 引用（614 行 val 绑定，循环外不重建）
} catch (_: Exception) {
    if (!isActive) break      // 已关闭的 socket 会立刻抛 SocketException → isActive 仍为 true → 继续循环
}
```

- **影响**：
  1. `receive()` 对已关闭 socket 立即抛异常，循环无退避 → 单核 100% 占用、耗电与发热；
  2. 热重连后**没有重启 `reverseJob`** → 反向串流（PC→手机）在用户改一次 IP 后永久失效，直到手动停止再启动。
- **建议**：捕获 `SocketException` 时 `break`；重连成功后显式 `reverseJob = launchReverseAudio()`；异常分支加 `delay(5..20)` 兜底退避。

---

### P1-9 反向播放阻塞写、无漂移补偿、忽略写入返回值

- **位置**：`android/.../AudioPlayer.kt:123-130`、`CaptureService.kt:648-651`
- **证据**：

```kotlin
fun write(pcm: ShortArray, offset: Int, length: Int) {
    val track = audioTrack ?: return
    try { track.write(pcm, offset, length) } catch (e: Exception) { ... }   // 阻塞写，返回值被丢弃
}
```

- **影响**：PC 端采集时钟与手机 DAC 时钟不同源。PC 稍快时 AudioTrack 缓冲填满 → `write` 阻塞 → 反向协程停摆 → socket 接收缓冲（约 1 s）耗尽后开始成片丢包；延迟单向累积且不会自愈。注意 **PC 端有完整的漂移补偿器（`windows/src/audio.rs:41-74`），手机端完全没有** —— 这是双向链路的非对称缺口。
- **建议**：`WRITE_NON_BLOCKING` + `playbackHeadPosition` 反馈，超出目标填充时丢帧/插帧（可直接移植 `AudioWriter` 的思路）。

---

### P1-10 反向 JNI 解码无同步，`stop()` 与 `decode()` 竞争导致 native 释放后使用

- **位置**：`android/.../native/OpusDecoder.kt:42-54`、`CaptureService.kt:579-582`
- **证据**：

```kotlin
// CaptureService finally
reverseAudioJob?.cancel()      // 异步，不等待
reverseDecoder?.stop()         // 立即 free 掉 native handle

// OpusDecoder
fun decode(...) { val h = handle; if (h == 0L) return -1; return OpusDecodeNative.decoderDecode(h, ...) }  // 无 @Synchronized
fun stop() { ... OpusDecodeNative.decoderDestroy(h); handle = 0 }
```

- **影响**：`cancel()` 后协程可能仍在 `decoderDecode` 的 JNI 帧内，`decoderDestroy` 释放 `OpusDecoder*` → `opus_decode` 访问已释放内存 → 概率性 SIGSEGV（用户侧表现为"偶发闪退"）。对照 `OpusEncoder` 的所有方法都有 `@Synchronized`，说明这是遗漏而非设计。
- **建议**：`decode` / `stop` 同时 `@Synchronized`，或在 `stop()` 前 `reverseAudioJob?.join()`。

---

### P2 级问题（18 项，摘要）

| 编号 | 平台 | 位置 | 问题 | 影响 |
|------|------|------|------|------|
| P2-1 | Win | `main.rs:920-928` | 每次 CONNECT 保活都 `bind("0.0.0.0:0")` 新建 socket 发 ACK，1 个/秒、约 8.6 万个/天 | 临时端口浪费；ACK 源端口随机（非监听端口），对端无法据此归属连接 |
| P2-2 | Win | `firewall.rs:6-29` | 规则为 `dir=in protocol=UDP localport=any profile=any`，未限定 `program=`；每次启动先删后加，失败被静默忽略 | 为整机开放**所有入站 UDP**，远超功能所需；删/加之间有空窗；`netsh` 需提权，失败用户无感知 |
| P2-3 | Win | `firewall.rs:9,18` | 规则名含空格却以 `name=UDP2CAL 局域网音频串流` 传参，netsh 自解析命令行通常要求 `name="..."` | 待实测：可能建出截断名规则，导致每次启动"删不掉旧的、又加一条新的" |
| P2-4 | Win | `main.rs:861-893` | `restore_native_window()` 取进程内**第一个**顶层窗口（`EnumWindows` 首命中即返回） | 可能命中的是不可见的辅助窗口 → 托盘双击"没反应"（间歇性） |
| P2-5 | Win | `capture.rs:128-152,201-242` | `stop()` 只置 `AtomicBool`，线程不可 join（`thread_handle` 被标 `#[allow(dead_code)]`）；端口变化时先 `stop()` 立刻 `start()` | 新旧发送线程重叠 ≤140 ms → 手机收到重复包（手机侧无序号处理，直接重复播放） |
| P2-6 | Win | `capture.rs:227` | `let plen = n.min(1472)` 对超长 Opus 包**截断**而非丢弃 | 产出非法包，解码端报错；正确做法是 `if n > MAX_PAYLOAD { 丢弃 }` |
| P2-7 | Win | `capture.rs:157` | `CoInitializeEx` 无配对 `CoUninitialize` | 每次反向重连泄漏一个 COM 初始化计数 |
| P2-8 | Win | `main.rs:1046-1066` | 只等 `AUDIO_INIT_STATE` 最多 500 ms；若超时仍为 0 则**不再复查** | 冷启动慢的机器上，后续若初始化失败，错误提示永不出现（与 P1-1 叠加后完全静默） |
| P2-9 | Win | `main.rs:77-83,969-1004` | `GLOBAL_DEVICE_STATE` 与 `BOUND_DEVICE_ID` 是两个独立全局，全部用 `Ordering::Relaxed` | 单个原子无竞争，但二者构成的复合不变式无 happens-before 保证，可观测到"BUSY + 旧 ID"；x86 上实践可用，属形式化隐患 |
| P2-10 | And | `CaptureService.kt:102-108` | `onStartCommand` 每次启动都 `serviceScope.launch { _status.collect { startForeground(...) } }`，且无去重 | 第 N 次"开始采集"后有 N 个常驻收集器，每秒重复调用 N 次 `startForeground` |
| P2-11 | And | `opus_jni_decoder.c:55-56` | `GetByteArrayElements` / `GetShortArrayElements` 返回值未判 NULL | OOM 时 `opus_bytes + offset` 空指针解引用；编码侧（`opus_jni.c:91,117`）**有**判空，属不一致 |
| P2-12 | And | `CaptureService.kt:641-648` | 反向路径不校验 `hdr.deviceId`，也无序号/乱序处理，任何合法头的包直接解码播放 | 局域网内任意主机可向该端口注入音频；P2-5 的重叠重发会被原样播放 |
| P2-13 | And | `MainActivity.kt:451-459` | 反向卡片"音频带宽"渲染的是 `bwDisplayName(opusBandwidth)`（**正向**编码带宽）；`status.reverseBw` 被赋值但从未渲染（grep 证实） | 界面显示 NB 8kHz，实际反向恒为 48 kHz 立体声 —— 参数面板给出错误信息 |
| P2-14 | And | `MainActivity.kt:147-158,288-307` | `deviceId`、`connected` 两处状态被写入却从不读取（grep 证实无读取点） | 每次状态变化触发无意义重组；改造成本低但属死代码 |
| P2-15 | And | `AndroidManifest.xml:8,10` | 声明 `FOREGROUND_SERVICE_SPECIAL_USE`（服务只声明 `microphone` 类型）与 `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`（全仓库无对应调用，grep 证实） | 未使用的敏感权限；上架审核会被要求说明理由，可能被驳回 |
| P2-16 | And | `CaptureService.kt:94-110` | 返回 `START_STICKY`，但 `onStartCommand` 对 `intent == null`（系统重启场景）无处理，只 `startForeground` | 进程被杀后复活成"僵尸前台服务"：常驻通知显示"等待连接"，实际不采集、无法通过界面停止 |
| P2-17 | And | `app/build.gradle.kts:66-69` | `lint { checkReleaseBuilds = false; abortOnError = false }` | 发布构建完全关闭 lint 门禁，是 P0-3、P2-15 这类问题未被拦截的直接原因 |
| P2-18 | And | 全局 | Windows 端与 Android 端均无针对音频路径的自动化测试（`protocol/` 有 15 个单测，`windows/`、`android/` 为 0） | 重采样、漂移补偿、重排这类纯逻辑完全可以单测覆盖，目前靠人耳回归 |

> 说明：P2 表内共 18 条，其中 P2-3 为待实测确认、P2-9 为形式化隐患，其余 16 条可按常规迭代节奏排期。

---

### P3 级问题（12 项，摘要）

| 编号 | 平台 | 位置 | 问题 |
|------|------|------|------|
| P3-1 | Win | `main.rs:747` | `if self.reverse_enabled { "反向串流" } else { "反向串流" }` —— 两分支完全相同（Clippy 已告警） |
| P3-2 | Win | `main.rs:736` | 用 `rev_c == accent`（`Color` 浮点相等）判断按钮是否激活，应为布尔量 |
| P3-3 | Win | `main.rs:605` | `self.last_level_db.max(-60.0).min(0.0)` → 应为 `clamp`（Clippy 已告警） |
| P3-4 | Win | `main.rs:259`、`952` | `match` 单分支/等值判断，应为 `if let` / `if`（Clippy 已告警） |
| P3-5 | Win | `main.rs:204-206,269-272` | 广播线程每秒 `Config::load()`（4 次注册表读）+ `env::var("COMPUTERNAME")`；`my_device_id()` 在每次 ACK 前也读注册表 |
| P3-6 | Win | `main.rs:401-408` | 窗口固定 320×290 且 `resizable: false`，按各元素高度估算约 297 px（含条件渲染的反向状态行）→ 底部有被裁切风险 |
| P3-7 | Win | `main.rs:342-357` | 单实例互斥体命中后只弹提示框，不激活已运行实例的窗口 |
| P3-8 | Win | `Cargo.toml:8-13` | `opt-level = "s"` 用于实时音频路径，以体积换性能；`image` 同时出现在 `[dependencies]` 与 `[build-dependencies]`（编译两份） |
| P3-9 | And | `proguard-rules.pro:1-2` | 保留规则写的是旧包名 `com.udp2mic.app.native`，实际为 `com.udp2cal.app.native`。当前靠 AGP 默认的 `-keepclasseswithmembernames class * { native <methods>; }` 兜住，自定义规则实际是死配置；若默认规则被替换，JNI 将 `UnsatisfiedLinkError` |
| P3-10 | And | `CaptureService.kt:493-505`、`UdpSender.kt:51-58,65-80` | 每帧（10 ms）读 9 个 SharedPreferences、`Objects.hash(varargs)` 装箱、`InetAddress.getByName(host)` 解析、`DatagramPacket` 分配、`drainAck()` 以 **1 ms `soTimeout` + 每帧一次异常** 做流控 —— 与文件内多处"零分配"注释自相矛盾，且异常构造本身有 µs 级开销 |
| P3-11 | And | `android/local.properties` | 入库了本机绝对路径（`sdk.dir=C:\Users\21248\...`），他人克隆即失败；应加入 `.gitignore` |
| P3-12 | And | `MainActivity.kt:67-71` | 强制 `preferredRefreshRate = maxRate`（90/120/144 Hz），而界面基本静态；对一个主打"低发热"的应用属反向优化 |

---

## 三、跨端一致性与协议层结论

1. **协议字段对齐正确**：Rust `protocol.rs` 与 Kotlin `Udp2CalProtocol.kt` 的 15 字节头、位域、`TYPE_*`、采样率 ID 完全一致；`CONNECT` 扩展参数（`complexity/signal/bandwidth/flags/bitrate/packetloss`）的 **偏移量在两端逐一核对无误**（Kotlin 侧 `byteArrayOf` 索引 3/4/5/6/7-8/9 ↔ Rust 侧 `payload[3]/[4]/[5]/[6]/[7..9]/[9]`）。
2. **反向链路恒为 48 kHz 立体声**：`capture.rs:159-163`（`Hz48000` + `Stereo`）、`capture.rs:232`（头里硬写 `SAMPLE_RATE_48K`）、`AudioPlayer.kt:19`（`SAMPLE_RATE = 48000`）三处一致，故不存在采样率错配 —— 但这也意味着 P2-13 的界面标签必然错误，且 `OpusDecoder(sampleRateHz)` 的参数化是"假参数化"（唯一调用点 `MainActivity.kt:112` 写死 48000）。
3. **`bitrate` 头字段是纯装饰**：`compute_bitrate_id` 上限 255（即 510 kbps，512 无法表示，实测见附录 C `[7]`），且接收端从不读取该字段。若未来要让码率参与决策，需先修正编码方案。
4. **协议无任何认证**：`device_id` 在 `DISCOVER_REPLY` 中明文广播，`CONNECT` 仅做"ID 相等"判断且不校验来源 IP。因此 `main.rs:1119-1120` 注释所称"防止第三者恶意抢占"只在"攻击者不知道受害者 ID"时成立 —— 而该 ID 是公开的。对局域网自用工具可接受，但不应当作为安全属性对外描述。

---

## 四、修复优先级建议

| 顺序 | 内容 | 理由 |
|------|------|------|
| 1 | **P0-3** 换掉入库的 release 密钥 | 唯一会危及用户设备安全的项，且越晚处理代价越高（涉及换钥/存量用户） |
| 2 | **P0-1 + P0-2** 解析 `WAVEFORMATEX` + 处理 SILENT 标志 | 同属 `read_loopback_stereo` 一处改造，可一次性消除 UB 与噪声；改动局部、风险可控 |
| 3 | **P1-1 + P2-8** 让错误可见 + 失败后状态收尾 | 只需渲染一个 `text(&self.status_text)`，却能立即消除大量"玄学问题"的排查成本 |
| 4 | **P1-4** 重排缓冲的有符号距离判定 | 三行改动，消除可听伪影，属纯逻辑、无副作用 |
| 5 | **P1-8 + P1-10** 反向协程退出条件 + JNI 同步 | 一个耗电空转，一个潜在闪退，都在同一条链路上 |
| 6 | **P1-9** 反向播放改为非阻塞 + 漂移补偿 | 改动量最大，但是"长时间使用后音画不同步/断流"的根因 |
| 7 | 其余 P2/P3 | 可在常规迭代中随模块重构消化 |

---

## 附录 A：`cargo clippy --all-targets` 输出

```
build.rs:56:29:   warning: this `map_or` can be simplified
src\main.rs:259:13: warning: you seem to be trying to use `match` for destructuring a single pattern
src\main.rs:605:18: warning: clamp-like pattern without using clamp function
src\main.rs:695:49: warning: casting to the same type is unnecessary (`f32` -> `f32`)
src\main.rs:747:52: warning: this `if` has identical blocks
src\main.rs:952:5:  warning: you seem to be trying to use `match` for an equality check
warning: `udp2cal` (bin "udp2cal") generated 5 warnings (2 duplicates)
```

`cargo check` 无任何告警通过，说明**不存在编译级缺陷**，本报告全部问题均为逻辑/契约/健壮性层面。

## 附录 B：已排除的疑似问题（避免误报）

| 疑似问题 | 结论 | 依据 |
|----------|------|------|
| `deviceIdToString` 的 `"%02x".format(byte)` 对负字节会输出 `-1`，导致持久化 ID 长度不等于 16、每次启动重新生成 | **不成立** | JDK 25 实跑：`{(byte)0xFF,(byte)0x80,(byte)0x7F,...}` → `"ff807f01a500fe10"`（长度 16）。`java.util.Formatter` 对负数 byte 会按 2⁸ 补偿，与 `& 0xFF` 等价 |
| Kotlin `seqNum: Byte` 自增在 127 处溢出，与 Rust `u8` 回绕语义不一致 | **不成立** | Kotlin `Byte++` 与 `u8::wrapping_add` 均为模 256 回绕；实测 254→255→0→1 正常输出 |
| `compute_bitrate_id(512) = 255`（只能表示 510 kbps）是码率协商缺陷 | **不降级为缺陷** | 接收端从不读取 `bitrate` 字段（`main.rs` 无 `h.bitrate` 引用），当前无功能影响，仅记为 §3 第 3 条 |

## 附录 C：重排缓冲实测复现

```bash
# 独立工程，不修改原仓库
mkdir -p /tmp/udp2cal_audit/src && cd /tmp/udp2cal_audit
# Cargo.toml 仅声明 bin；src/main.rs 用 #[path] 直接引入原 protocol.rs
#   #[path = "C:/Users/.../UDP2CAL/protocol/protocol.rs"] mod protocol;
cargo run -q --offline
```

输出（节选）：

```
[1] 顺序包        -> ["0","1","2","3","4"]
[2] 重复包        -> ["0","1","1","2"]        ← P1-4：重复帧被播放
[3] 迟到包        -> ["0","1","2","3"]        ← 单包位移可正常重排
[4] 双乱序(1,2,0) -> ["2","0"]                ← P1-4：乱序 + 丢帧
[5] 丢包(2,3)后    -> ["0","1","4","5"]       ← 无 PLC，直接跳空
[6] 序号回绕      -> ["254","255","0","1"]    ← 回绕正确
```

---

*本报告仅做问题认定与建议，未对仓库做任何代码改动。*

---

## 五、修复记录（2026-10-23）

> 本节为修复后追加。以下 43 项**已全部处理**，并完成回归验证：

| 验证项 | 结果 |
|--------|------|
| `cargo check`（windows） | 通过，0 告警 |
| `cargo clippy --all-targets`（windows） | **0 告警**（修复前 5 条） |
| `cargo test`（windows） | **4 项通过**（Ring 出入队/满丢弃、首帧比率、同率直通） |
| `cargo test`（protocol） | **20 项通过**（新增重复包丢弃、迟到丢弃、大跳变重同步、首包基准、连续迟到强制重同步） |
| 独立复现工程复测 | 重复包不再二次播放；`(1,2,0)` 不再乱序输出 |
| `:app:compileDebugKotlin` | BUILD SUCCESSFUL |
| `:app:assembleDebug` | BUILD SUCCESSFUL（含 NDK/JNI） |
| `:app:assembleRelease` | BUILD SUCCESSFUL，产出 `app-release-unsigned.apk`（确认不再使用入库调试密钥） |

### 5.1 P0 / P1

| 编号 | 修复方式 | 文件 |
|------|----------|------|
| P0-1 | 新增 `parse_mix_format()` 解析 `WAVEFORMATEX`/`WAVEFORMATEXTENSIBLE`（format tag、位深、声道数），按 f32/i16/i32 × N 声道转换为固定立体声；不支持的格式直接失败而非越界读 | `windows/src/capture.rs` |
| P0-2 | `read_stereo()` 检查 `AUDCLNT_BUFFERFLAGS_SILENT`，置位或指针为空时输出等长零样本 | `windows/src/capture.rs` |
| P0-3 | 签名改为从 `local.properties`/环境变量读取；移除仓库内密钥与 `"android"` 口令；`git rm --cached` + `.gitignore`；未配置密钥时回落到**本机** `~/.android/debug.keystore`（不入库）并打印警告，保证 APK 仍可安装自测 | `android/app/build.gradle.kts`、`.gitignore` |
| P1-1 | 新增 `notice` 字段与界面提示行（错误红色/信息蓝灰）；绑定失败与初始化失败改发 `Message::EngineError`，自动回到停止态并复位广播 | `windows/src/main.rs` |
| P1-2 | `resolve_device_id()` 用 `OnceLock` 保证只生成一次并立即落盘；`save()` 只写缓存值，不再重生成 | `windows/src/config.rs` |
| P1-3 | Run 项写入 `"路径"` 形式；`set_auto_start()` 返回 `Result`，失败回滚开关并提示 | `windows/src/config.rs`、`main.rs` |
| P1-4 | 重排缓冲区分"落后"与"超前"：迟到/重复包丢弃、真跳变重同步、首包作为基准、连续迟到超阈值强制重同步 | `protocol/protocol.rs` |
| P1-5 | 引入无锁 SPSC 环形缓冲（`AtomicU32` 承载 f32，全安全代码）；音频回调不再取锁，生产侧不再在锁内扩容/搬移 | `windows/src/audio.rs` |
| P1-6 | 首次 `write()` 即按 `source/device` 计算 `effective_ratio`，消除开头 3 秒变速 | `windows/src/audio.rs` |
| P1-7 | 按 `supported_config.sample_format()` 分派 f32/i16/u16/i32 建流；配置只在目标设备上枚举 | `windows/src/audio.rs` |
| P1-8 | 反向协程捕获到 socket 关闭/异常时 `break`（含 20ms 退避）；热重连显式取消并复位反向状态，待新 ACK 后重建 | `CaptureService.kt` |
| P1-9 | 播放改为本地队列 + `WRITE_NON_BLOCKING` + 高水位裁剪（240ms 触发、回落到 120ms），接收协程永不被阻塞 | `AudioPlayer.kt` |
| P1-10 | `OpusDecoder` 的 `start/decode/stop` 全部 `@Synchronized`，并校验 offset/length | `native/OpusDecoder.kt` |

### 5.2 P2

| 编号 | 修复方式 |
|------|----------|
| P2-1 | ACK 复用监听 socket（`socket.send_to().await`），不再每次保活新建 socket |
| P2-2 | 防火墙规则限定 `program="<exe>"` + `localport=44043,<端口>`，不再 `localport=any` |
| P2-3 | 规则名改为无空格 ASCII（`UDP2CAL-In-UDP`），规避 netsh 自身解析截断 |
| P2-4 | `find_window_for_process` 只接受"可见 + 有标题"的顶层窗口 |
| P2-5 | `ReverseSender::stop_and_join()` 消费式 join，所有停止点统一调用，消除新旧线程重叠 |
| P2-6 | 编码结果 `n > MAX_PAYLOAD` 直接丢弃，不再 `min(1472)` 截断；发送错误计数并打印首条 |
| P2-7 | `ComGuard` 在 `Drop` 中 `CoUninitialize()`，与 `CoInitializeEx` 严格配对 |
| P2-8 | 等待上限提升到 3 秒；失败改走 `EngineError`；启动时可重试 `init_audio_worker()`（通道可重建） |
| P2-9 | `GLOBAL_DEVICE_STATE` + `BOUND_DEVICE_ID` 合并为 `P2pState` + 单锁，状态与 ID 同时可见 |
| P2-10 | `notificationJob` 去重，`startForeground` 不再随启动次数线性放大 |
| P2-11 | JNI 侧判空 + `offset/length`/PCM 容量校验，失败即返回 -1 |
| P2-12 | 反向路径按来源 IP 过滤，非目标 PC 的包直接丢弃 |
| P2-13 | 反向卡片改用 `status.reverseBw`（真实反向带宽） |
| P2-14 | 删除 `deviceId` / `connected` 两处死状态 |
| P2-15 | 移除 `FOREGROUND_SERVICE_SPECIAL_USE`、`REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` |
| P2-16 | 返回 `START_NOT_STICKY`；`intent == null` 时直接 `stopSelf()` |
| P2-17 | `checkReleaseBuilds` 恢复为 true；`abortOnError` 暂保持非阻断；同时禁用与 Kotlin 2.0 不兼容的 `NullSafeMutableLiveData` 检查（见 §6.1） |
| P2-18 | windows 新增 4 项单测、protocol 新增 5 项；Android 侧仍无 JVM 测试基建（见 §5.4） |

### 5.3 P3

| 编号 | 修复方式 |
|------|----------|
| P3-1 | 删除同分支的 `if`，统一为 `mk("反向串流", …)` |
| P3-2 | 用 `rev_on: bool` 取代 `Color` 浮点相等判断 |
| P3-3 | 改为 `clamp(-60.0, 0.0)` |
| P3-4 | `recv_from` 改 `if let`；`handle_control` 改为 `if msg_type == TYPE_CONNECT` |
| P3-5 | 设备 ID/主机名进程内缓存；监听端口改用 `LISTEN_PORT` 原子量，广播线程不再读注册表 |
| P3-6 | 窗口高度 290 → 340，容纳提示行 |
| P3-7 | 单实例命中时先 `FindWindowW("UDP2CAL")` 还原并置前 |
| P3-8 | `opt-level` 改 3（实时音频路径优先速度） |
| P3-9 | proguard 包名修正为 `com.udp2cal.app.native`；移除不可靠的 `finalize()` |
| P3-10 | ACK 改独立线程（500ms 超时）、目标地址连接时解析一次、Prefs 同步降为 5Hz |
| P3-11 | `android/local.properties` 移出版本库并加入 `.gitignore` |
| P3-12 | 移除强制最高刷新率设置 |

### 5.4 本次未覆盖（需单独立项）

1. **协议无认证**：`device_id` 在 `DISCOVER_REPLY` 中明文广播，局域网内可伪造 CONNECT 抢占绑定。本次只加了反向来源 IP 过滤，属缓解而非根治；根治需要 v3 协议引入随机挑战/密钥。
2. **`bitrate` 头字段仍无消费者**：保留字段语义，未做编码方案变更。
3. **Android 单元测试基建缺失**：新增 `testImplementation(junit)` 需联网拉依赖，当前离线环境无法验证，故未落地；建议后续补 `Udp2CalProtocol` 与 `AudioPlayer.ShortQueue` 的 JVM 测试。
4. **`CaptureStatus.deviceId`** 已无 UI 消费者，但作为状态契约字段保留。

---

*修复后复核：`cargo check`/`clippy` 零告警，Rust 侧 24 项、Android 侧构建与打包均通过。*

---

## 六、修复后追加发现（同日 19:30 补记）

恢复 release lint 门禁后，暴露出两个**与审查项无直接关系、但会阻断发布构建**的问题。二者均已修复并实测通过。

### 6.1 `lintVitalAnalyzeRelease` 崩溃（工具链不兼容）

- **现象**：`IncompatibleClassChangeError: Found class KaCallableMemberCall, but interface was expected`，崩溃点为 `androidx.lifecycle.lint.NonNullableMutableLiveDataDetector`。
- **根因**：AGP 8.7 内置 lint 31.7 链接的 Kotlin 分析 API 与项目使用的 Kotlin 2.0.20 不兼容（K2 把 `KaCallableMemberCall` 由接口改为类），属 Google issue 374783344；此前 `checkReleaseBuilds = false` 完全跳过 lint，因此从未暴露。
- **处置**：`android { lint { disable += "NullSafeMutableLiveData" } }`。本项目为纯 Compose，无任何 `LiveData` 使用，该检查对本工程无意义。已注明"升级 AGP ≥ 8.8.2 后删除此行"。
- **验证**：`assembleRelease` 中 `lintVitalAnalyzeRelease` / `lintVitalRelease` 均通过。

### 6.2 `build.bat ... --proxy` 在 Windows 上必失败

- **现象**：`'127.0.0.1' 不是内部或外部命令`，构建在 Gradle 启动前即中断（退出码 255）。
- **根因**：`scripts/udp2cal/proxy.py::jvm_args()` 生成的 `-Dhttp.nonProxyHosts=localhost|127.0.0.1|::1` 被写入 `GRADLE_OPTS`，由 `gradlew.bat` 在 cmd 中展开时，`|` 被当作**管道符**，于是 cmd 尝试执行 `127.0.0.1`。这是命令注入面的经典形态，任何带 `|`、`&`、`^` 的参数都会踩到。
- **处置**：为该参数整体加引号（`"-Dhttp.nonProxyHosts=..."`），cmd 展开时不再解析管道符；JVM 侧会自动去引号。
- **验证**：`build.bat android --modern --abi arm64-v8a --release --proxy http://127.0.0.1:10808` → `BUILD SUCCESS`，产出 `udp2cal_arm64-v8a_Release.apk`（2.68 MB，v2/v3 已签名，`CN=Android Debug`）。

### 6.3 签名回落的升级兼容性提示

旧版 APK 使用仓库内 `lanmic-debug.keystore`（alias `lanmic`）签名；本次改为"未配置则用本机调试密钥"。**两者的签名证书不同**，因此：

- 已装旧版 APK 的设备直接覆盖安装会报 `INSTALL_FAILED_UPDATE_INCOMPATIBLE`，需先 `adb uninstall com.udp2cal.app`；
- 若希望保持覆盖升级，可在 `android/local.properties` 中把旧密钥配为发布密钥（该文件已不受版本控制）：

```properties
RELEASE_STORE_FILE=lanmic-debug.keystore
RELEASE_STORE_PASSWORD=android
RELEASE_KEY_ALIAS=lanmic
RELEASE_KEY_PASSWORD=android
```

> 注意：这等于继续使用公开密钥，仅适合本机自测；对外分发仍应换用独立密钥（届时本来就需要用户重装）。
