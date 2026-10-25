# v3 迁移实施计划（已归档）

> 2026-10-24 v2→v3 一次性重写的落地顺序与逐文件改动清单。
> 迁移已于 v1.2.0 完成，本节原为协议规范 §11.5 / §11.6；
> 保留仅为追溯当时的改动范围，**不再维护**，现状以代码与
> [PROTOCOL_V3_RTP.md](../PROTOCOL_V3_RTP.md) 为准。

---

### 11.5 实施顺序

建议按以下顺序落地，避免同时改动所有链路：

1. **协议核心 + 测试**
   - `protocol/protocol.rs`：控制 v3 头、CONNECT/ACK/发现编解码；
   - `protocol/rtp.rs`：RTP 头、扩展 seq/ts、常量；
   - Kotlin `Udp2CalProtocol.kt` 镜像；
   - 先跑通字节级测试，不接音频。

2. **正向链路**
   - Android 发送 RTP；
   - Windows 主 socket 解复用、校验、抖动缓冲；
   - Windows 解码线程 + PCM 环 + 播放头；
   - 先单声道 48 kHz、512 kbps。

3. **反向链路**
   - Windows 反向发送 RTP；
   - Android 反向接收、AudioTrack 拉取；
   - ACK 下发反向 SSRC；
   - 10 ms PLC。

4. **清理旧路径**
   - 删除 v2 音频头、`ReorderBuffer`、`Channel`、PCM 队列、定时器、3 秒 EMA、2 ms 轮询、3 秒 `pcm_buf`；
   - 更新 `docs/PROTOCOL.md` 为历史文档并链接本文；
   - 更新 `ARCHITECTURE.md`。

### 11.6 迁移清单

| 文件 | 改动 |
|------|------|
| `protocol/protocol.rs` | 控制版本 3；15 字节控制头；删除音频字段；CONNECT/ACK/发现负载；测试 |
| `protocol/rtp.rs` | 新增 RTP 编解码、扩展 seq/ts、包长校验、测试 |
| `windows/src/protocol.rs` | 重导出新模块；删除 v2 音频接口 |
| `android/.../Udp2CalProtocol.kt` | 镜像控制 v3 + RTP；删除 `writeAudioHeader`；新增 `writeRtpHeader` |
| `android/.../UdpSender.kt` | 主 socket 发送 RTP + CONNECT；解析 ACK 负载；维护 `forwardSsrc`/`reverseSsrc`；反向 socket 只收 RTP |
| `android/.../native/OpusEncoder.kt` | 删除 v2 头和 `seqNum`；负载写到偏移 12；帧长 10 ms |
| `android/app/src/main/cpp/opus_jni.c` | 最大输出 1275 字节；确认 10 ms 帧；不得截断 |
| `android/.../native/OpusDecoder.kt` | `frameSize=480`；PLC 480；声道按方向 |
| `android/app/src/main/cpp/opus_jni_decoder.c` | 解码/PLC 帧长 10 ms（480），不再硬编码 20 ms |
| `android/.../service/CaptureService.kt` | 删除 `Channel<ShortArray>(3)`、`ShortArrayPool`、双协程；采集线程直接编码发送；控制独立 |
| `android/.../AudioPlayer.kt` | 删除 `ShortQueue`/高水位 pump；实现按播放头拉取、阻塞 `write`、最小缓冲 + low-latency |
| `windows/src/main.rs` | 解复用 RTP/控制；RTP 校验；抖动缓冲；控制 v3；删除 `recv_timeout(30ms)`、60 ms PLC 计时、`Instant` 重排、3 秒 EMA |
| `windows/src/decoder.rs` | 固定 48 kHz 单声道正向解码；PLC 每次 480 采样；删除 120 ms PLC 用法 |
| `windows/src/audio.rs` | 保留无锁 PCM 环；增加 `played_frames`/播放头；删除 3 秒漂移 EMA；水位校正移到解码/抖动缓冲侧 |
| `windows/src/capture.rs` | 事件驱动环回；删除 2 ms 轮询、`pcm_buf` 3 秒上限；10 ms 组帧；RTP 发送；自动码率 512 |
| `docs/PROTOCOL.md` | 标记 v2 历史；链接本文 |
| `docs/ARCHITECTURE.md` | 更新管线、线程、时钟模型 |

