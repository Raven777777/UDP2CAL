# 功能特性

> 相关文档：[构建说明](BUILD.md) · [架构说明](ARCHITECTURE.md) · [低性能版专项](ANDROID_OLD.md)

---

## 一、系统概述

```
Android 手机 ↔ 双向音频 ↔ Windows PC
  正向: 手机麦克风 → Opus 编码 → UDP → PC → WASAPI → VB-Cable → 任意应用
  反向: PC 扬声器 → WASAPI Loopback → Opus 编码 → UDP → 手机听筒/扬声器
```

UDP2CAL 是一个**双向、低延迟**的局域网音频串流系统。提供两个 Android 客户端版本：

- **`android/`（现代版）** — 面向 Jetpack Compose 设备，固定 48kHz 采集，支持全参数 Opus 调节 + 反向编码实时同步；
- **`android_old/`（低性能版，已归档）** — 面向 Android 6~8 低端/翻盖机，源码仅作归档；**v1.1.2 起 PC 端已删除低性能适配，旧 APK 与当前 PC 端不互通**。

**全链路零堆分配**（ByteArray + ShortArray 双闭环，仅池热身期 3 次构造）、**生产-消费双协程**消除阻塞饥饿、**JNI 双重边界守卫**防越界写穿。

---

## 二、v1.1.0 版本要点

- 双向音频：手机麦克风→PC (VB-Cable) + PC 扬声器→手机听筒/扬声器
- 反向串流：WASAPI Loopback → Opus 编码 → UDP 发送（Windows UI 按钮控制启停）
- 声学回声消除：语音模式启用 AcousticEchoCanceler + MODE_IN_COMMUNICATION + 同 session 绑定
- 立体声/单声道自适应：现代手机立体声编码，低端设备单声道降级（CONNECT payload 模式标志，v1.1.2 已移除）
- **android_old 发热优化**：48kHz FB + 40ms 帧合并 + JNI 动态帧长，CPU 120%→14%
- **android_old 反向解码立体声修复**：`opus_jni_decoder.c` 单声道解码器→立体声，修复 PC 高品质模式反向无声
- v2 控制协议：二进制发现、独占连接（TYPE_CONNECT/ACK）、8 字节设备 ID 鉴权
- 双状态机：广播状态机（Ready/Silent）+ 连接状态机（Idle/Busy），全局原子状态同步
- 心跳熔断：3 秒 ACK 超时标记断连，1 秒无包自动释放独占，防碰撞防抢占
- 防抢占静默拒绝：BUSY 时异设备 TYPE_CONNECT 不回复 ACK，新设备永无法发送音频，已有连接完全不受影响
- 保活 CONNECT 携带反向端口：Win 重启后自动恢复反向串流
- 全链路生命周期：ACK 超时断连 / 无包超时释放 / 并发锁绑定，无需重启

---

## 三、双向音频串流（v1.1.0 新增）

- **正向**：手机麦克风 → Opus 编码 → UDP → PC（VB-Cable 输出），逻辑不变
- **反向**：PC 扬声器 → WASAPI Loopback → Opus 编码 → UDP → 手机听筒/扬声器播放
- **Windows 端开关控制**：UI 按钮（开机自启右侧）控制反向串流启停，运行中不可更改
- **立体声编码**：反向串流固定 48kHz 立体声、最大码率、全频带，手机端显示带宽+码率
- **反向参数热同步**：手机端 Opus 面板调整后，PC 反向编码器每秒读取 CONNECT 载荷并热应用，无需重启串流

---

## 四、Android 发送端（现代版，`android/`）

