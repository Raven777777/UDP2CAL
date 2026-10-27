# Sunshine 为什么在局域网里又快又稳 —— 源码级拆解

> **归档材料（2026-10-28 归档）**。外部项目调研，**不是**本项目的规范、路线图或对照结论；
> 现行真源仍是 [../PROTOCOL_V3_RTP.md](../PROTOCOL_V3_RTP.md) 与 [../LATENCY_CALIBRATION.md](../LATENCY_CALIBRATION.md)。
> 与本项目（音频串流、无视频）的逐项对照见 [PLAN_SUNSHINE_ALIGNMENT_2026-10-28.md](PLAN_SUNSHINE_ALIGNMENT_2026-10-28.md)。
>
> 调研对象：`github.com/LizardByte/Sunshine`，commit `d0b4a88`，浅克隆于 `C:\Users\21248\Documents\GitHub\sunshine`。
> 所有结论都带 `文件:行` 证据（行号为该 commit）。标注「推测」的部分依赖未 checkout 的子模块 `third-party/moonlight-common-c`（线格式常量定义在那里）。

## 一句话结论

Sunshine 的快不是靠"某个 zerolatency 开关"，而是**用结构消灭排队**：

1. **编码管线压到恰好 1 帧**（无 B 帧、无 lookahead、无限 GOP、VBV = 1 帧）；
2. **采集→编码→发送每一级都是"最新帧胜出"**（单槽覆盖 / 队列满即清空），延迟不可能累积；
3. **发送端自己按 1 ms 粒度整形**，把 bufferbloat 从交换机挪进可控定时器；
4. **带内 RS-FEC 默认 20%** + RTSP 阶段预先扣掉冗余带宽，靠冗余而非重传来吸收 Wi-Fi 丢包；
5. 运行时**没有 ABR / 拥塞控制**——码率帧率在 `ANNOUNCE` 时一次定死，唯一的运行时闭环是"请求 IDR / 参考帧失效"。

---

## 一、编码层：把延迟压到 1 帧

通用规则（FFmpeg 路径，所有编码器共用）：

| 项 | 值 | 证据 |
|---|---|---|
| B 帧 | 恒 0 | `src/video.cpp:2055-2056` `// B-frames delay decoder output, so never use them` / `ctx->max_b_frames = 0;` |
| GOP | 无限（`std::numeric_limits<int>::max()`）；不支持按需 IDR 的后端才用 120 | `src/video.cpp:2060-2069` |
| 标志 | `AV_CODEC_FLAG_LOW_DELAY \| CLOSED_GOP` + `AV_CODEC_FLAG2_FAST` | `src/video.cpp:2086-2089` |
| VBV | `rc_buffer_size = bitrate / framerate`（**恰好 1 帧**）；软编多 slice/HEVC 放宽到 1.5 帧 | `src/video.cpp:2235-2247` |
| 码率 | `bitrate = min(client, max_bitrate) * 1000`，`rc_min_rate = rc_max_rate = bit_rate`（CBR） | `src/video.cpp:2218-2226` |
| slice | 硬件用客户端值，软件 `max(client, min_threads=2)` | `src/video.cpp:2149-2160`、`src/config.cpp:743` |

NVENC（Windows 直连 SDK）再叠一层：

```cpp
enc_config.gopLength = NVENC_INFINITE_GOPLENGTH;
enc_config.frameIntervalP = 1;                              // 无 B 帧、无重排
enc_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
enc_config.rcParams.zeroReorderDelay = 1;                   // 编码器内部不缓存重排
enc_config.rcParams.enableLookahead = 0;                    // 无前瞻
enc_config.rcParams.lowDelayKeyFrameScale = 1;
enc_config.rcParams.vbvBufferSize = client_config.bitrate * 1000 / client_config.framerate;  // 1 帧
```
`src/nvenc/nvenc_base.cpp:258-280`；另外 `tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`、`preset = P1`（默认 `quality_preset = 1`）、`sliceMode = 3`（按帧切片数）、参考帧限 1 帧。

AMD 侧等价参数：`{"latency", 1}` + `{"async_depth", 1}` + `frame_skipping=0`（`src/video.cpp:996-1074`）。VAAPI：`async_depth=1`、`idr_interval=int_max`、`low_power=1`（`src/video.cpp:1249-1280`、`src/platform/linux/vaapi.cpp:287`）。

