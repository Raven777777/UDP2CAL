# UDP2CAL 协议 v3：RTP/Opus 媒体 + 控制面

> **状态：已实现（v1.3.0）。**
> 本文档描述 v3 的线格式与接收端时钟模型；`protocol/`、`windows/`、`android/` 均已按本文实现，
> v2 代码路径已删除（`archive/PROTOCOL_V2.md` 仅作历史存档）。
>
> - 控制协议版本：**3**
> - 媒体承载：**标准 RTP/AVP，RTP version 2**
> - 适用范围：Android ↔ Windows，局域网 / 点对点，单向或双向实时音频
> - 协议真源：`protocol/protocol.rs`（控制面） + `protocol/rtp.rs`（媒体面 / 抖动缓冲 / 播放头）
> - 相关文档：[v2 协议（历史）](archive/PROTOCOL_V2.md) · [架构说明](ARCHITECTURE.md)
>
> 本文使用 RFC 2119 风格的关键词：**必须（MUST）**、**不得（MUST NOT）**、**应该（SHOULD）**、**可以（MAY）**。

---

## 一、目标与非目标

### 1.1 目标

1. 媒体面改用标准 RTP 线格式，不再自定义音频包头。
2. 接收端以**声卡播放头**作为唯一音频时钟；音频路径上不设置定时器。
3. 发送端**无状态、零应用层音频队列**：采集 → 编码 → 立即发送，不等 ACK、不重传、不积压。
4. 保留 512 kbps 以上码率能力，因此固定使用 **10 ms Opus 帧**。
5. 控制面保留私有协议，版本升到 v3；控制包与正向 RTP 共用主 UDP socket。
6. 彻底切断 v2 兼容：Android 与 Windows 必须同时升级。

### 1.2 非目标

以下内容明确不在 v3 范围内：

- SIP、ICE、STUN、TURN、rtpengine、媒体服务器、中继、多人会议；
- RTCP、SRTP、NACK、重传、拥塞控制、带宽探测；
- 公网穿透、NAT 穿越、IPv6 双栈；
- 音频加密与身份认证；
- 动态 Opus 帧长协商（v3 固定 10 ms）；
- 与 v2 客户端互操作。

### 1.3 核心原则

| 原则 | 说明 |
|------|------|
| RTP 时间戳不是播放时钟 | 它只标记媒体时间线，不能用来决定“现在该播放哪一帧” |
| 播放头是唯一时钟 | 声卡实际消费到哪个 RTP 时间戳，就播到哪里 |
| 等待用水位表达 | 接收端不 `sleep`、不设置播放定时器；缓冲不足就等待，水位够了就继续 |
| 发送端不排队 | 采集线程直接编码并发送；发送失败就丢帧并计数，绝不补发 |
| 丢包由播放头触发 PLC | 只有播放头走到缺口时才生成 PLC，而不是由网络线程或定时器触发 |
| 干净断代 | 控制版本 3，媒体 RTP v2；不保留 v2 双栈 |

---

## 二、版本与兼容性

| 项目 | v3 取值 |
|------|---------|
| 控制协议版本 | `0x03` |
| RTP 版本 | `2` |
| Opus payload type | `111` |
| Opus 采样率 | `48000 Hz` |
| Opus 帧长 | `10 ms` |
| 每帧采样数 | `480` |
| 兼容 v2 | 否 |

- 控制包首字节必须为 `0x03`；否则丢弃。
- RTP 包必须满足 RTP version 2；否则丢弃。
- 不进行版本协商、不降级、不探测旧版本。
- v2 文档保留作历史参考，但 v2 客户端不能连接 v3 客户端。

---

## 三、传输层

### 3.1 端口与 socket

| 用途 | 方向 | 本地端口 | 远端端口 |
|------|------|---------|---------|
| 发现广播 | Android → 局域网 | 临时端口 | `44043` |
| 控制 + 正向 RTP | Android → Windows | Android 主 socket 临时端口 | Windows 监听端口（默认 `44044`） |
| 控制 ACK | Windows → Android | Windows 监听端口 | Android 主 socket 临时端口 |
| 反向 RTP | Windows → Android | Windows 临时端口 | CONNECT 中通告的 Android 反向端口 |

约定：

- Windows 主 socket 同时接收**控制包**和**正向 RTP**。
- Android 主 socket 同时发送**控制包**和**正向 RTP**，并接收 ACK。
- Android 另建一个反向接收 socket，端口在 CONNECT 的 `reverse_port` 中通告。
- Windows 反向发送使用独立临时 socket；其源端口可以是任意临时端口。
- 不建立 RTCP socket，不发送 RTCP。
- 所有端口均为 UDP。

### 3.2 同一 socket 上的解复用

接收端必须先看首字节，再决定解析路径：

```
首字节 b0:
  0x80..0xBF  -> RTP 包（b0 高两位为 10）
  0x03        -> 控制包 v3
  其他        -> 丢弃
```

RTP 的合法首字节在 v3 中实际上只有 `0x80`（V=2、P=0、X=0、CC=0）。先按 `0x80..0xBF` 识别，再严格校验 `b0 == 0x80`；不满足则丢弃。

控制包最小长度 15 字节；RTP 包最小长度 13 字节（12 字节头 + 至少 1 字节 Opus 负载）。**不得**用“长度是否达到 15 字节”来判断是否为控制包，因为 DTX 期间的 RTP 包可能很短。

### 3.3 源地址与身份校验

| 流量 | 校验规则 |
|------|---------|
| Android → Windows 控制 | `device_id` 必须等于当前绑定设备；源 IP/端口必须与 CONNECT 来源一致 |
| Android → Windows 正向 RTP | 源 IP/端口必须与 CONNECT 来源一致；SSRC 必须等于 CONNECT 中的 `forward_ssrc`；PT 必须为 111 |
| Windows → Android ACK | `device_id` 必须是已发现的 Windows；源地址必须是该 Windows |
| Windows → Android 反向 RTP | 源 IP 必须是 Windows 控制对端；SSRC 必须等于 ACK 中的 `reverse_ssrc`；源端口允许临时端口 |

说明：

- 正向 RTP 与 CONNECT 共用 Android 主 socket，因此源端口稳定；Windows 可以按“源 IP + 源端口”绑定。
- 反向 RTP 的源端口由 Windows 临时 socket 决定，不能按端口绑定；Android 只校验源 IP + SSRC。
- `device_id` 只用于控制面，不出现在 RTP 中。
- 这些校验只防误连和普通串流，不构成密码学安全；详见第 12 节。

### 3.4 MTU 与分片

- 单个 RTP 包最大 `1287` 字节（12 字节 RTP 头 + 1275 字节 Opus 负载）。
- 加 UDP 8 字节、IPv4 20 字节后最大 `1315` 字节，小于典型以太网 MTU 1500。
- 不得依赖 IP 分片；超过 1287 字节的 RTP 包必须丢弃。
- 若链路 MTU 小于 1500，v3 不做额外适配；这属于非目标场景。

---

## 四、RTP 媒体格式

### 4.1 RTP 头

v3 使用标准 RTP 固定头，12 字节，无 CSRC、无扩展、无填充。

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|V=2|P|X|  CC   |M|     PT      |       sequence number         |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                           timestamp                           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                             SSRC                              |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                            payload ...                        |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

| 字段 | 长度 | v3 取值 / 语义 |
|------|------|----------------|
| V | 2 bit | 必须为 `2` |
| P | 1 bit | 必须为 `0` |
| X | 1 bit | 必须为 `0` |
| CC | 4 bit | 必须为 `0` |
| M | 1 bit | 标记位；语义见 4.6 |
| PT | 7 bit | 必须为 `111`（Opus） |
| sequence number | 16 bit | 网络字节序；每发送一个 RTP 包递增 |
| timestamp | 32 bit | 网络字节序；48 kHz 媒体时间线 |
| SSRC | 32 bit | 网络字节序；每个方向每个会话独立，非零 |
| payload | 1..1275 字节 | Opus 包，不含任何 UDP2CAL 自定义头 |

校验规则：

- `b0 != 0x80` → 丢弃（P/X/CC 非零视为不支持）。
- PT != 111 → 丢弃。
- SSRC == 0 → 丢弃。
- 负载长度为 0 或大于 1275 → 丢弃。
- RTP 包总长度小于 13 或大于 1287 → 丢弃。

### 4.2 Opus 负载

- 编码格式遵循 RFC 6716；RTP 负载格式遵循 RFC 7587。
- 采样率固定 48 kHz；帧长固定 10 ms；每帧 480 个采样/声道。
- 当前实现：
  - **正向（Android 麦克风 → Windows）**：单声道；
  - **反向（Windows 环回 → Android）**：立体声。
- 声道数由 Opus 包 TOC 决定。接收端解码器必须按当前方向配置的声道数初始化；不得从 RTP 头或控制包推断声道数。
- 负载就是 Opus 包本身，不添加长度、序号、设备 ID、码率或采样率字段。
- DTX 静音包允许为 1–2 字节；不允许发送零长度 RTP 负载。
- FEC/DRED 只作为 Opus 带内冗余，不拆成独立 RTP 包。

### 4.3 时间戳

