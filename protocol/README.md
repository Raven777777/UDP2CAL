# UDP2CAL UDP私有协议 v2

## 统一协议 (音频 + 控制共用)

v2 统一协议替代了旧版分离的 v1（6B 音频头）和旧 v2（15B "UD2M" 控制头）。**所有包使用同一 15 字节包头格式，携带设备 ID 实现 1对1 P2P 过滤。**

首字节 `0x02` 标识 v2 协议，与旧 v1（首字节 `0x80+`）永不冲突。

### 包格式 (15字节固定头 + 负载)

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

### 消息类型

| 类型 | 值 | 方向 | 说明 |
|------|-----|------|------|
| `TYPE_DATA` | 0 | Android → Windows | Opus 音频数据 |
| `TYPE_CONNECT` | 1 | Android → Windows | 连接请求/保活 |
| `TYPE_DISCOVER_REQ` | 2 | Android → 广播(44043) | 发现请求 |
| `TYPE_DISCOVER_REPLY` | 3 | 单播回复 | 发现回复(携带端口+设备名) |
| `TYPE_CONNECT_ACK` | 4 | Windows → Android | 连接确认(保活回复) |

### 采样率-码率自动关联

| 采样率 ID | 采样率 | 自动码率 | 协议编码字节 |
|-----------|--------|---------|-------------|
| 0 | 8kHz | 64 kbps | 32 |
| 1 | 12kHz | 64 kbps | 32 |
| 2 | 16kHz | 128 kbps | 64 |
| 3 | 24kHz | 256 kbps | 128 |
| 4 | 48kHz | 512 kbps | 255 |

> 协议字节 = `(kbps/2).min(255).max(1)`

### 设备 ID

- 8 字节唯一标识，时间戳哈希 + 伪随机数
- Android: `SharedPreferences` 持久化
- Windows: 注册表 `HKCU\Software\UDP2CAL\device_id`
- **所有包均携带**，音频包用于 1对1 过滤，控制包用于身份鉴权

## P2P 独占通信生命周期

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

### 常量

| 常量 | 值 | 说明 |
|------|-----|------|
| `HEADER_SIZE` | 15 | 包头固定字节数 |
| `MAX_PAYLOAD` | 1472 | 最大负载 (MTU 安全) |
| `MAX_PACKET` | 1487 | 最大完整包 |
| `MAX_REORDER` | 2 | 乱序重排窗口大小（低延迟优化，原 8→2） |
| `DEVICE_ID_SIZE` | 8 | 设备 ID 字节数 |
| `DISCOVER_PORT` | 44043 | 发现广播端口 |

### 实现

| 平台 | 文件 | 语言 |
|------|------|------|
| Rust crate | `protocol/protocol.rs` | Rust (唯一协议真源) |
| Windows 接收端 | `windows/src/protocol.rs` | 重导出 |
| Android 发送端 | `Udp2CalProtocol.kt` | Kotlin (手动对齐) |

### 用法 (Rust)

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
