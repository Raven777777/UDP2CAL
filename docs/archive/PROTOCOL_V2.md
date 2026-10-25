# UDP 私有协议 v2（历史存档）

> ⚠️ **本文档描述的 v2 协议已被 v3 取代，两者不可互通。**
>
> 现行规范：[**PROTOCOL_V3_RTP.md**](../PROTOCOL_V3_RTP.md) —— 控制协议 `0x03` +
> 标准 RTP（PT=111，48 kHz，固定 10 ms）+ 声卡播放头时钟模型。
>
> 本文仅作历史参考：v2 的 15 字节私有头、8 位序号、20 ms 帧长、定时器驱动的 PLC
> 全部已删除。任何按本文实现的客户端**无法**连接当前版本的 Android / Windows 端。

---

## 一、v2 统一包头（已废弃）

所有包（音频 + 控制）共用同一 15 字节头，首字节必须为 `0x02`：

```
Byte 0:       0x02
Byte 1:       [1bit is_audio][3bit codec][4bit sample_rate]
Byte 2:       msg_type: 0=DATA, 1=CONNECT, 2=DISCOVER_REQ, 3=DISCOVER_REPLY, 4=CONNECT_ACK
Byte 3-4:     payload_len (u16 BE)
Byte 5:       bitrate/2 kbps（0=auto，仅 DATA 有效）
Byte 6:       seq_num（u8，仅 DATA 有效）
Byte 7-14:    device_id[8]
Byte 15+:     payload
```

## 二、v2 消息类型

| 类型 | 值 | 方向 |
| --- | --- | --- |
| `TYPE_DATA` | 0 | Android → Windows（Opus 音频） |
| `TYPE_CONNECT` | 1 | Android → Windows（首包仅 2 字节反向端口） |
| `TYPE_DISCOVER_REQ` | 2 | Android → 广播 44043 |
| `TYPE_DISCOVER_REPLY` | 3 | Windows → Android |
| `TYPE_CONNECT_ACK` | 4 | Windows → Android（空负载） |

## 三、v2 采样率-码率关联

| 采样率 ID | 采样率 | 自动码率 | 协议字节 |
| --- | --- | --- | --- |
| 0 | 8 kHz | 64 kbps | 32 |
| 1 | 12 kHz | 64 kbps | 32 |
| 2 | 16 kHz | 128 kbps | 64 |
| 3 | 24 kHz | 256 kbps | 128 |
| 4 | 48 kHz | 512 kbps | 255 |

## 四、v2 的已知问题

v2 把两件不同的事混在一起，最终导致延迟与抖动的系统性劣化：

1. **媒体身份与序号放在每个音频包里**（`device_id` + 8 位 `seq`），
   接收端只能靠自定义 `ReorderBuffer` 猜乱序，没有时间戳可以判断"该播哪一帧"；
2. **播放节奏由定时器/队列水位驱动**：`recv_timeout(30ms)`、60 ms PLC 计时、
   `Instant` 重排衰减、3 秒漂移 EMA。一旦声卡时钟与定时器漂移，
   要么缓冲持续增长（延迟越用越大），要么反复欠载（爆音）。

v3 用"标准 RTP 线格式 + 声卡播放头唯一时钟"替换了这两套机制。

## 五、v2 → v3 对照

| 项目 | v2 | v3 |
| --- | --- | --- |
| 媒体包 | 15 字节私有头 + Opus | 12 字节 RTP 头 + Opus |
| 媒体身份 | 每包 `device_id` | 控制面绑定 + SSRC |
| 媒体序号 | u8 | RTP u16 + 64 位扩展 |
| 媒体时间戳 | 无 | RTP 48 kHz，每帧 +480 |
| 帧长 | 20 ms 为主 | 固定 10 ms |
| 音频时钟 | 定时器 + 水位混合 | 声卡播放头唯一 |
| 发送端 | 有队列/协程 | 无队列，直接发送 |
| 接收缓冲 | 自定义 `ReorderBuffer` | 扩展 seq/ts 抖动缓冲 |
| PLC | 定时器触发 | 播放头到期触发 |
| 控制版本 | `0x02` | `0x03` |
| 兼容 | 仅 v2 | 仅 v3（干净断代） |
| 最大码率 | 1024 kbps（有超限风险） | 1000 kbps，严格 1275 字节负载 |

## 六、当前实现位置

| 平台 | 文件 |
| --- | --- |
| Rust 协议真源（控制面） | `protocol/protocol.rs` |
| Rust 协议真源（媒体面） | `protocol/rtp.rs` |
| Windows | `windows/src/{main,capture,decoder,audio}.rs` |
| Android | `Udp2CalProtocol.kt` / `UdpSender.kt` / `CaptureService.kt` / `AudioPlayer.kt` |

> v2 时代的代码（`build_packet`、`decode_header`、`ReorderBuffer`、
> `writeAudioHeader`、`TYPE_DATA`）已在 v1.2.0 中删除。`TYPE_DATA` 常量仍然保留在
> 类型表里，但收到即丢弃——媒体不再走控制类型。