- 时钟频率固定 48 kHz。
- 每个 10 ms 音频帧，时间戳递增 `480`。
- 初始时间戳为随机 32 位值；不得从 0 开始作为固定约定。
- 时间戳是媒体时间线，不是墙钟，也不是播放时钟。
- 发送端必须为每一个 10 ms 采集帧推进时间戳，包括：
  - 正常发送的帧；
  - 编码失败/发送失败而丢弃的帧；
  - DTX 静音未发包的帧。
- 接收端必须用扩展计数器处理 32 位回绕；不得在回绕时清空抖动缓冲或重置播放头。
- 时间戳回绕周期约为 `2^32 / 48000 ≈ 24.85 小时`。

### 4.4 序列号

- 初始序列号为随机 16 位值。
- **序列号与时间戳同频推进：发送端每产生一个 10 ms 采集帧，序列号必须递增 1，
  与该帧是否真的发出无关。** 包括：正常发送的帧、编码失败/发送失败而丢弃的帧、
  DTX 静音未发包的帧、以及 ACK 超时暂停发送期间的帧。
- DTX 静音结束后恢复发送时，用 `marker=1` 表示静默结束。
- 序列号回绕周期约为 `2^16 × 10 ms ≈ 10.9 分钟`。
- 接收端必须用扩展计数器处理 16 位回绕；乱序窗口远小于 32768，因此可用有符号差值扩展。

> **为什么必须同频（2026-10-25 修订）**：接收端在断流/静音期间会用 PLC/静音推进
> `next_seq`。若发送端在此期间冻结序列号，恢复包的 `ext_seq` 会永久落后于
> `next_seq`，且滞后量随静音时长**单调增大**，永不闭合；恢复包会被"序号迟到"
> 判定拦下，`marker=1` 的重同步分支永远不可达 → 单向音频永久静音。
> 旧表述"DTX 不递增序列号"与接收端算法不自洽，已废止。

### 4.5 SSRC

- 每个方向、每个连接会话使用独立的随机非零 32 位 SSRC。
- 正向 SSRC 由 Android 生成，放在 CONNECT 的 `forward_ssrc` 中。
- 反向 SSRC 由 Windows 生成，放在 CONNECT_ACK 的 `reverse_ssrc` 中。
- 重连时必须生成新的 SSRC；旧 SSRC 的包必须被丢弃。
- 同一设备、同一 SSRC 的周期 CONNECT 是保活，不得重置抖动缓冲。
- SSRC 不是认证凭据；只能配合源地址校验使用。

### 4.6 标记位 M

`M=1` 的语义：

1. 连接建立后的第一个 RTP 包；
2. DTX/静音结束后的第一个 RTP 包；
3. 发送端发生时间戳不连续后的第一个 RTP 包；
4. 反向发送重新开始后的第一个包。

其余正常连续帧 `M=0`。

接收端不得只凭 `M=1` 就清空缓冲；必须结合时间戳跳变判断：

- `M=1` 且时间戳跳变大于 `200 ms`：执行重同步；
- `M=1` 但时间戳连续或跳变很小：只更新标记状态，不重置。

### 4.7 包长与码率

10 ms 帧下，码率与负载大小关系：

| 码率 | 每帧字节数（CBR 参考） | RTP 包大小 | UDP/IP 总计 | 结论 |
|------|------------------------|-----------|-------------|------|
| 64 kbps | 80 | 92 | 120 | 安全 |
| 128 kbps | 160 | 172 | 200 | 安全 |
| 256 kbps | 320 | 332 | 360 | 安全 |
| 384 kbps | 480 | 492 | 520 | 安全 |
| 512 kbps | 640 | 652 | 680 | **上限**（产品设定） |
| 1024 kbps | 1280 | 1292 | 1320 | **禁止**（超过 Opus 1275 字节上限） |

规则：

- 固定 10 ms 帧；v3 不支持 20 ms 或 120 ms 帧。
- **码率上限为 512 kbps**（`BITRATE_MAX_KBPS`），两端必须同值：
  接收端用它钳制对端下发的值，跨端不一致会出现"界面显示 512、实际跑 1000"。
- 自动码率 `0` 的含义是"用上限"：映射为 **512 kbps**，由 VBR 在该上限内浮动，
  不做动态探测。因此 `BITRATE_AUTO_KBPS` 必须恒等于 `BITRATE_MAX_KBPS`。
  本版客户端**不再提供自动档**（UI 只发具体档位）；`0` 仅作跨版本兼容保留。
- 手动档位固定五档 `64 / 128 / 256 / 384 / 512 kbps`（`BITRATE_STEPS_KBPS`）。
  协议只强制上限，不强制档位；档位是 UI 约定，用于把滑块量化为整数位置。
  非档位值（例如历史配置 768/960/1000）由 `snap_bitrate_kbps()` 吸附到最近档位，
  等距时取较小档，超过上限的先钳制再吸附。
- 高于 512 kbps 的配置必须被拒绝或钳制到 512。
- Opus 负载最大 1275 字节；编码结果超过 1275 字节时：
  - 必须丢弃该帧，不得截断；
  - 计数并降低码率/关闭或降低 FEC/DRED 冗余；
  - 不得让 RTP 包超过 1287 字节。
- FEC/DRED 会增加单帧体积；高码率 + 高冗余时更容易超限，必须做上述保护。

### 4.8 RTP 示例

V=2、M=1、PT=111、seq=`0x1234`、ts=`0x000004B0`、SSRC=`0xDEADBEEF`，负载为 3 字节示例：

```
80 EF 12 34 00 00 04 B0 DE AD BE EF F8 FF FE
```

- `0x80`：V=2，P/X/CC=0
- `0xEF`：M=1，PT=111
- 去掉标记位时为 `0x6F`。

---

## 五、控制协议 v3

### 5.1 固定头

控制包使用 15 字节固定头 + 负载：

```
Byte 0:       0x03                     控制协议版本
Byte 1:       0x00                     保留；发送端必须写 0，接收端忽略
Byte 2:       msg_type                 消息类型
Byte 3-4:     payload_len (u16 BE)     负载长度
Byte 5:       0x00                     保留；发送端必须写 0，接收端忽略
Byte 6:       0x00                     保留；发送端必须写 0，接收端忽略
Byte 7-14:    device_id[8]             发送端设备 ID
Byte 15+:     payload                  负载
```

说明：

- v2 的 `is_audio/codec/sample_rate` 位域、`bitrate`、`seq_num` 字段在 v3 控制面全部淘汰；对应字节保留为 0。
- 接收端不得再把 Byte 1 解释为音频标志或采样率。
- `payload_len` 必须等于实际负载长度；控制包总长度必须等于 `15 + payload_len`，否则丢弃。
- 控制负载最大 `512` 字节；超出丢弃。
- 控制包与 RTP 的区分见 3.2。

### 5.2 消息类型

| 类型 | 值 | 方向 | 说明 |
|------|----|------|------|
| `TYPE_DATA` | 0 | — | **v3 废弃**；控制包中不得出现，收到丢弃 |
| `TYPE_CONNECT` | 1 | Android → Windows | 连接请求 / 保活 / 参数同步 |
| `TYPE_DISCOVER_REQ` | 2 | Android → 广播 `44043` | 局域网发现请求 |
| `TYPE_DISCOVER_REPLY` | 3 | Windows → Android | 发现回复 |
| `TYPE_CONNECT_ACK` | 4 | Windows → Android | 连接确认 / 保活回复 / 反向 SSRC 下发 |
| 5–255 | — | — | 保留；收到后忽略，不报错 |

媒体数据不再使用控制消息类型 0；所有音频都走 RTP。

### 5.3 设备 ID

- 8 字节，持久化，所有控制包携带。
- Android：`SecureRandom` 生成并持久化到 `SharedPreferences`。
- Windows：生成后持久化到注册表 `HKCU\Software\UDP2CAL\device_id`。
- 全零视为无效 ID。
- 设备 ID 只用于控制面绑定和发现，不出现在 RTP 中。
- 设备 ID 明文传输，不具备密码学强度；不要把它当作会话密钥。

### 5.4 CONNECT（类型 1）

CONNECT 负载固定 15 字节：

| 偏移 | 长度 | 字段 | 说明 |
|------|------|------|------|
| 0 | 2 | `reverse_port` (u16 BE) | Android 反向接收端口；`0` 表示不接收反向音频 |
| 2 | 4 | `forward_ssrc` (u32 BE) | Android 正向 RTP 的 SSRC；必须非零 |
| 6 | 1 | `payload_type` | 必须为 `111` |
| 7 | 1 | `frame_ms` | 必须为 `10` |
| 8 | 1 | `complexity` | Opus 复杂度 `1..10`；`0` 表示使用默认值 |
| 9 | 1 | `signal` | `0`=auto，`1`=voice，`2`=music |
| 10 | 1 | `bandwidth` | `0`=NB，`1`=MB，`2`=WB，`3`=SWB，`4`=FB |
| 11 | 1 | `opus_flags` | bit0=VBR，bit1=DTX，bit2=VBR 约束，bit3=FEC；bit4–7 保留为 0 |
| 12 | 2 | `bitrate_kbps` (u16 BE) | 反向 Opus 目标码率；`0`=自动（512 kbps）；最大 1000 |
| 14 | 1 | `packet_loss` | 预期丢包率 `0..100`（百分比） |

规则：