**为什么有效**：延迟 = 采集 1 帧 + 编码 1 帧 + 传输 1 帧，没有重排缓冲、没有前瞻缓存；无限 GOP 让码流不必周期性塞大 I 帧，于是带宽曲线平——这直接决定了"超高码率"下不会因为 IDR 尖峰引起瞬时排队。

## 二、采集与节拍：不让帧堆积

**只降不升帧率**（关键的一行注释）：
```cpp
// Can only decrease requested fps, otherwise client may start accumulating frames and suffer increased latency.
if (client_frame_rate > candidate_rate && candidate_rate / client_frame_rate > 0.99) { ... }
```
`src/platform/windows/display_base.cpp:238-246`

**绝对时刻调度，误差不累积**：
```cpp
const auto sleep_target = *frame_pacing_group_start + nanoseconds(1s) * seconds + nanoseconds(1s) * remainder / rate.Numerator;
if (sleep_period <= 0ns) { frame_pacing_group_start = std::nullopt; status = capture_e::timeout; }  // 错过就作废整组
```
`src/platform/windows/display_base.cpp:277-296`，组起点用**采集帧时间戳**（`:311`）而不是 `now()`。另有独立的 `"Sleep overshoot"` 统计项。

**单槽覆盖代替队列**：采集→编码用 `safe::event_t`，`raise()` 直接覆盖旧值（`src/thread_safe.h:38-49`、`src/video.h:124`、`src/video.cpp:1717`）。编码器慢 → 采集帧被静默丢弃，**永不排队**。

**空闲不断流**：`images->pop(max_frametime)` 超时就跳过 convert、直接重编上一帧（dupe），`minimum_fps_target` 默认 `framerate/2`（`src/video.cpp:2437-2483`）；dupe 帧的 `frame_processing_latency` 记 0。

**零拷贝**（Windows+NVENC 路径）：WGC 的 `ID3D11Texture2D` → `CopyResource` 到 NT-handle + keyed-mutex 共享纹理（`display_vram.cpp:1897,1977,2006`）→ 编码器 `OpenSharedResource1`（`:1003`）→ HLSL shader 直接渲染成 NVENC 已注册的 NV12 纹理（`:514-556`）→ `nvEncRegisterResource`（`nvenc_d3d11_native.cpp:31`）。`img->data` 存的是**纹理指针而不是像素**（`display_vram.cpp:2023`）。只有压缩后的码流回读 CPU。

## 三、网络层：整形 + 冗余

**打包**：帧前加 8 字节 `video_short_frame_header_t`（`stream.cpp:100-130`），按 `packetsize` 切片，每片 28 或 32 字节头；`packetsize` 由客户端给，服务端默认 0=跟随，`PACKETSIZE_MIN/MAX = 200/65535`（`config.h:37-40`）。

**批量发送（GSO/USO）**：Linux `sendmsg + UDP_SEGMENT`（≤64 段，`platform/linux/misc.cpp:759-883`，失败回退 `sendmmsg`）；Windows `WSASendMsg + UDP_SEND_MSG_SIZE`（`platform/windows/misc.cpp:1562-1576`）。批大小上限 64 包且单批 <64 KB（注释说明：Windows 上超过 64 KB 会绕过 `SO_SNDBUF`、退化成等中断）——`src/stream.cpp:1675-1683`。

**socket 选项**：视频 socket `SO_SNDBUF = 1 MiB`（`stream.cpp:1994`），失败有显式错误日志；接收侧不调 `SO_RCVBUF`。

**DSCP 打标默认开**：视频 DSCP 40（CS5）、音频 48（CS6），并设 `SO_PRIORITY` 5/6（`platform/linux/misc.cpp:1079-1096`）；Windows 走 `IP_TOS`（`platform/windows/misc.cpp:1663+`）。客户端 `qosTrafficType` 默认 `"5"`/`"4"`（`rtsp.cpp:1084-1085`），所以默认就打标。