- **音源自适应切换**：仅一个 `isVoiceMode` 状态变量驱动全部音源决策。语音模式→VOICE_COMMUNICATION（安卓系统原生硬件降噪+AEC），全频模式→MIC（裸麦直出，硬件降噪关闭）；异常回退 MIC 保底通路
- **声学回声消除**：语音模式启用 AcousticEchoCanceler + MODE_IN_COMMUNICATION AudioTrack + 同音频 session 绑定
- **生产-消费双协程**：独占线程 `AudioRecord.read()` + `Channel` 投递消费者，消除饥饿。信号类型变更时消费者循环检测→自动重建 AudioRecord，保留网络层和反向播放器
- **全链路零堆分配**：`ShortArrayPool` 帧复用 + 乒乓发送缓冲区 + `encoderEncodeTo` 直接写入
- **纯直通无软件处理**：AudioRecord 取 PCM → 直通拷贝 → Opus 编码输出，APP 端无任何降噪/AGC/音效代码，硬件降噪由安卓系统全权托管
- **Opus CBR/VBR 热调节**：码率 32~512kbps；固定 48kHz 采集，协议层支持 5 种采样率（8k/12k/16k/24k/48k）
- **Opus 高级编码设置面板**：Jetpack Compose 全参数面板（复杂度/信号类型/带宽/VBR/FEC/DTX/丢包率/码率），面板内集成运行时状态卡片（音源/Opus编码/反向串流）和 1kHz 测试音开关。**手机调参实时同步到 PC 反向编码器**
- **1kHz 测试音模式**：内置正弦波发生器，无需麦克风即可调试端到端音频链路
- **JNI `@Synchronized` 互斥锁**：杜绝多线程并发闪退
- **网络无缝热重连**：改 IP 不重启录音流
- **UDP 广播自动发现**：统一 v2 协议发现局域网内 Windows 接收端
- **P2P 双向保活**：每 ~1s（50 帧）发送 TYPE_CONNECT（携带反向端口 + Opus 编码参数）+ 非阻塞 1ms 超时 drainAck 收 CONNECT_ACK
- **优雅断连重连**：断连不退出采集，持续发 CONNECT 保活等待重连，收到 ACK 自动恢复；3 秒无 ACK 标记断连
- **持久化设备 ID**：8 字节唯一标识，SharedPreferences 存储，连接鉴权用

![Android 现代版主界面](assets/android-modern-main.webp)

---

## 五、Android Old 低性能版（`android_old/`）— 已停止开发