- 负载长度必须恰好 15 字节；不足或超出都丢弃。
- `payload_type != 111` 或 `frame_ms != 10`：拒绝该 CONNECT。
- `forward_ssrc == 0`：拒绝该 CONNECT。
- `reverse_port == 0`：Windows 不启动反向发送；ACK 中 `reverse_ssrc=0`。
- `bitrate_kbps > 1000`：拒绝或钳制到 1000；不得按 1024 kbps 发送。
- Android 每次发送完整 15 字节 CONNECT；不再有“首包只发 2 字节”的 v2 行为。
- 连接期间每 1 秒发送一次 CONNECT，作为保活和参数同步。
- 同一设备、同一 `forward_ssrc` 的周期 CONNECT 不重置 Windows 抖动缓冲。
- `forward_ssrc` 变化表示新会话：Windows 必须重置正向抖动缓冲/解码器，并更新绑定。
- CONNECT 同时携带反向 Opus 配置；Windows 只在配置变化时重新应用，避免每秒重置编码器状态。

### 5.5 CONNECT_ACK（类型 4）

ACK 负载固定 6 字节：

| 偏移 | 长度 | 字段 | 说明 |
|------|------|------|------|
| 0 | 4 | `reverse_ssrc` (u32 BE) | Windows 反向 RTP 的 SSRC；`0` 表示反向未启用 |
| 4 | 1 | `payload_type` | 必须为 `111` |
| 5 | 1 | `frame_ms` | 必须为 `10` |

规则：

- Windows 收到每个合法 CONNECT 后立即回复 ACK。
- ACK 的 `device_id` 是 Windows 的设备 ID。
- ACK 的目标地址是 CONNECT 的源 IP/端口（Android 主 socket）。
- `reverse_ssrc=0` 时，Android 不得启动反向接收解码；已有反向播放应停止并清空缓冲。
- `reverse_ssrc` 非零且发生变化时，Android 必须重置反向抖动缓冲/解码器；旧 SSRC 的包丢弃。
- ACK 不携带音频，也不用于媒体流控。

### 5.6 DISCOVER_REQ / DISCOVER_REPLY

**DISCOVER_REQ（类型 2）**

- 负载为空。
- Android 从临时 socket 广播到 `255.255.255.255:44043`。
- 头部 `device_id` 为 Android 设备 ID。

**DISCOVER_REPLY（类型 3）**

负载格式：

| 偏移 | 长度 | 字段 | 说明 |
|------|------|------|------|
| 0 | 2 | `listen_port` (u16 BE) | Windows 控制/正向 RTP 监听端口，默认 `44044` |
| 2 | N | `device_name` | UTF-8 设备名，不带 NUL；最长 254 字节 |

规则：

- Windows 收到 DISCOVER_REQ 后，向请求源地址单播回复。
- Windows 在 Ready 状态每秒向 `255.255.255.255:44043` 广播一次 DISCOVER_REPLY；Busy 状态停止广播。
- 回复源端口为 `44043`；负载中的 `listen_port` 才是后续 CONNECT 的目标端口。
- Android 解析 `listen_port` 和设备名；设备 ID 从控制头读取。
- 设备名超过 254 字节时截断，且不得截断 UTF-8 多字节字符中间。

### 5.7 控制包校验

接收端必须依次检查：

1. 数据报长度 ≥ 15；
2. 首字节 == `0x03`；
3. `payload_len <= 512`；
4. 数据报长度 == `15 + payload_len`；
5. `msg_type` 合法（1–4；5–255 忽略；0 丢弃）；
6. 消息负载长度符合该类型要求；
7. 需要绑定的消息，`device_id` 与当前绑定一致。

任何一步失败：丢弃并计数；不得把控制包误送进 RTP 解析器。

### 5.8 控制包示例

**CONNECT**（device_id=`01 23 45 67 89 AB CD EF`，reverse_port=`50000`，forward_ssrc=`0x11223344`，PT=111，10 ms，complexity=3，signal=voice，bandwidth=FB，VBR+DTX，bitrate=auto，packet_loss=5）：

```
03 00 01 00 0F 00 00 01 23 45 67 89 AB CD EF
C3 50 11 22 33 44 6F 0A 03 01 04 03 00 00 05
```

**CONNECT_ACK**（device_id=`FE DC BA 98 76 54 32 10`，reverse_ssrc=`0xAABBCCDD`）：

```
03 00 04 00 06 00 00 FE DC BA 98 76 54 32 10
AA BB CC DD 6F 0A
```

**DISCOVER_REPLY**（listen_port=`44044`=`0xAC0C`，名称 `UDP2CAL`）：

```
03 00 03 00 09 00 00 FE DC BA 98 76 54 32 10
AC 0C 55 44 50 32 43 41 4C
```

---

## 六、连接生命周期

### 6.1 发现

```
Android                                   Windows
   |                                         |
   |  DISCOVER_REQ (broadcast :44043)        |
   |---------------------------------------->|
   |                                         |
   |  DISCOVER_REPLY (unicast, listen_port)  |
   |<----------------------------------------|
   |                                         |
   |  （Windows Ready 时也会每秒广播）         |
```

### 6.2 建连与 SSRC 交换

```
Android                                   Windows
   |                                         |
   |  CONNECT (device_id=A, reverse_port=R,  |
   |           forward_ssrc=S_A, PT=111)     |
   |---------------------------------------->|
   |                                         | 绑定 A
   |                                         | 启动反向发送，生成 S_W
   |  CONNECT_ACK (device_id=W,              |
   |               reverse_ssrc=S_W)         |
   |<----------------------------------------|
   | 记录 W、S_W                              |
   | 启动正向 RTP (SSRC=S_A)                  |
   |---------------------------------------->| 校验源地址 + S_A
   |                                         |
   | 反向 RTP (SSRC=S_W)                      |
   |<----------------------------------------| 目标端口 R
```

### 6.3 媒体启动条件

- Android 收到合法 ACK 后，才可以开始正向 RTP。
- Windows 绑定成功后，才可以接受正向 RTP；未绑定状态下收到 RTP 一律丢弃。
- Android 收到 `reverse_ssrc != 0` 的 ACK 后，才可以启动反向接收。
- Windows 只有在 CONNECT 的 `reverse_port != 0` 且反向功能开启时，才启动反向发送。
- 接收端在收到第一个 RTP 包后进入预缓冲；预缓冲满足 30 ms（3 帧）才开始播放。

### 6.4 保活与超时

| 项目 | 值 |
|------|----|
| Android CONNECT 间隔 | 1 s |
| Windows ACK 延迟 | 立即回复 |
| Android ACK 超时 | 3 s |
| Windows CONNECT 超时 | 3 s |

规则：

- 保活只看控制面，**不得**用“是否收到 RTP”判断连接存活；DTX 静音期间可能长时间没有 RTP。
- Android 3 s 未收到新 ACK：标记断连，停止正向 RTP，保留 CONNECT 重试。
- Windows 3 s 未收到绑定设备的 CONNECT：释放绑定，停止反向发送，回到 Ready。
- ACK 丢失但 CONNECT 正常时，以最近一次 ACK 时间为准；下一次 CONNECT 会重新触发 ACK。
- 重连必须使用新 SSRC；旧 SSRC 包丢弃，抖动缓冲清空。

### 6.5 反向音频开关

- Android 在 CONNECT 中通过 `reverse_port` 表达“是否接收反向音频”：
  - `0`：不接收；Windows 不启动反向发送，ACK 中 `reverse_ssrc=0`。
  - 非零：接收；Windows 启动反向发送并返回非零 `reverse_ssrc`。
- 运行中切换反向开关时，Android 更新 CONNECT；Windows 重新生成/停止反向 SSRC，并在 ACK 中体现。
- Android 收到 `reverse_ssrc=0` 后必须停止反向播放并清空反向抖动缓冲。

### 6.6 状态机

**Windows 绑定状态**

```
Ready ──(合法 CONNECT)──> Busy
Busy  ──(3s 无 CONNECT)──> Ready
Busy  ──(异设备 CONNECT)──> Busy（静默忽略，不回 ACK）
```

| 状态 | 行为 |
|------|------|
| Ready | 每秒广播 DISCOVER_REPLY；接受 CONNECT；丢弃所有 RTP |
| Busy | 停止广播；只接受绑定设备的 CONNECT；只接受绑定源地址 + SSRC 的 RTP；异设备 CONNECT 静默丢弃 |

**Android 连接状态**

```
Idle ──(CONNECT)──> Connecting ──(ACK)──> Connected
Connected ──(3s 无 ACK)──> Reconnecting
Reconnecting ──(ACK)──> Connected
```

| 状态 | 行为 |
|------|------|
| Idle | 未连接；可发现/发起连接 |
| Connecting | 已发 CONNECT，等待 ACK；不发送正向 RTP |
| Connected | 发送正向 RTP；按 ACK 启动/停止反向接收；每秒 CONNECT |
| Reconnecting | 停止媒体；持续每秒 CONNECT；收到 ACK 后新 SSRC 重建 |

---

## 七、接收端抖动缓冲与播放

### 7.1 设计约束

1. **声卡播放头是唯一音频时钟**。不得用 `Instant`、`System.currentTimeMillis()`、`sleep` 或定时器决定播放/PLC/重同步。
2. 网络线程只负责收包、校验、插入抖动缓冲；不得在音频路径上阻塞等待播放。
3. Windows 使用独立解码线程 + 现有无锁 PCM 环；cpal 回调只搬运 PCM，不碰 Opus、不分配、不加锁。
4. Android 在 AudioTrack 写线程中直接拉取/解码/PLC；AudioTrack 的阻塞写入天然提供节拍。
5. 接收缓冲是“已收到但未播放”的音频水位；等待由水位表达。