**1 ms 粒度节拍**（真正的抗 bufferbloat 手段）：
```cpp
// Use around 80% of 1Gbps
size_t ratecontrol_packets_in_1ms = std::giga::num * 80 / 100 / 1000 / blocksize / 8;
auto due = ratecontrol_frame_start + nanoseconds(1ms) * ratecontrol_frame_packets_sent / ratecontrol_packets_in_1ms;
if (now < due) timer->sleep_for(due - now);
```
`src/stream.cpp:1673,1786-1794`；余量跨帧结转（`:1831-1834`）；定时器在 Windows 用 `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`（`platform/windows/misc.cpp:1810-1847`），创建失败直接放弃广播线程（`stream.cpp:1553-1557`）。注意这是**硬编码 800 Mbps 上限**，不是按用户码率的令牌桶——低于这个速率时节拍器几乎不 sleep，真正的码率闸门在编码器 CBR + 1 帧 VBV。

**FEC：Reed-Solomon（GF(256)），带内，和数据包同端口同序号**
```cpp
auto parity_shards = (data_shards * fecpercentage + 99) / 100;
if (parity_shards < minparityshards && fecpercentage != 0) { parity_shards = minparityshards; fecpercentage = (100u * parity_shards) / data_shards; }
rs = reed_solomon_new(data_shards, parity_shards); reed_solomon_encode(...);
```
`src/stream.cpp:858-921`。默认 `fec_percentage = 20`，范围 0–100（`config.cpp:839,1828`）；帧超过 `DATA_SHARDS_MAX` 时按最多 **4 个 FEC block** 切分，超了就打 warning 并**放弃该帧 FEC**（`stream.cpp:1621-1644`）；单帧分片数 ≥1024 时打 "Encoder produced a frame too large to send!"（`:1654`）。音频是独立的 `AUDIO_FEC_HEADER` 块（4 数据 + 2 校验，包数来自 moonlight 头，推测）。

**带宽预算在 RTSP 阶段就扣干净**（避免超卖）：
```cpp
if (config::stream.fec_percentage <= 80) { configuredBitrateKbps /= 100.f / (100 - config::stream.fec_percentage); }
configuredBitrateKbps -= std::min((std::int64_t) audioBitrateAdjustment, configuredBitrateKbps / 5);   // 音频，≤20%
configuredBitrateKbps -= std::min((std::int64_t) 500, configuredBitrateKbps / 10);                     // 协议开销，≤10%
config.monitor.bitrate = (int) configuredBitrateKbps;
```
`src/rtsp.cpp:1205-1223`。三处都带"最多扣多少"的兜底，防止把码率扣到几乎为零。

**加密默认关**：`lan_encryption_mode` 默认 0（NEVER），可选 AES-GCM 逐包加密（每个分片 +32 字节前缀）。LAN 场景不付这笔钱（`config.cpp:844`、`stream.cpp:331-334,1762-1777`）。

## 四、"稳定"到底指什么：容量 + 冗余 + 请求式修复

| 机制 | 实现 | 证据 |
|---|---|---|
| 队列溢出 | 32 槽，溢出策略 `drop_oldest` **整队清空**（不是丢最旧一条） | `thread_safe.h:401-404,432-437,849` |
| 音频 PCM 上限 | 30 帧 × 5 ms = 150 ms | `audio.cpp:237` |
| 丢包自愈 | 客户端发 `IDX_REQUEST_IDR_FRAME` → `idr_events->raise(true)` → 下一帧强制 IDR（NVENC `NV_ENC_PIC_FLAG_FORCEIDR`）；`IDX_INVALIDATE_REF_FRAMES` → NVENC 精确失效参考帧，不支持时降级为 IDR | `stream.cpp:1206-1223`、`video.cpp:2468-2473,589-604` |
| 会话存活 | 任意 ENet 事件都续 `pingTimeout`，超时默认 10 s → `session::stop()`；视频/音频通道还有"首包 ping 门禁" | `stream.cpp:776,1342-1346,2075-2135` |
| 发送错误 | Linux `EAGAIN` → `poll(POLLOUT)` 等可写再重试；批量失败回退逐包；`SO_SNDBUF` 设置失败显式报错 | `platform/linux/misc.cpp:943-1020`、`stream.cpp:1993-1997` |
| 线程优先级 | 采集/音频采集/控制线程 `critical`（Windows `THREAD_PRIORITY_HIGHEST`），编码/发送 `high`；**无 CPU 亲和性、无 SCHED_FIFO**；Windows 检查返回值并 warning | `video.cpp:1703`、`audio.cpp:235`、`stream.cpp:1319,1543`、`platform/windows/misc.cpp:1135-1160` |

