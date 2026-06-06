# UDP2Mic UDP私有协议 v1

## 协议格式 (大端序)

```
Byte 0:  [1bit 版本:1] [3bit 编码:1] [4bit 采样率]
Byte 1:  [8bit 包序号]
Byte 2-3: [16bit 负载长度 (大端)]
Byte 4:  [8bit 码率 (kbps, 0=auto)]
Byte 5:  [8bit 保留/扩展标志]
Byte 6+:  [Opus 音频负载]
```

## 字段说明

| 位域 | 取值 | 含义 |
|------|------|------|
| 版本 (1bit) | 1 | 当前协议版本 |
| 编码 (3bit) | 1 | Opus |
| 采样率 (4bit) | 0-4 | 8kHz / 12kHz / 16kHz / 24kHz / 48kHz |
| 包序号 (8bit) | 0-255 | 循环自增, 接收端乱序重排 |
| 负载长度 (16bit) | 0-1472 | Opus 帧字节数 (大端) |
| 码率 (8bit) | 0-255 | 编码码率 kbps, 0=auto/unknown |
| 扩展 (8bit) | 0 | 保留以备未来扩展 |

## 采样率-码率自动关联

当发送端码率字段设为 0（`BITRATE_AUTO`）时，接收端根据采样率自动匹配默认码率：

| 采样率 | Android 自动码率 | 协议编码字节 (kbps/2) |
|--------|-----------------|---------------------|
| 8kHz   | 64 kbps         | 32                  |
| 12kHz  | 64 kbps         | 32                  |
| 16kHz  | 128 kbps        | 64                  |
| 24kHz  | 256 kbps        | 128                 |
| 48kHz  | 512 kbps        | 255 (OPUS 510kbps 天花板) |

> 协议字节 = `compute_bitrate_id(kbps)` = `(kbps/2).min(255).max(1)`。512→256→coerced 255→解码 510kbps。

## 常量

| 常量 | 值 | 说明 |
|------|-----|------|
| `HEADER_SIZE` | 6 | 包头固定字节数 |
| `MAX_PAYLOAD` | 1472 | 最大负载 (MTU 安全) |
| `MAX_PACKET` | 1478 | 最大完整包 = 6 + 1472 |
| `MAX_REORDER` | 8 | 乱序重排窗口大小 |
| `BITRATE_AUTO` | 0 | 自动码率标记 |

## 实现

| 平台 | 文件 | 语言 |
|------|------|------|
| Rust crate | `protocol/protocol.rs` | Rust (唯一协议真源) |
| Windows 接收端 | `windows/src/protocol.rs` | 重导出 `udp2mic_protocol::*` |
| Android 发送端 | `Udp2MicProtocol.kt` | Kotlin (手动对齐，基准) |

## 用法 (Rust)

```rust
use udp2mic_protocol::*;

// 编码包
let packet = build_packet(SAMPLE_RATE_48K, seq_num, &opus_frame, BITRATE_AUTO);

// 解码包
let buf: [u8; HEADER_SIZE] = packet[..HEADER_SIZE].try_into().unwrap();
if let Some(header) = decode_header(&buf) {
    // header.sample_rate, header.seq_num, header.payload_len, header.bitrate
}

// 乱序重排（零分配回调模式）
let mut rb = ReorderBuffer::new();
rb.insert_and_drain(header.seq_num, header.sample_rate, payload, |sr, data| {
    // 按序到达的完整帧，sr=采样率ID, data=Opus负载
});

// 码率工具
let id = compute_bitrate_id(512); // 255
let br = resolve_bitrate(BITRATE_AUTO, SAMPLE_RATE_48K); // 255 (512kbps)
```