### 7.2 参数

| 参数 | 值 | 说明 |
|------|----|------|
| `FRAME_MS` | 10 ms | 固定 |
| `FRAME_SAMPLES` | 480 | 48 kHz |
| `JITTER_START_MS` | 30 ms | 预缓冲启动阈值（3 帧连续） |
| `WATER_TARGET_MS` | 260 ms | 目标**总**水位（单一口径：抖动缓冲 + PCM 环 + 声卡缓冲）。取值由实测断供幅度决定，见 §7.10.1 |
| `DECODE_LIMIT_MS` | 40 ms | 解码缓冲深度上限：到顶就暂停生产（生产节奏由声卡消费节奏约束） |
| `PLC_LEAD_MS` | 20 ms | 可听点见底的判据：`pipe_ms <= 20` 才允许补 PLC |
| `PLC_MAX_MS` | 120 ms | 连续 PLC 上限（12 帧） |
| `MAX_REORDER_PACKETS` | 64 | 编码包重排槽位 |
| `RESYNC_GAP_MS` | 200 ms | 触发重同步的时间戳跳变阈值 |
| `RESAMPLE_MAX_ADJUST` | ±0.005 | 重采样比率微调上限（±0.5%） |
| `RATIO_SLEW_STEP` | 0.0001 | 单次比率步进（1%/s 斜坡，按真实 dt 缩放） |
| `PI_KP_PER_MS` | 0.00005 | 反馈比例增益：每 1 ms 水位偏差给 50 ppm |
| `PI_TI_S` | 10 s | 积分时间常数（**必须 ≥5 s**，否则把网络抖动积成变调） |
| `PI_MIN_UPDATE_MS` | 5 ms | 单次评估的最小时间跨度；更密的调用只累计时间 |
| `WATER_SMOOTH_ALPHA` | 0.065 | 水位一阶低通系数（τ≈150 ms，带宽≈1 Hz） |
| `RATE_WINDOW_S` / `RATE_BUCKETS` | 30 s / 30 | 前馈观测窗与桶数（每桶 1 s，桶内只留最小单向时延） |
| `RATE_MIN_BUCKETS` | 6 | 起算所需最少桶数（没攒够宁可不出估计） |
| `RATE_JUMP_MAX_SAMPLES` | 96000（2 s） | 单次观测允许的媒体跳变上限：超过即判时间线被重建，整窗作废 |
| `RATE_FEEDFORWARD_MAX` | ±0.02 | 前馈钳位（±2%：超出说明不是钟差） |

> UDP2CAL v1.3.0 起删除了 `JITTER_TARGET_MS`（被 `WATER_TARGET_MS` 取代，口径并入声卡缓冲）、
> `JITTER_HIGH_MS`/`JITTER_LOW_MS`（从未被读）、全部 `SLIP_*` 与
> `RATIO_DEADBAND_MS`/`RATIO_FULL_MS`（离散丢/插已整个删除）。

### 7.3 数据结构

每个方向一个抖动缓冲实例：

```text
JitterBuffer {
    slots: [PacketSlot; 64]       // 按扩展序列号/时间戳存放
    next_seq: u64                 // 下一个应播放的扩展序列号
    next_ts: u64                  // 下一个应播放的扩展时间戳（48 kHz）
    highest_seq: u64              // 已见最大扩展序列号
    highest_ts: u64               // 已见最大扩展时间戳
    started: bool                 // 是否已建立基准
    playing: bool                 // 是否已进入 PLAYING
    plc_run: u32                  // 连续 PLC 帧数
    stats: ...                    // 含 seq_realign：因序号滞后触发的重新对齐次数
}
```

`PacketSlot` 至少保存：扩展序列号、扩展时间戳、负载长度、负载字节（最大 1275）、有效标志、到达顺序（可选）。

### 7.4 插入算法

网络线程收到 RTP 后：

```text
1. 校验 RTP 版本、P/X/CC、PT、长度、SSRC、源地址。
2. 用 16 位/32 位有符号差值扩展 seq/ts：
     ext_seq = highest_seq + (seq - (u16)highest_seq)   // 以 i16 解释差值
     ext_ts  = highest_ts  + (ts  - (u32)highest_ts)    // 以 i32 解释差值
3. 若尚未 started：
     next_seq = ext_seq
     next_ts  = ext_ts
     started  = true
4. 若包迟到/重复：
     ext_ts < next_ts                    -> 丢弃并计数（时间戳是播放顺序的唯一权威）
     slots 中已存在同一 ext_ts            -> 丢弃并计数（重复）
5. 断点判定（**必须先于"序号迟到"判定**）：
     若 ext_seq < next_seq：
         说明发送端发生过"时间戳前进、序号冻结"的断点（ACK 中断 / DTX），
         或断流跨过了 i16 差值边界（>32767 个包 ≈ 5.4 分钟）。
         以该包为新基准重新对齐（清 slots、重置 next_seq/next_ts/highest_*），
         然后照常入槽；计入 seq_realign。
6. 若 M=1 且 (ext_ts - next_ts) > RESYNC_GAP_MS：
     清空 slots；重置 next_seq/next_ts/highest_*；
     重置解码器/PLC 状态；进入 PREBUFFER；
     以该包为新基准。
7. 若 slots 已满：
     丢弃扩展序列号最小的非 next 包；计数。
8. 将包拷入 slot；更新 highest_seq/highest_ts。
9. 唤醒播放/解码线程。
```

注意：

- 扩展差值超出重排窗口的包按“跳变”处理，不得让 `u16/u32` 回绕误判为巨大的未来包。
- 时间戳是播放顺序的权威；序列号用于重复检测与断点识别。**序号落后于
  `next_seq` 而时间戳合法且未重复时，结论是"发生了断点"，不是"包迟到"**——
  真正迟到的包在第 4 步已由 `ext_ts < next_ts` 拦下。旧实现的顺序（先判序号
  迟到再判重同步）会让断流后的恢复包永久被丢弃。
- 不得在插入路径上调用 Opus 解码。

### 7.5 播放算法

播放线程（Android 的 AudioTrack 写线程，或 Windows 的解码线程）循环：

```text
ring_ms = PCM 环中已解码但尚未被声卡取走的毫秒数   // Android 无独立 PCM 环，恒为 0
pipe_ms = ring_ms + 声卡已接手但尚未播完的毫秒数    // = 可听点之前压着的全部媒体量

if (!started) return 等待;

if (!playing):
    if 连续可解码帧数 >= JITTER_START_MS / 10:
        playing = true
    else:
        return 等待;

if slot[next_ts] 存在:
    if ring_ms >= DECODE_LIMIT_MS:      # 解码缓冲已到上限，先把数据放掉
        return 等待;                    # 开门的是声卡的消费节奏，不是定时器
    decode(slot.payload) -> PCM
    remove slot
    next_seq += 1
    next_ts  += 480
    plc_run = 0
    return PCM

if pipe_ms <= PLC_LEAD_MS:              # 可听点之前已经见底 → 缺口确定
    if plc_run < PLC_MAX_MS / 10:
        pcm = opus_plc_10ms()
        plc_run += 1
    else:
        pcm = silence_10ms()
    next_seq += 1
    next_ts  += 480
    return pcm

return 等待            // 还有余量，继续等
```

关键点：

- `next_ts` 是“下一个要输出的 10 ms 帧”的 RTP 时间戳；它只有一个用途：索引槽位。
- **不存在播放头时间戳**。旧实现的 `playhead_ts`（设备帧→48 kHz 媒体帧的换算 + 偏移记账）
  已被整条删除：`playback − callback` 换算出的设备帧从不参与播放判定，只用于上报声卡延迟。
- 两个判据都是水位：`ring_ms` 决定“现在该不该生产下一帧”（生产节奏跟声卡走），
  `pipe_ms` 决定“这一帧还来不来得及”（要不要补 PLC）。二者相差的正是声卡缓冲那一段。
- `ring_ms >= DECODE_LIMIT_MS` 与 `pipe_ms <= PLC_LEAD_MS` 是同一个量的两个阈值：
  前者是上限（别把延迟堆起来），后者是下限（见底就补）。中间是自由区。
- 有真实包到达但 `next_ts` 已经越过它：按迟到丢弃；不得回退 `next_ts`。
- `PLC_LEAD_MS` 就是"可听点只剩 20 ms 余量"这件事本身，不需要再按下标换算。

### 7.6 状态机

```
IDLE
  │ 收到第一个合法 RTP
  ▼
PREBUFFER ──(连续 >= 3 帧)──> PLAYING
  ▲                              │
  │                              ├─ 下一帧缺失，但 pipe_ms > PLC_LEAD_MS
  │                              │     └─> GAP（等待，不消耗媒体时间）
  │                              │
  │                              ├─ next_ts 缺失且 pipe_ms <= PLC_LEAD_MS
  │                              │     └─> PLC（最多 12 帧）→ 之后静音
  │                              │
  │                              └─ M=1 且跳变 > 200ms
  │                                    └─> RESYNC
  │
  └────────────────────────────── RESYNC（清空、重置、重新预缓冲）
```