**音频很有趣**：Opus 5 ms 帧（客户端 `x-nv-aqos.packetDuration` 默认 `"5"`，**没有任何范围校验**）、`OPUS_APPLICATION_RESTRICTED_LOWDELAY`（CELT-only）+ `OPUS_SET_VBR(0)` 强制 CBR、**不开 in-band FEC**，抗丢包完全交给带外 RS 块（`audio.cpp:120-132`、`rtsp.cpp:1079,1099`）。服务端**没有 jitter buffer**，时钟同步外包给 Windows 的 `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`（`platform/windows/audio.cpp:398`）——抗抖全部由客户端 Moonlight 负责。

## 五、它明确"没有做"的事（重要）

- **没有 ABR / 拥塞控制**：客户端的丢包上报 `IDX_LOSS_STATS` 被解析后**只打了一行 verbose 日志**，`lastGoodFrame` 读出来再没用过（`stream.cpp:1190-1204`）。没有 GCC/BBR 之类算法。
- **没有"最大延迟"判据**：全仓库 grep 不到 `max_frame_latency` / `frame_checker` / EWMA；延迟只被**观测**——写入私有扩展头 `frame_processing_latency`（0.1 ms 单位，`uint16` 饱和到 6553.5 ms，`stream.cpp:113-116,1599-1607`）并做 min/max/avg 日志（20 s 窗口，`stat_trackers.h:57-76`）。防积压靠的是容量为 1 的数据结构和 1 ms 整形，不是"测出来再调"。
- **会话中不再重协商**：码率/帧率/FEC/切片/加密全部在 RTSP `ANNOUNCE` 一次定死（`rtsp.cpp:1136-1154`）。
- **`packetDuration` 无范围校验**（对比 `packetsize`、`fec_percentage` 都有 `int_between_f`），算一个健壮性缺口。
- **macOS 的 QoS 与 Linux 的 `pthread_setname_np` 返回值没检查**，与 Windows 侧的严谨度不一致。

## 六、关键数字一览

| 参数 | 默认 | 范围 | 作用 |
|---|---|---|---|
| `fec_percentage` | 20 | 0–100 | 冗余比例；>80 时跳过码率反算 |
| `packetsize` | 0（跟随客户端） | 0–65535，警告阈 500/1456 | 分片大小 |
| `lan_encryption_mode` | 0（不加密） | 0–2 | AES-GCM 逐包 |
| `video.max_bitrate` | 0（不限） | 无 clamp | 只做 `min(client, value)` |
| `minimum_fps_target` | 0（= fps/2） | 0–1000 | dupe 帧触发间隔 |
| `min_threads` | 2 | 无 clamp | 软编 slices 下限 |
| `ping_timeout` | 10 s | — | 会话超时 |
| 音频帧长 | 5 ms | 无校验 | Opus 240 samples |
| 发送节拍 | ~800 Mbps 上限 | 硬编码 | 1 ms 粒度 sleep |
| 视频 `SO_SNDBUF` | 1 MiB | — | 吸收批量发送 |

## 七、可借鉴的三条（对 UDP2CAL 这类自建串流同样成立）

1. **"不排队"胜过"测延迟"**：单槽覆盖 + 队列满即全清，让延迟在任意负载下都钉在"一个处理周期"，代价是丢帧。阈值式方案（超过 X ms 才丢）在阈值以下会安静地积压，阈值以上才救火——本质是在赌"永远不到阈值"。
2. **发送端自己做整形**：1 ms 槽位摊平一帧的包，让网络瞬时速率 ≈ 平均速率。家用 AP/交换机里排队的是几十毫秒，自己的定时器里排队是微秒。配套要有高精度定时器（`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`）和"定时器建不出来就别发"的硬失败。
3. **绝对时刻调度 + 只降不升**：`组起点 + n×帧间隔` 而非 `sleep_for(帧间隔)` 累加，抖动不累积；错过就作废整组重新锚定。统计上刻意只用 min/max/avg，用 min/max 的差看抖动、avg 看趋势，不引入 EWMA 系数（少一个会被调错而掩盖问题的旋钮）。

反面教训同样值得记：**Sunshine 放弃了丢包反馈闭环**（`LOSS_STATS` 死路），所以"稳"完全依赖静态 FEC 比例 + 静态 CBR + 用户自选码率。这在局域网（丢包近似独立同分布）够用，但换到无线干扰变化剧烈的环境，就只能靠用户手动把 `fec_percentage` 调高、把码率调低。
