# UDP2CAL Bug 修复记录

> 记录项目中已修复的 bug 及其根因分析、修复方式。  
> 格式：`YYYY-MM-DD` — 按时间倒序排列。

---

## 2026-06-09 — android_old 发热优化（CPU 120% → 9%）

### 概述

android_old 在低端 Sharp 设备上运行 CPU 占用 117-120%（Opus 编码线程 97.9% R 状态），电池温度持续上升。ADB 诊断定位到多处问题叠加导致发热。

### 修复清单

| # | 问题 | 根因 | 修复 |
|---|------|------|------|
| 1 | `drainAck()` 每帧忙轮询 | `sock.soTimeout = 1`，50 帧/秒 × 1ms 空轮询 | 每 10 帧(~5Hz)检查一次，soTimeout 1→5ms |
| 2 | Opus 编码 CPU 占满 | 48kHz 编码 + 50 帧/秒，低端设备算力不足 | 固定 16kHz 采集编码，PC 端低性能模式降采样 48k→16k |
| 3 | 反向解码采样率不匹配 | reverseDecoder 用了 forward 的 sampleRate(24000) 解码 48kHz Opus → 颤音 | 固定 16000Hz 解码器，匹配 PC 低性能模式 |
| 4 | AudioPlayer 播速 3 倍 | AudioTrack 固定 48000Hz 播放 16000Hz PCM → 升调失真 | AudioPlayer 固定 16000Hz |
| 5 | 编码频率过高 | 20ms 帧，每秒编码 50 次 | 合并 2 帧为 40ms 打包编码，频率降至 25 帧/秒 |
| 6 | 采集 read 失败忙等 | `if (read <= 0) continue` 导致异常时 100% 忙等 | 添加 `delay(1)` 让出 CPU |
| 7 | WakeLock 10 分钟超时 | `acquire(10 * 60 * 1000L)` 超时后自动释放 | 改为 `acquire()` 无限期保持，finally 释放 |
| 8 | PC 反向编码器采样率过高 | 低性能模式仍然用 48000Hz 编码，仅带宽限幅 | PC `capture.rs` 改为 16000Hz 编码器 + 48kHz→16kHz 3:1 降采样 |
| 9 | 包头采样率不实 | PC 低性能模式包头仍标 `sample_rate=4`(48kHz) | 包头正确标注 `sample_rate=2`(16kHz) |

### 性能对比

| 指标 | 优化前 (48000Hz / 20ms帧) | 优化后 (16000Hz / 40ms帧) |
|------|:-:|:-:|
| CPU 总占用 | 117-120% | **~9%** |
| 编码线程状态 | **R(运行) 97.9%** | **S(睡眠) 4.4%** |
| 电池温度 | 35→37°C（上升） | **36°C（稳定）** |

---

## 2026-06-09 — 全面改名 UDP2CAL + 广播控制 + 包名更新

### 概述

项目全面从 UDP2Mic 更名为 UDP2CAL，包名从 `com.udp2mic.app` 改为 `com.udp2cal.app`，清理所有残留旧引用。修复 UDP 广播在应用停止/已占用时仍响应的安全问题。

### 修复清单

| # | 问题 | 根因 | 修复 |
|---|------|------|------|
| 1 | 停止后 UDP 广播仍响应 | `start_broadcast_listener` 未检查 `APP_RUNNING` | 监听线程加入 `APP_RUNNING` 判断 |
| 2 | 已占用时仍可被搜索发现 | `start_broadcast_listener` 未检查 `DEVICE_READY` | 监听线程加入 `GLOBAL_DEVICE_STATE == DEVICE_READY` |
| 3 | 停止后主动广播仍进行 | `start_broadcast_state_machine` 未检查 UI 状态 | 主动广播线程加入 `APP_RUNNING` 判断 |
| 4 | android_old 闪退 | JNI 函数名未随包名更新 | 所有 C 文件 JNI 函数名 `com_udp2mic` → `com_udp2cal` |
| 5 | 构建失败 Theme 引用错误 | `AndroidManifest.xml` 仍引用旧 `Theme.UDP2CALL` | 更新为 `Theme.UDP2CAL` |
| 6 | 多文件遗留旧名 | 注释、Prefs、CMakeLists、布局等未更新 | 全面扫描清理 |

---

## 2026-06-09 — 双向音频串流 + AEC 移植与修复

### 概述

将 `windows_cc` / `android_old_cc` 的双向音频串流（PC 扬声器→手机听筒）及 AEC 功能移植到面向现代化设备的 `windows` / `android` 代码库。

### 修复清单

| # | 问题 | 根因 | 修复 |
|---|------|------|------|
| 1 | 保活重连每秒重启反向发送器 | keepalive CONNECT 携带反向端口，被误判为端口变化 | 仅端口真正变化时重启 |
| 2 | Win 重启后反向串流不恢复 | `REVERSE_ENABLED` 是进程级 static | 启动时从 config 同步 |
| 3 | 模式切换后反向串流卡死 | 端口变化需重启发送器；模式切换销毁 UdpSender | 端口变化时自动重启；模式切换保留 UdpSender |
| 4 | 音乐模式无声音 | `routeToEarpiece(false)` 改变全局音频路由模式，干扰 AudioRecord | 音乐模式跳过路由变更 |
| 5 | 音乐模式单声道（顶部扬声器） | AudioPlayer 固定走 VOICE_COMMUNICATION + 听筒路由 | 语音→听筒，音乐→扬声器 |
| 6 | 通知栏状态不更新 | `buildNotification()` 只调用一次 | 添加 `_status.collect` 协程动态更新 |
| 7 | 延迟 ~500ms | WASAPI 缓冲 100ms + AudioTrack 缓冲 300ms | WASAPI→30ms，AudioTrack→80ms（预计 ~150ms） |
| 8 | 低性能模式无声 | 立体声→单声道下混逻辑有 bug（原地修改破坏数据） | 重写为独立 mono_buf 累积+下混 |

### 协议变更

- CONNECT payload 扩展为 3 字节：2 字节反向端口 + 1 字节模式标志（0=高品质/1=低性能）
- 移除旧版纯文本发现协议支持

---

## 2026-06-07 — Android 端 UDP 乐观连接

### 现象

手机使用移动数据（未连接 WiFi）时点击"开始采集"，Opus 状态立即显示"已连接"，编码器正常工作，但实际上音频包全部发到虚空。

### 根因

两处乐观初始化导致连接状态脱离真实网络状况：
1. `CaptureService.kt` 音频引擎启动后立即标记 `connected = true`
2. 消费者协程 `p2pConnected` 初始化为 `true`

### 修复

- 启动后等待 `CONNECT_ACK` 才标记连接
- 每帧非阻塞 `drainAck()` 1ms 超时检测
- 断开时持续发 CONNECT 保活等待重连