- `GAP` 不是定时器状态：只要可听点之前还压着超过 `PLC_LEAD_MS` 的媒体量，就继续等；
  等待本身不消耗媒体时间，也不产生定时器。
- `PLC` 由**水位见底**触发（`pipe_ms <= PLC_LEAD_MS`），不是由时钟触发；
  `plc_run` 达到 12 后输出静音，但仍保持 `PLAYING`，直到真实包恢复。
- 真实包恢复且时间戳跳变大于 200 ms：进入 `RESYNC`；否则直接继续 `PLAYING`，`plc_run=0`。

### 7.7 启动与重同步

**启动**

- 首包建立基准；不要求首包序列号是某个固定值。
- 连续 3 帧（30 ms）可解码后才开始播放。降到 30 ms 的前提是水位由受控重采样闭环维持（7.9），不再是"离散校正需要留安全垫"的旧模型。
- 若首包丢失，以第一个收到的包为基准；不得无限等待“第 0 帧”。
- Windows cpal 回调在预缓冲完成前输出静音；播放进度只由回调的 `played_frames` 表达，
  不存在需要"启动计时"的播放头，也没有设备帧→媒体帧的换算。
- Android 的 AudioTrack 只需 2 帧（20 ms）缓冲并启用 low-latency 通路；这 2 帧是输出缓冲，不是应用层无限队列。

**重同步**

触发条件：

- `M=1` 且 `ext_ts - next_ts > 200 ms`；
- 或连续 PLC 达到上限后收到时间戳跳变大于 200 ms 的包；
- 或发送端 SSRC 变化（由控制面触发）。

动作：

1. 清空 slots；
2. 重置 `next_seq/next_ts/highest_*`；
3. 重置 Opus 解码器和 PLC 状态；
4. 清空 PCM 环（`flush`）、重采样器相位与历史、解码器 PLC 状态；
5. 复位漂移控制器与速率估计器（`highest_ts` 重建基准，旧观测不再有意义）；
6. 回到 `PREBUFFER`，重新满足 3 帧后播放。

### 7.8 PLC 规则

- 使用 Opus 解码器的 PLC 接口；每次请求 **10 ms / 480 采样**。
- 不得一次请求 120 ms（5760 采样）后当成一帧播放。
- 连续 PLC 上限 12 帧（120 ms）。
- 真实包到达或重同步时，`plc_run=0`。
- Opus 解码失败按丢包处理：丢弃该包，水位见底后 PLC。
- PLC 不得由网络线程、定时器或“超过 N 毫秒没收到包”的计时器触发。
- PLC 输出必须进入与真实 PCM 相同的输出路径，参与水位统计。

### 7.9 漂移校正：前馈管速率，反馈管相位

发送端采集时钟与接收端声卡时钟不同源，水位会缓慢漂移。校正分两条**互不污染**的支路。

**反馈支路（PI，管相位）**

```text
water   = 抖动缓冲 + PCM 环 + 声卡已接手未播（ms）      # 单一口径，见 7.10
err     = 低通(water) − WATER_TARGET_MS
p       = clamp(err × PI_KP_PER_MS, ±RESAMPLE_MAX_ADJUST)
i_next  = clamp(i + (PI_KP_PER_MS / PI_TI_S) × err × dt_s, ±RESAMPLE_MAX_ADJUST)
          抗饱和：p+i 已到钳位且 err 与 i 同号时不累加（否则饱和后积分一直堆）
Δu      = 限速后的 clamp(p + i, ±RESAMPLE_MAX_ADJUST)
          单次限速 RATIO_SLEW_STEP × (dt/10ms)      # 约 1%/s
```

- 比例增益 `PI_KP_PER_MS = 0.00005`：20 ms 偏差只给 1000 ppm，抖动不会被放大成变调。
- 积分时间 `PI_TI_S = 10 s`（**必须 ≥5 s**）：再快就把网络抖动积成变调。
- 计时用**声卡已消费帧数**而不是媒体样本数：断流时媒体时间不前进，用它会冻结积分。
- 低通按真实 `dt` 缩放：`alpha = 1 − (1 − WATER_SMOOTH_ALPHA)^(dt/10ms)`，带宽≈1 Hz。
- PI 无静差：稳态水位回到目标，不存在旧的"±300 ppm 残余比率"。
- `dt < PI_MIN_UPDATE_MS`（5 ms）的调用只累计时间，不评估——正常 10 ms 帧率下每次都会评估。

**前馈支路（速率估计器，管频率）**

- 观测对：每个新包的 `(highest_ts, 到达时刻)`。用 `highest_ts` 而不是"已收样本数"：
  丢包会让后者少计 480 样本，时间线则无偏。
- 每 1 s 一个桶，桶内只保留**最小单向时延** `min(t_local − media)`。
  排队只会增大单向时延，取最小几乎不受排队影响（NTP min-filter / WebRTC 同源）。
- 30 个桶做最小二乘拟合：`r̂ = −slope`（媒体快于本地钟 → slope 负 → r̂ 正），
  钳 `±RATE_FEEDFORWARD_MAX`（±2%）；同时给出斜率标准差折算的 `σ`（ppm）。
- 桶数不足 `RATE_MIN_BUCKETS`（6）时**不发布**估计（宁可用 0，也不用噪声）。
- 观测窗 30 s：精度下限 `σ_δ ≈ σ_J·√12·√τ_j / T^1.5`（τ_j≈50 ms），
  σ_J=2 ms 时 30 s 约 ±9 ppm、10 s 仅 ±49 ppm、3 s 完全不可用。
- `resync`/epoch 变化会重建 `highest_ts` 基准，速率估计器必须在**同一处** `reset()`，
  否则旧观测会把"重同步跳变"当成钟差。

**合成**

```text
ratio = 源采样率/设备采样率 × (1 + ff) × (1 + Δu)      # ff = r̂ − 1
```

- 频率补偿（晶振差、温度漂移）全部由 `ff` 承担，反馈环只需修相位，工作点永远在 0 附近；
- 两条支路都**不需要记住偏移**：没有 `timestamp_offset`、没有 `note_ratio_correction`、
  没有 `note_pcm_correction`。任何需要"记住偏移"的机制都说明中间表示选错了。

**为什么不再有第二级离散回落**

逐样本丢/插 1 ms 的"有界回落"（`SLIP_*` 常量、升余弦交叉淡化、`slip_drop`/`slip_insert`）
已整个删除：比率是连续量，改动它不会在 PCM 里留下任何接缝；回落方案则每个接缝都在
PCM 上留痕，而且它的存在只为兜住"比率顶满"的极端情况。±0.5% 的吞吐等于每秒 5 ms 水位差，
覆盖任何真实晶振（典型 10–100 ppm）；真正超出钳位的是**阶跃**（改设备采样率、WASAPI 引擎
周期切换、发送端节拍重置），那不是 ASRC 能修的，交给 `RESYNC`/PLC。

### 7.10 水位口径与可听点换算

**只有一个水位**：

```text
water_ms    = jb.water_ms() + ring_ms + device_latency_ms
jb.water_ms = ((jb.highest_ts() − jb.next_ts()) / 480 + 1) × 10   # 上限 64 帧
ring_ms     = 环内已解码帧数 × 1000 / device_rate
pipe_ms     = ring_ms + device_latency_ms          # 可听点之前压着的全部媒体量
```

三项分别是"已收未解码""已解码未被声卡取走""已交给声卡未播完"。所有判据都用这三个量：

| 判据 | 用途 |
|------|------|
| `water_ms` vs `WATER_TARGET_MS` | PI 的 `err`（唯一控制目标） |
| `pipe_ms <= PLC_LEAD_MS` | 可听点见底 → 允许补 PLC |
| `ring_ms >= DECODE_LIMIT_MS` | 解码缓冲到上限 → 暂停生产 |

`jb.water_ms()` 用 `highest_ts − next_ts` 而不是"从 `next_ts` 起连续可解的帧数"：
后者在**队首那一帧还没到**（乱序/迟到）时会瞬间塌到 0，而后面明明还压着十几帧。
把它当控制信号，PI 会在每次乱序时误以为缓冲空了、反向把比率拉满，实机上是水位
长期锯齿的主因（P0 打点里 3.4 万个样本中仅 39 个塌陷，但每次都会引发一次错误的
反向调节）。
`device_latency_ms` 来自 `OutputCallbackInfo::timestamp()` 的 `playback − callback`
（Windows）；Android 缺省 0（播放头本身就是混音器消费位置）。

**可听点对应的媒体时间戳**（只用于总延迟估算与日志）：

```text
queued_media = (ring_frames + device_latency_frames) × current_ratio()
audible_ts   = jb.next_ts() − queued_media
```

即"下一个要播的媒体样本"往前推掉缓冲里压着的那些样本。乘 `current_ratio()`
是因为环里装的是设备帧，而媒体时间线是 48 kHz。

**为什么不再需要双向换算**

旧实现要从设备帧反算播放头（`base_ts + floor(played×48000/device_rate) + offset`，
外加余数累加器），于是每次比率调整都要 `note_ratio_correction` 把换算偏移补回去，
每次离散回落还要 `note_pcm_correction` 再补一次——三条补丁互相依赖，任何一处漏掉
都会让"播放头"与"水位"背道而驰。改为水位口径后，`played_frames` 不再出现在任何
播放判定里，换算链与偏移记账整条消失。

