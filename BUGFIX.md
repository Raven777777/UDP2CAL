# UDP2Mic Bug 修复记录

> 记录项目中已修复的 bug 及其根因分析、修复方式。  
> 格式：`YYYY-MM-DD` — 按时间倒序排列。

---

## 2026-06-07 — Android 端 UDP 乐观连接：流量下显示"已连接"

### 现象

手机使用移动数据（未连接 WiFi）时点击"开始采集"，Opus 状态立即显示"已连接"，编码器正常工作，但实际上音频包全部发到虚空，接收端从未收到任何数据。

### 根因

两处乐观初始化导致连接状态脱离真实网络状况：

1. **`CaptureService.kt` L254**：音频引擎启动后立即标记 `connected = true`
2. **`CaptureService.kt` L309**：消费者协程 `p2pConnected` 初始化为 `true`

配合 `UdpSender.connect()` 中的 `DatagramSocket.send()` 无条件返回 `true`（UDP 无连接语义，发完就认为成功），导致即便目标 IP 不存在，流程也一路绿灯进入"已连接"状态。

### 修复

| 文件 | 行号 | 改动 |
|------|------|------|
| `CaptureService.kt` | 254 | `connected = true` → `connected = false` |
| `CaptureService.kt` | 309 | `var p2pConnected = true` → `var p2pConnected = false` |

**逻辑变更**：
- 启动采集后：`status.connected = false`，消费者 `p2pConnected = false`
- 消费者每帧调用 `drainAck()` 非阻塞检测 `CONNECT_ACK`（1ms 超时）
- 仅在真正收到 ACK 时：`p2pConnected = true`，开始编码/发送音频
- 断开时：仅停止发包，持续发 CONNECT 保活等待 ACK 恢复

**影响范围**：Android 发送端连接状态显示、P2P 发包控制逻辑。
