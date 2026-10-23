# udp2cal-protocol

UDP2CAL 私有协议 v2 的 Rust 实现（协议唯一真源，文件 `protocol.rs`）。

**协议规范文档已迁移至 [`../docs/PROTOCOL.md`](../docs/PROTOCOL.md)** —— 包头布局、消息类型、采样率-码率关联、P2P 独占生命周期、常量表与用法示例。

```
cargo test          # 运行协议单元测试
```

被 `windows/` 作为本地依赖引用；Android 端 `Udp2CalProtocol.kt` 为手动对齐实现。