绝对精度仍需**实测校准一次**（见 [LATENCY_CALIBRATION.md](LATENCY_CALIBRATION.md)），
但校准值只作为日志/展示的修正量，不参与水位控制——水位控制只关心相对填充变化。

### 7.10.1 为什么 `WATER_TARGET_MS` 是 260 ms

这个数不是拍脑袋定的，而是 P0 实机打点（`UDP2CAL_ASRC_DIAG=20`，每轮 300 s，
Android 发送端 + Wi-Fi + VB-Cable）量出来的。关键发现是网络**周期性断供**：

| 观测量 | 实测值 |
|--------|--------|
| 长期到达率 | 99.99 帧/s（发送端节拍无锅） |
| 单次到位 ≥6 帧的突发 | 中位 **9 帧 = 90 ms**，约每 1.2 s 一次 |
| 更大幅度的断供→补齐 | 每隔几十秒一次 ~200 ms（断供期只有 PLC 顶着，随后十几帧一起到） |
| 时钟差（前馈） | 中位 4 ppm，`σ` 中位 13 ppm（两端晶振几乎一样） |
| 发送端整形 | 已是 10 ms 网格（`CaptureService.sendFrames`），不是发送端攒批 |

所以目标水位必须盖住**断供幅度**（~200 ms），而不是批次大小（90 ms）：
断供期间水位往下掉，水位不够就直接变成 PLC；而补回来的富余只能靠比率排水
（±0.5% = 5 ms/s，200 ms 要 40 s）。三轮实测：

| 目标水位 | 实测 `water_ms` | PLC | `adjust` 中位 | 贴 ±0.5% 钳位 |
|---------|----------------|-----|--------------|--------------|
| 70 ms | 120–306（中位 179） | 3–8 次/分 | **+3684 ppm** | 7.1% |
| 210 ms | 中位 233，尖峰 500 | 1.7 次/分 | +1096 ppm | 15.1% |
| **260 ms** | **中位 275–277，max 328–490**（随网络波动） | **0.4–1.1 次/分** | **−46…+517 ppm** | **0%** |

目标设低了 PI 会永久顶满 +0.5%（一个 3–9 音分的常年在调），水位不收敛而是"慢锯"；
260 ms 时反馈量回到 0 附近、不再长期贴钳位，PLC 降到约 1 分钟一次（好时段可到 25 分钟一次）。
网络本身会波动：同一配置重跑一次，水位中位仍在 275–284 ms，但最大尖峰会在 328–490 ms
之间变（断供幅度变了）。

**代价与取舍**：稳态总延迟约 275 ms。想更短只有两条路——把网络的断供幅度降下来
（关手机 Wi-Fi 省电 / 用 5 GHz / 有线或 USB 网络共享），或抬高 `RESAMPLE_MAX_ADJUST`
用变调换排水速度（规范 §八 明确不做后者）。这个参数是**唯一**跟环境相关的常数，
换环境后应重跑 P0 打点重新定值。
### 7.11 Windows 接收管线

```
UDP 主 socket
   │ 控制包 -> 控制状态机
   │ 正向 RTP -> 校验 -> JitterBuffer（网络线程只插入）
   ▼
JitterBuffer
   │ 由解码线程按播放头拉取
   ▼
Opus 解码线程（10 ms；PLC；漂移校正）
   │ 写入无锁 PCM 环
   ▼
cpal 输出回调（只 pop PCM、填静音、更新 played_frames）
   │
   ▼
声卡
```

约束：

- cpal 回调内**不得**加锁、分配、调用 Opus、写日志、做重采样比率的复杂计算。
- 解码线程等待用条件变量/事件；不得用 `recv_timeout`、`sleep(10ms)` 或“60 ms PLC 定时器”驱动播放。
- PCM 环容量至少 200 ms；建议 16384 帧（约 341 ms）或按设备缓冲调整。
- 解码线程只在环有空间且水位未超过目标时解码；不得无限预解码。
- `played_frames` 与 `device_latency_us` 用原子变量发布；解码线程据此算 `ring_ms`/`pipe_ms`，
  不反算任何播放头时间戳。
- 网络线程、解码线程、音频回调之间的唯一共享是抖动缓冲（锁/无锁队列）和 PCM 环；不得共享可变 Opus 实例。
- 删除 v2 的 `recv_timeout(30ms)`、60 ms PLC 计时、`Instant` 重排衰减、3 秒漂移 EMA。

### 7.12 Android 接收管线

```
反向 UDP socket
   │ 反向 RTP -> 校验 -> JitterBuffer
   ▼
AudioTrack 写线程
   │ 按水位拉取 -> Opus 解码 / PLC -> 重采样 -> AudioTrack.write（阻塞）
   ▼
AudioTrack / 声卡
```

约束：

- 写线程是唯一的反向解码者；网络线程只插入编码包。
- `AudioTrack.write` 使用阻塞模式；写入节奏由 AudioTrack 缓冲决定，不额外 `delay(20)`。
- 不使用 `Channel<ShortArray>(3)`、`ShortArrayPool` 或应用层 PCM 队列。
- 不使用 `soTimeout=40ms` 驱动接收；反向 socket 用阻塞 `receive()`。
- 不使用 40/80 ms 反向 PLC 定时器；PLC 由播放头到期触发。
- 播放头用 `AudioTrack.getPlaybackHeadPosition()`/`getTimestamp()` 获取；处理 32 位回绕。
- 若设备不支持 48 kHz，使用受控重采样；Android 没有独立 PCM 环，`ring_ms` 恒为 0，
  生产节拍由 `AudioTrack.write` 的阻塞本身提供（见 7.10）。
- 预缓冲：写线程先在 AudioTrack 里累积 2 帧（20 ms）再 `play()`；之后按播放头拉取。

### 7.13 统计

建议暴露以下计数器（原子变量，音频路径只递增，不格式化日志）：

- 收到 RTP 包数、字节数；
- 丢包/迟到/重复/乱序数；
- 抖动缓冲当前水位（ms）；
- PLC 帧数、连续 PLC 最大值；
- 漂移校正：当前比率微调量（%）、反馈量（ppm）、前馈量 `r̂`（ppm）、其标准差 σ（ppm）、
  观测桶数；
- **总延迟估算**（见 7.14）；
- 解码错误数、超长包丢弃数；
- 输出欠载次数；
- 当前 SSRC、当前播放头时间戳。

### 7.14 总延迟估算

接收端唯一能在本地测到的延迟口径：

```
total_latency_ms = now − arrival_time(audible_ts 那一帧) + device_latency_ms
```

- `arrival_time` 由抖动缓冲在**接纳**每个包时记录（128 条环形记录，≈1.28 s 历史）；
  查询时取"最新一条 `entry_ts ≤ audible_ts` 的记录"（`audible_ts` 见 7.10）；
- `device_latency_ms` 是声卡自报的"填缓冲→发声"延迟（见 7.10），Android 端缺省为 0；
- 它**不包含**发送端采集→发包以及单程网络时延：这两个量在没有跨设备时钟的情况下
  测不出来，因此这个数字应理解为"这段音频到了本机之后又等了多久"。

要求：

- 实时显示（UI 每次状态刷新读一次原子变量），并每 10 s 落一行日志，
  同时打印水位与比率微调量——出现"延迟越来越大"时可直接判断是水位涨还是声卡缓冲涨；
- 不得在音频回调里计算（回调只做一次 `Instant` 差值，写在原子变量里）。

---

## 八、发送端规则

### 8.1 通用规则

每个发送方向都必须满足：

1. 采集 10 ms PCM（48 kHz，单/立体声按方向）。
2. Opus 编码为 10 ms 帧。
3. 在负载前写入 12 字节 RTP 头。
4. 直接调用 UDP `send`；不得进入应用层音频队列。
5. 发送失败/编码失败：丢帧、计数、继续下一帧；不得重试、不得补发、不得阻塞采集。
6. 不得在音频线程等待 ACK 或控制回复。
7. 每个 10 ms 帧推进时间戳 480、**序列号推进 1**（两者同频，不论该帧是否真的发出，见 4.4）。
8. 超过 1275 字节的 Opus 负载不得发送。
9. `M=1` 按 4.6 设置。
10. **1 帧节拍整形**：发送时刻钉在 10 ms 网格上，把采集设备成簇交出的帧摊平。
    它不是队列——最多只压 1 帧；落后超过 4 帧就重置网格，绝不用连续突发追赶。

发送端节拍整形为什么必要：采集硬件的读接口按"周期"交货（WASAPI 环回常见
10/20 ms，`AudioRecord` 视缓冲而定），成簇输出的直接后果是接收端水位呈锯齿。
接收端当然能靠水位控制吸收锯齿，但那部分锯齿会实实在在变成抖动余量需求，
把目标水位抬高。在源头摊平，比在接收端加缓冲便宜得多。

实现要点（两端一致）：

```
next_send_at += 10 ms
wait = next_send_at − now
wait ≥ 1 ms   → sleep/delay(wait)
0 < wait < 1 ms → yield
wait < −40 ms → next_send_at = now      // 落后过多：重置，不追赶
```

禁止用"补发"实现追赶：连发两包会让接收端瞬时水位抬高 10 ms，
这正是要消灭的东西。心跳/静音补齐同理，不得超过实时速率。

