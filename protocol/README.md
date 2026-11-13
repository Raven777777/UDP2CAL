# udp2cal-protocol

UDP2CAL **协议 v3** 的 Rust 实现（协议唯一真源）。

| 文件 | 内容 |
| --- | --- |
| `protocol.rs` | 控制面：15 字节固定头、`CONNECT` / `CONNECT_ACK` / `DISCOVER_*` 负载、同 socket 解复用、设备 ID |
| `rtp.rs` | 媒体面：12 字节 RTP 头、扩展 seq/ts、`JitterBuffer`（水位驱动的按时位拉取 / PLC）、`DeviceRateEstimator`（声卡钟差前馈）、`DriftController`（水位 PI） |

规范文档：[`../docs/PROTOCOL_V3_RTP.md`](../docs/PROTOCOL_V3_RTP.md)
（v2 已归档在 [`../docs/archive/PROTOCOL_V2.md`](../docs/archive/PROTOCOL_V2.md)，**不可互通**）

```
cargo test          # 协议单元测试（含附录 B 的全部字节向量）
```

被 `windows/` 作为本地依赖引用；Android 端将本 crate 编译为 JNI 库，`Udp2CalProtocol.kt` 只是薄 Kotlin 门面，
不再维护第二份协议实现。Rust 与 Android 测试共用同一组十六进制向量；字段变更需同步更新协议规范和测试向量。
