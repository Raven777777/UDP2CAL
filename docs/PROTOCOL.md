# UDP 私有协议 v2

> 协议唯一真源：`protocol/protocol.rs`（Rust crate `udp2cal-protocol`）
> 相关文档：[架构说明](ARCHITECTURE.md) · [Bug 修复记录](BUGFIX.md)

---

## 一、统一协议（音频 + 控制共用）

v2 统一协议替代了旧版分离的 v1（6B 音频头）和旧 v2（15B "UD2M" 控制头）。**所有包使用同一 15 字节包头格式，携带设备 ID 实现 1对1 P2P 过滤。**

首字节 `0x02` 标识 v2 协议，与旧 v1（首字节 `0x80+`）永不冲突。

### 包格式（15 字节固定头 + 负载）

```
Byte 0:       PROTO_VERSION = 0x02
Byte 1:       [1bit is_audio][3bit codec][4bit sample_rate]
              is_audio=1 音频数据, =0 控制消息; codec=1 (Opus)
Byte 2:       [8bit msg_type]
              0=TYPE_DATA, 1=TYPE_CONNECT, 2=TYPE_DISCOVER_REQ,
              3=TYPE_DISCOVER_REPLY, 4=TYPE_CONNECT_ACK
Byte 3-4:     [16bit payload_len BE]
Byte 5:       [8bit bitrate_kbps] (0=auto, 仅 TYPE_DATA 有效)
Byte 6:       [8bit seq_num] (仅 TYPE_DATA 有效)
Byte 7-14:    [8 bytes device_id] (发送端唯一标识)
Byte 15+:     [payload]
```

---

## 二、消息类型

| 类型 | 值 | 方向 | 说明 |
|------|-----|------|------|
| `TYPE_DATA` | 0 | Android → Windows | Opus 音频数据 |
| `TYPE_CONNECT` | 1 | Android → Windows | 连接请求/保活 |
| `TYPE_DISCOVER_REQ` | 2 | Android → 广播(44043) | 发现请求 |
| `TYPE_DISCOVER_REPLY` | 3 | 单播回复 | 发现回复(携带端口+设备名) |
| `TYPE_CONNECT_ACK` | 4 | Windows → Android | 连接确认(保活回复) |

### CONNECT 载荷扩展

CONNECT 保活包在基础结构之外携带：

| 字段 | 长度 | 说明 |
|------|------|------|
| 反向端口 | 2 字节 | PC 向手机发送反向音频的目标端口 |
| 模式标志 | 1 字节 | 0=高品质 / 1=低性能 |
| Opus 参数 | 10 字节 | 手机端码率/带宽/复杂度/VBR/DTX/FEC/丢包率，供 PC 反向编码器热同步 |

> 手机调参后，PC 端 `capture.rs` 每秒检查一次配置并热应用 `opus_encoder_ctl`，实现反向串流参数实时同步。

---

## 三、采样率-码率自动关联

| 采样率 ID | 采样率 | 自动码率 | 协议编码字节 |
|-----------|--------|---------|-------------|
| 0 | 8kHz | 64 kbps | 32 |
| 1 | 12kHz | 64 kbps | 32 |
| 2 | 16kHz | 128 kbps | 64 |
| 3 | 24kHz | 256 kbps | 128 |
| 4 | 48kHz | 512 kbps | 255 |

> 协议字节 = `(kbps/2).min(255).max(1)`

---

## 四、设备 ID

- 8 字节唯一标识，时间戳哈希 + 伪随机数
- Android：`SharedPreferences` 持久化
- Windows：注册表 `HKCU\Software\UDP2CAL\device_id`
- **所有包均携带**，音频包用于 1对1 过滤，控制包用于身份鉴权

---

## 五、P2P 独占通信生命周期

### 安全准入模型

```
TYPE_CONNECT 是唯一建连通道
DEVICE_READY 时拒绝所有 TYPE_DATA（防止音频包绕过鉴权）
DEVICE_BUSY 时仅接受绑定设备 ID 的 TYPE_DATA
```

### 交互流程

```
1. 发现:  Android → DISCOVER_REQ(广播44043) → Windows 回复 DISCOVER_REPLY(端口+名)
2. 连接:  Android → CONNECT(device_id) → Windows 绑定 → 回复 CONNECT_ACK
3. 音频:  Android → DATA(device_id, Opus) → Windows 校验 device_id → 播放
4. 保活:  每1s Android CONNECT → Windows ACK, 3s 无 ACK 标记断连
5. 重连:  Android 持续发 CONNECT → Windows 恢复后 ACK → 自动恢复
6. 抢占:  异设备 CONNECT → DEVICE_BUSY 拒绝; 重启 Win 即可清空非法绑定
```

### 状态机

```
广播状态机:  Ready ──(占用)──→ Silent ──(释放)──→ Ready
连接状态机:  Idle  ──(CONNECT)──→ Busy ──(停止/超时)──→ Idle
```

| 状态 | 行为 |
|------|------|
| `DEVICE_READY` + `Ready` | 每秒广播 DISCOVER_REPLY 到 255.255.255.255:44043；接受 CONNECT；丢弃 DATA |
| `DEVICE_BUSY` + `Silent` | 停止广播（不可被发现）；仅接受绑定设备 ID 的 DATA / CONNECT 保活；异设备 CONNECT 静默丢弃 |

---

## 六、常量

| 常量 | 值 | 说明 |
|------|-----|------|
| `HEADER_SIZE` | 15 | 包头固定字节数 |
| `MAX_PAYLOAD` | 1472 | 最大负载 (MTU 安全) |
| `MAX_PACKET` | 1487 | 最大完整包 |
| `MAX_REORDER` | 2 | 乱序重排窗口大小（低延迟优化，原 8→2） |
| `DEVICE_ID_SIZE` | 8 | 设备 ID 字节数 |
| `DISCOVER_PORT` | 44043 | 发现广播端口 |

---

## 七、实现

| 平台 | 文件 | 语言 |
|------|------|------|
| Rust crate | `protocol/protocol.rs` | Rust（唯一协议真源） |
| Windows 接收端 | `windows/src/protocol.rs` | 重导出 |
| Android 发送端 | `Udp2CalProtocol.kt` | Kotlin（手动对齐） |

> Kotlin 端为手动对齐实现，任何协议字段变更必须同步修改 `protocol/protocol.rs`、`windows/src/protocol.rs`（若涉及重导出）以及 `android/`、`android_old/` 两份 `Udp2CalProtocol.kt`。
>
> 历史教训：Kotlin 版 `sampleRateToHz` 默认值曾为 `16000`，与 Rust 版 `48000` 不一致，已修正对齐（见 [BUGFIX.md](BUGFIX.md)）。

---

## 八、用法（Rust）

```rust
use udp2cal_protocol::*;

// 构建音频包
let dev_id = generate_device_id();
let audio_packet = build_packet(true, TYPE_DATA, SAMPLE_RATE_48K, seq, &dev_id, &opus_frame, BITRATE_AUTO);

// 构建连接请求
let connect_packet = build_packet(false, TYPE_CONNECT, 0, 0, &dev_id, &[], 0);

// 解码
let buf: [u8; HEADER_SIZE] = packet[..HEADER_SIZE].try_into().unwrap();
if let Some(h) = decode_header(&buf) {
    // h.is_audio, h.msg_type, h.device_id, h.sample_rate, h.seq_num, ...
}

// 零分配重排序
let mut rb = ReorderBuffer::new();
rb.insert_and_drain(h.seq_num, h.sample_rate, &payload, |sr, data| {
    // 按序到达的帧
});
```