### 8.2 Android 正向发送

- `AudioRecord`：48 kHz、单声道、16-bit、`READ_BLOCKING`。
- 每次读 480 采样（10 ms）；不得累积成 20 ms 帧。
- 编码器 JNI 已按 10 ms 创建；Kotlin 侧不再写 v2 头，RTP 头从偏移 0 开始，Opus 负载从偏移 12 开始。
- 编码输出直接写进预分配发送缓冲；不得每帧 `ByteArray` 分配。
- 发送线程使用音频优先级；`UdpSender.send` 直接调用，不切协程。
- 控制/ACK 由独立线程处理；音频线程只读 `connected` 和配置快照。
- 删除 `Channel<ShortArray>(3)`、生产者/消费者双协程、`ShortArrayPool`、`soTimeout=40ms`、`delay(20)`。
- 正向 Opus 参数（复杂度/信号/带宽/DTX/VBR/FEC/码率/丢包率）由本地配置决定；不需要通过控制面告诉 Windows。

### 8.3 Windows 反向发送

- WASAPI loopback，事件驱动；端点混音率不是 48 kHz 时先重采样到 48 kHz。
- 累积 480 个立体声帧（10 ms）后编码；不得用 20 ms 或更大帧。
- RTP 头发送目标为 CONNECT 的 `reverse_port`；源 socket 为临时端口。
- SSRC 使用控制面生成并写入 ACK 的 `reverse_ssrc`。
- 反向 Opus 配置来自 CONNECT；只在配置变化时应用，避免每秒重置编码器。
- 删除 `pcm_buf` 3 秒上限、2 ms 轮询、`sleep(5ms)` 兜底和每 1 秒无条件重配。
- 若 `reverse_port=0` 或反向关闭：不创建反向发送线程。
- 发送失败：丢帧、计数，不回压采集。

### 8.4 编码器配置同步

- CONNECT 的 Opus 参数只描述**反向**（Windows → Android）编码器。
- Windows 收到相同配置时不得重复调用 `OPUS_SET_*`；比较结构体，只有变化才应用。
- 正向编码器配置由 Android 本地决定；Windows 不需要知道。
- 帧长固定 10 ms；任何一方收到 `frame_ms != 10` 都拒绝连接，不做动态切换。
- 码率 `0`（自动）= 上限 512 kbps；最大 512 kbps；手动五档 64/128/256/384/512。

### 8.5 发送端会话状态

新会话开始时：

- 生成新的随机 `seq` 初始值；
- 生成新的随机 `ts` 初始值；
- 生成新的随机非零 SSRC；
- 第一帧 `M=1`。

发送过程中：

- 编码/发送失败不改变 SSRC，不重置时间戳；
- DTX 不发包但时间戳继续按 10 ms 推进；
- 恢复发送时 `M=1`；
- 重连/换 SSRC 时重置 `seq/ts` 和抖动缓冲。

---

## 九、常量汇总

| 常量 | 值 | 说明 |
|------|----|------|
| `CONTROL_VERSION` | `0x03` | 控制协议版本 |
| `CONTROL_HEADER_SIZE` | `15` | 控制头 |
| `CONTROL_MAX_PAYLOAD` | `512` | 控制负载上限 |
| `RTP_VERSION` | `2` | RTP 版本 |
| `RTP_HEADER_SIZE` | `12` | RTP 固定头 |
| `OPUS_PAYLOAD_TYPE` | `111` | Opus PT |
| `OPUS_SAMPLE_RATE` | `48000` | 48 kHz |
| `OPUS_FRAME_MS` | `10` | 固定 10 ms |
| `OPUS_FRAME_SAMPLES` | `480` | 每帧采样/声道 |
| `OPUS_MAX_PAYLOAD` | `1275` | Opus 负载上限 |
| `RTP_MAX_PACKET` | `1287` | RTP 包上限 |
| `UDP_IP_MAX_PACKET` | `1315` | UDP/IP 上限 |
| `DISCOVER_PORT` | `44043` | 发现广播端口 |
| `DEFAULT_LISTEN_PORT` | `44044` | Windows 默认监听端口 |
| `JITTER_START_MS` | `30` | 预缓冲（3 帧） |
| `WATER_TARGET_MS` | `260` | 目标总水位（抖动缓冲 + PCM 环 + 声卡） |
| `DECODE_LIMIT_MS` | `40` | 解码缓冲深度上限（生产节流） |
| `PLC_LEAD_MS` | `20` | 可听点见底判据 |
| `PLC_MAX_MS` | `120` | PLC 上限 |
| `MAX_REORDER_PACKETS` | `64` | 重排槽位 |
| `RESYNC_GAP_MS` | `200` | 重同步阈值 |
| `RESAMPLE_MAX_ADJUST` | `0.005` | 比率微调上限（±0.5%） |
| `RATIO_SLEW_STEP` | `0.0001` | 比率单次步进（1%/s） |
| `PI_KP_PER_MS` | `0.00005` | 反馈比例增益 |
| `PI_TI_S` | `10.0` | 积分时间常数（≥5 s） |
| `PI_MIN_UPDATE_MS` | `5.0` | 评估最小时间跨度 |
| `WATER_SMOOTH_ALPHA` | `0.065` | 水位低通（τ≈150 ms） |
| `RATE_WINDOW_S` / `RATE_BUCKETS` | `30.0` / `30` | 前馈观测窗 / 桶数 |
| `RATE_MIN_BUCKETS` | `6` | 前馈最少桶数 |
| `RATE_JUMP_MAX_SAMPLES` | `96000` | 前馈单次观测的媒体跳变上限（2 s），超过即重建窗口 |
| `RATE_FEEDFORWARD_MAX` | `0.02` | 前馈钳位（±2%） |
| `BITRATE_AUTO_KBPS` | `512` | `0` 的兼容映射（= 上限） |
| `BITRATE_MAX_KBPS` | `512` | 码率上限 |
| `BITRATE_STEPS_KBPS` | `[64,128,256,384,512]` | 手动档位（UI 滑块） |
| `KEEPALIVE_INTERVAL_MS` | `1000` | CONNECT 间隔 |
| `ACK_TIMEOUT_MS` | `3000` | Android 断连 |
| `PEER_TIMEOUT_MS` | `3000` | Windows 释放绑定 |

---

## 十、错误处理

| 情况 | 处理 |
|------|------|
| RTP 版本不是 2 | 丢弃，计数 |
| RTP P/X/CC 非零 | 丢弃，计数 |
| PT 不是 111 | 丢弃，计数 |
| SSRC 为 0 或不匹配 | 丢弃，计数 |
| RTP 源地址不匹配 | 丢弃，计数 |
| RTP 长度 <13 或 >1287 | 丢弃，计数 |
| Opus 负载 >1275 | 丢弃，计数；降低码率/冗余 |
| Opus 解码失败 | 丢弃该包；播放头到期后 PLC |
| 控制版本不是 3 | 丢弃，计数 |
| 控制包长度/`payload_len` 不一致 | 丢弃，计数 |
| 控制类型 0 | 丢弃；RTP 不走控制类型 |
| 未知控制类型 5–255 | 忽略，不报错 |
| CONNECT 负载不是 15 字节 | 丢弃，不绑定 |
| CONNECT PT/帧长不合法 | 丢弃，不绑定 |
| 无 ACK 超过 3 s | Android 断连，停止媒体，继续 CONNECT |
| 无 CONNECT 超过 3 s | Windows 释放绑定，停止反向，回到 Ready |
| 抖动缓冲满 | 丢弃扩展序号最小的非 `next` 包，计数 |
| 迟到/重复包 | 丢弃，计数 |
| 时间戳合法但序号落后（断流/DTX 恢复） | 按时间戳重新对齐并计数，**不得丢弃** |
| PLC 超过 120 ms | 输出静音，等待真实包/重同步 |
| 声卡/解码器致命错误 | 停止播放，报告错误；不得忙等 |

所有错误处理都不得引入无界队列、无界重试或音频路径定时器。

---

## 十一、测试与验收

### 11.1 单元测试

**RTP/控制编解码**

- RTP 头字段逐位断言；
- `M=0/1`、PT、SSRC、seq/ts 网络字节序；
- seq `0xFFFF -> 0x0000`；
- ts `0xFFFFFF00 + 480 -> 0x000000E0`；
- 扩展时间戳 `0xFFFFFFFF + 480 -> 0x1000001DF`；
- 控制头 15 字节、`payload_len` 边界 0/512/513；
- CONNECT 15 字节、ACK 6 字节；
- 控制包与 RTP 解复用；
- 超长/截断/全零 SSRC/错误 PT 全部拒绝。

**抖动缓冲**

- 顺序插入/拉取；
- 乱序 1–10 帧；
- 重复包；
- 迟到包；
- 丢包 + PLC 10 ms × N；
- 连续 PLC 上限；
- `M=1` + 200 ms 跳变重同步；
- 16/32 位回绕；
- 序号冻结后恢复（ACK 中断 / DTX）必须自愈；
- 序号跨度超过 32767 的断流必须自愈；
- 反馈 PI：目标水位上不动；偏大给正比率、偏小给负比率且 |Δu| ≤ `RESAMPLE_MAX_ADJUST`；
  单次变化 ≤ `RATIO_SLEW_STEP`×(dt/10 ms)；闭环仿真收敛到目标且**无静差**；
  抗饱和生效；±4 ms 抖动带内比率几乎不动；`dt` 不足 `PI_MIN_UPDATE_MS` 时不出量；
  44.1 kHz 设备率下同样收敛；