> ⚠️ 该版本自 v1.1.1 起停止开发，v1.1.2 起 PC 端已删除全部低性能适配代码，与当前 PC 端**不互通**。以下特性描述对应其最终状态，源码仅归档。旧设备请成对使用 [Release v1.1.1](https://github.com/Raven777777/UDP2CAL/releases/tag/v1.1.1)。

专为 Android 6~8（API 21~26）低端设备/翻盖机定制的精简版本。详见 [ANDROID_OLD.md](ANDROID_OLD.md)。

- **48kHz 全频带编码**：正向 48kHz Opus 编码，FB 全频带，256kbps VBR；反向 48kHz 立体声最大码率（PC 高品质模式）
- **40ms 帧合并编码**：合并 2 帧打包编码，编码频率 50→25 帧/秒，CPU 占用从 120% 降至 ~14%
- **JNI 动态帧长**：`opus_jni.c` 硬编码 `state->frame_size` → 改用实际数组长度，支持 20ms/40ms/60ms 灵活帧，修复 40ms 帧降调问题
- **CPU 自动降级**：读取 `/proc/self/stat` 进程 CPU 时间，每 2s 检测；>40% 时渐进降级（FB→SWB→WB→MB，码率 256→128→64→32kbps），通过 `encoder.update()` 热切换，无需重启采集流
- **ACK 限频轮询**：每 10 帧检查一次 ACK，`soTimeout` 5ms，drainAck CPU 开销降为 1/50
- **传统 View 布局**：无 Jetpack Compose，minSdk=21，targetSdk=26
- **一键自动连接**：UDP 广播发现局域网 PC 端，自动连接无需手动输入
- **均衡配置**：Opus 复杂度 1、DTX 开启、VBR 256kbps、全频带 FB
- **十字键交互**：上下调音量，左右切换关于页，适配物理键盘翻盖机
- **音源回退**：优先 VOICE_COMMUNICATION（系统硬件降噪），失败回退 MIC 裸采集

> 测试设备：SHARP NP805SH（翻盖机）— Snapdragon 210 (MSM8909) / 4×Cortex-A7 @1.1GHz / 1GB RAM / Android 8.1。优化后 CPU ~14%，温度 37°C 稳定。

![Android Old 主界面](assets/android-old-main.webp)&nbsp;&nbsp;![Android Old 关于页](assets/android-old-about.webp)

![Android Old CPU + 温度优化前后对比](assets/android-old-cpu-power.webp)

*android_old 发热优化：CPU 120% → 14%，温度 35→37°C 稳定*

---

## 六、Windows 接收端

- **Rust + iced 原生 UI**：暗色主题，实时音量电平显示
- **反向串流按钮**：UI 按钮控制 PC→Phone 反向音频启停（开机自启右侧），设置持久化到注册表
- **5 独立 Opus 解码器**：支持所有采样率无缝切换
- **相位连续流式重采样**：EMA 自适应漂移补偿，±0.2% 区间防止变调
- **VB-Cable 自动检测**：无虚拟声卡时回退到默认输出设备
- **系统托盘集成**：关闭按钮隐藏到托盘，右键菜单退出，双击恢复窗口；UI 显示 VB-Cable 状态、P2P 连接状态（●已占用/●已连接/●空闲）、反向串流状态
- **P2P 独占通信系统**：
  - 双状态机：广播（Ready 周期广播/Silent 停止）+ 连接（Idle 监听/Busy 独占）
  - TYPE_CONNECT 唯一准入，READY 时拒绝所有音频
  - 设备 ID 1对1 过滤 + 并发互斥锁防抢占，重启 Win 即清空非法绑定
  - **防抢占静默拒绝**：DEVICE_BUSY 时异设备 TYPE_CONNECT → device_id 不匹配 → 不回复 CONNECT_ACK → 新设备 `drainAck()` 永 false → `p2pConnected` 永 false → 新设备不编不送音频（但持续发 CONNECT 等待，由 Win 端静默丢弃）；已有设备保活和音频流完全不受影响
- **双向保活**：CONNECT → CONNECT_ACK，1 秒保活周期；300ms 接收超时，1 秒无包标记断连
- **统一 v2 协议**：15B 包头携带设备 ID，所有包同格式
- **二进制发现服务**：监听 44043，v2 DISCOVER_REQ/REPLY
- **广播状态机**：READY 时每秒广播 TYPE_DISCOVER_REPLY 到 255.255.255.255:44043；BUSY 时自动静音
- **注册表持久化配置**：开机自启、监听地址、端口、设备 ID（8 字节 u64 存档）、反向串流开关
- **初始化状态检测**：Opus 解码器 / WASAPI 音频引擎初始化失败时实时反馈到 UI
- **Windows 防火墙自动放行**（静默 `NETSH`，无 CMD 窗口）
- **单实例互斥锁检测**：`CreateMutexW` 防止重复启动

![Windows 接收端 UI](assets/windows-main.webp)

---

## 七、Opus 编码器参数说明（现代版 `android/` 设置面板）

现代版 Android App 内置 Opus 高级编码设置面板，以下为各参数详解：

| 参数 | 默认值 | 可选值 | 说明 |
|------|--------|--------|------|
| **编码复杂度** | 10（默认最高品质） | 1~10 (滑动条) | 1=最快速(省CPU)，10=最佳质量(费CPU)。局域网推荐较高复杂度 |
| **信号类型** | 语音 | 语音 / 音乐 | 语音模式启用系统硬件降噪(NS+AGC)+AEC；音乐模式裸麦直出 |
| **音频带宽** | 全频带(FB) | NB 8k / MB 12k / WB 16k / SWB 24k / FB 48k | 限制编码频率范围，越低越省码率。语音推荐 WB 或 SWB |
| **VBR (可变码率)** | 关闭(CBR) | 开启(VBR) / 关闭(CBR) | VBR 在静音时自动降低码率节省带宽；开启 DTX 时强制 VBR |
| **VBR 码率约束** | 无约束 | 无约束 / 约束 | 约束时 VBR 不超过目标码率；无约束时允许临时突增 |
| **DTX (不连续传输)** | 关闭 | 开启 / 关闭 | 开启后静音段停止传输数据，大幅节省带宽。开启后强制 VBR |
| **FEC (前向纠错)** | 关闭(0)（低延迟默认） | 关闭(0) / 开启(2) | 开启后丢包容忍度提高，适合弱网环境。局域网推荐关闭以降低延迟 |
| **预期丢包率** | 0%（低延迟默认） | 0~30% (滑动条) | 告知编码器预期丢包率，FEC 开启时按此比例插入冗余数据。局域网设置 0% 可避免冗余编码 |
| **码率** | 自动分配（默认） | 自动(0) / 32~512 kbps (手动) | 自动模式下：48kHz→512k，24kHz→256k，16kHz→128k，≤12kHz→64k |
| **采样率** | 48000 Hz | 固定不可调 | 采集固定 48kHz，协议层支持动态切换(8k/12k/16k/24k/48k) |

> 所有参数在编码运行中可热修改，无需重启采集流（通过 `encoder.update()` 每帧检测变更毫秒级同步）。

![Android 现代版高级设置面板](assets/android-modern-settings.webp)