- 前馈估计器：未攒够 `RATE_MIN_BUCKETS` 不发布；200 ppm / −300 ppm 钟差在 30 s 窗内
  误差 ≤15 ppm；0–20 ms 单向排队不影响结果；钳位 ±`RATE_FEEDFORWARD_MAX`；
  `reset()` 后重新起算；同一 `highest_ts` 重复观测只记一次；
  媒体跳变超过 `RATE_JUMP_MAX_SAMPLES` 时旧窗口作废、从新基准重算；
- 调度节流：`ring_ms >= DECODE_LIMIT_MS` 时不生产；`pipe_ms <= PLC_LEAD_MS` 才补 PLC；
  阈值序 `PLC_LEAD_MS < DECODE_LIMIT_MS < WATER_TARGET_MS`；
- 重采样器：48k→48k 与 48k→44.1k 的输出帧数正确、跨块相位连续、DC 精确通过、
  块边界无台阶、12 kHz 幅度波动 <0.1 dB、比率钳位；
- 水位边界：`pipe_ms` 在 `PLC_LEAD_MS` 上下切 PLC/正常；`ring_ms` 在 `DECODE_LIMIT_MS` 上下切生产/暂停。

**Opus**

- 10 ms 解码返回 480 采样；
- PLC 每次返回 480 采样；
- 超过 1275 字节的包被拒绝；
- DTX 1–2 字节包可解码。

### 11.2 跨语言一致性

Kotlin 与 Rust 必须对同一组输入产生相同字节：

- RTP 头示例（4.8）；
- CONNECT/ACK 示例（5.8）；
- DISCOVER_REPLY 示例；
- seq/ts 回绕；
- 控制头字节。

建议把十六进制向量放在两端测试中，任何一端改动都必须同时更新。

### 11.3 集成测试

在局域网/本机用网络损伤工具模拟：

- 0%、1%、3%、5%、10% 随机丢包；
- 0–50 ms 乱序；
- 1–100 ms 抖动；
- 重复包；
- 突发丢失 100–500 ms；
- 200 ms 时间戳跳变/发送端重启；
- DTX 静音 1–10 s 后恢复；
- 64 / 128 / 256 / 384 / 512 kbps（五档，滑块）；
- FEC/DRED 开启与关闭；
- Windows 44.1 kHz 输出设备；
- Android 44.1/48 kHz 输出；
- 反向开关切换；
- 断网 1 s / 5 s / 30 s 后重连；
- 连续运行 1 小时以上。

### 11.4 验收标准

1. 音频路径无 `sleep`、无 PLC 定时器、无重排衰减定时器、无 3 秒漂移 EMA。
2. 发送端无应用层音频队列；发送线程单帧处理时间稳定。
3. 接收端稳态总水位维持在 `WATER_TARGET_MS` 附近（实测中位 277 ms / 目标 260 ms，网络断供
   幅度决定不可能更紧）；无持续增长；30 min 内无单向漂移；稳态比率残差 <20 ppm（实测反馈量
   中位 −46 ppm）。强抖动网络下“水位标准差 <3 ms”不可达——那是网络交付节奏的函数，不是
   控制器的函数（实测 sd 31 ms）。
4. 丢包时只在可听点见底（`pipe_ms <= PLC_LEAD_MS`）后 PLC；真实包恢复后 PLC 停止。
5. DTX 静音不导致断连；恢复时能正确重同步。
6. 任意 RTP 包不超过 1287 字节；任意 UDP/IP 包不超过 1315 字节。
7. 序列号/时间戳回绕不产生爆音或长时间静音。
8. 重连使用新 SSRC；旧包不进入新会话。
9. Windows cpal 回调无锁、无分配、无 Opus 调用。
10. Android AudioTrack 写线程无应用层 PCM 队列、无 `soTimeout` 驱动。
11. 1 小时连续运行无内存增长、无无界队列、无明显漂移。
12. 两端控制包字节级一致。

### 11.5 已知限制

**已通过实现解决**（不再列为风险，留作记录）：WASAPI 环回事件驱动、44.1 kHz 设备的播放头换算、
FEC/DRED 的超限保护（码率上限 512 kbps，最坏情况 640 + 冗余仍远低于 1275 字节）。
实测与校准流程见 [LATENCY_CALIBRATION.md](LATENCY_CALIBRATION.md)。

仍然存在、且**不打算在本版解决**的限制：

1. **Android 蓝牙输出**：蓝牙链路延迟远大于 60 ms，且不同编解码器差异巨大。
   本版不为此提高目标水位（那会让有线用户白白多等），需要时由上层应用自行补偿。
2. **AudioTrack 播放头精度**：不同厂商 `getPlaybackHeadPosition()` 的粒度不同（典型 5–20 ms，
   低延迟模式下更细）。协议侧只要求播放头单调，不假设其精度。
3. **Wi-Fi 抖动**：目标水位 60 ms 覆盖常见家用/办公环境；极端拥塞下会增加 PLC 比例，
   而不是抬水位——抬水位会把延迟永久留在链路上。
4. **干净断代**：v2/v3 不能互通，发布时必须同时升级两端（见第二章）。

---

## 十二、安全边界

v3 的安全模型与 v2 相同，仅适用于可信局域网：

- 控制包和 RTP 均为明文；
- `device_id` 和 SSRC 不是密码学凭据；
- 源地址/SSRC 校验只能降低误连和普通串流风险；
- 不防御同网段主动注入、窃听、重放、DoS；
- 不实现 SRTP、DTLS、认证或密钥交换。

部署建议：

- 仅在可信 Wi-Fi/有线局域网使用；
- Windows 防火墙只放行必要的 UDP 端口；
- 不要在公共 Wi-Fi、咖啡馆、宿舍共享网络等不可信环境使用；
- 后续若需公网/不可信网络，必须引入 SRTP/DTLS 或受控中继，不属于 v3。

---

## 附录 A：RTP 头字段速查

| 字段 | 位宽 | 值 |
|------|------|----|
| V | 2 | 2 |
| P | 1 | 0 |
| X | 1 | 0 |
| CC | 4 | 0 |
| M | 1 | 0/1 |
| PT | 7 | 111 |
| seq | 16 | 随机初值，逐包 +1 |
| timestamp | 32 | 随机初值，每 10 ms +480 |
| SSRC | 32 | 每方向随机非零 |
| payload | 1..1275 | Opus |

## 附录 B：测试向量

### B.1 RTP

```
输入：
  V=2, P=0, X=0, CC=0
  M=1, PT=111
  seq=0x1234
  ts=0x000004B0
  ssrc=0xDEADBEEF
  payload=F8 FF FE

输出：
  80 EF 12 34 00 00 04 B0 DE AD BE EF F8 FF FE
```

### B.2 序列号回绕

```
0xFFFF + 1 = 0x0000
扩展序列号：65535 -> 65536
```

### B.3 时间戳回绕

```
0xFFFFFF00 + 480 = 0x000000E0
扩展时间戳：0xFFFFFF00 -> 0x1000000E0
```

### B.4 控制头

```
版本=0x03，type=1，payload_len=15
device_id=01 23 45 67 89 AB CD EF

头：
  03 00 01 00 0F 00 00 01 23 45 67 89 AB CD EF
```

### B.5 CONNECT 负载

```
C3 50 11 22 33 44 6F 0A 03 01 04 03 00 00 05

reverse_port=0xC350 (50000)
forward_ssrc=0x11223344
payload_type=0x6F (111)
frame_ms=0x0A (10)
complexity=3
signal=1 (voice)
bandwidth=4 (FB)
opus_flags=0x03 (VBR + DTX)
bitrate_kbps=0 (auto)
packet_loss=5
```

### B.6 CONNECT_ACK 负载

```
AA BB CC DD 6F 0A

reverse_ssrc=0xAABBCCDD
payload_type=0x6F (111)
frame_ms=0x0A (10)
```

### B.7 包长边界

```
Opus 负载 1275 字节
RTP 包 1287 字节
UDP/IP 1315 字节
```

---

## 附录 C：v2 → v3 关键差异

| 项目 | v2 | v3 |
|------|----|----|
| 媒体包 | 15 字节私有头 + Opus | 12 字节 RTP 头 + Opus |
| 媒体身份 | 每包 device_id | 控制面绑定 + SSRC |
| 媒体序号 | u8 | RTP u16 + 扩展 |
| 媒体时间戳 | 无 | RTP 48 kHz |
| 帧长 | 20 ms 为主 | 固定 10 ms |
| 音频时钟 | 定时器/队列水位混合 | 声卡播放头唯一时钟 |
| 发送端 | 有队列/协程 | 无队列，直接发送 |
| 接收缓冲 | 自定义 ReorderBuffer | 扩展 seq/ts 抖动缓冲 |
| PLC | 定时器触发 | 播放头到期触发 |
| 控制版本 | 0x02 | 0x03 |
| 兼容 | 仅 v2 | 仅 v3，干净断代 |
| 最大码率 | 1024 kbps（有超限风险） | 512 kbps，严格 ≤1275 字节负载 |
