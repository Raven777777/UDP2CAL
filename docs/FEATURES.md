# 功能特性

> 相关文档：[构建说明](BUILD.md) · [架构说明](ARCHITECTURE.md)

---

## 一、系统概述

```
Android 手机 ↔ 双向音频 ↔ Windows PC
  正向: 手机麦克风 → Opus 编码 → UDP → PC → WASAPI → VB-Cable → 任意应用
  反向: PC 扬声器 → WASAPI Loopback → Opus 编码 → UDP → 手机听筒/扬声器
```

UDP2CAL 是一个**双向、低延迟**的局域网音频串流系统。Android 客户端为 `android/`：面向 Jetpack Compose 设备（Android 10+），固定 48kHz 采集，支持全参数 Opus 调节 + 反向编码实时同步。

**全链路零堆分配**（ByteArray + ShortArray 双闭环，仅池热身期 3 次构造）、**生产-消费双协程**消除阻塞饥饿、**JNI 双重边界守卫**防越界写穿。

---

## 二、v1.1.0 版本要点

- 双向音频：手机麦克风→PC (VB-Cable) + PC 扬声器→手机听筒/扬声器
- 反向串流：WASAPI Loopback → Opus 编码 → UDP 发送（Windows UI 按钮控制启停）
- 声学回声消除：语音模式启用 AcousticEchoCanceler + MODE_IN_COMMUNICATION + 同 session 绑定
- 立体声/单声道自适应：现代手机立体声编码，低端设备单声道降级（CONNECT payload 模式标志，v1.1.2 已移除）
- v3 控制协议：二进制发现（携带 listen_port）、独占连接（CONNECT/CONNECT_ACK）、8 字节设备 ID 绑定
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
- **立体声编码**：反向串流固定 48 kHz 立体声、10 ms 帧、全频带，手机端显示带宽+码率
- **反向参数热同步**：手机端 Opus 面板调整后，PC 反向编码器每秒读取 CONNECT 载荷并热应用，无需重启串流

---

## 四、Android 发送端（`android/`）

- **音源自适应切换**：仅一个 `isVoiceMode` 状态变量驱动全部音源决策。语音模式→VOICE_COMMUNICATION（安卓系统原生硬件降噪+AEC），全频模式→MIC（裸麦直出，硬件降噪关闭）；异常回退 MIC 保底通路
- **声学回声消除**：语音模式启用 AcousticEchoCanceler + MODE_IN_COMMUNICATION AudioTrack + 同音频 session 绑定
- **生产-消费双协程**：独占线程 `AudioRecord.read()` + `Channel` 投递消费者，消除饥饿。信号类型变更时消费者循环检测→自动重建 AudioRecord，保留网络层和反向播放器
- **全链路零堆分配**：`ShortArrayPool` 帧复用 + 乒乓发送缓冲区 + `encoderEncodeTo` 直接写入
- **纯直通无软件处理**：AudioRecord 取 PCM → 直通拷贝 → Opus 编码输出，APP 端无任何降噪/AGC/音效代码，硬件降噪由安卓系统全权托管
- **Opus CBR/VBR 热调节**：码率自动(512k) / 512 / 768 / 960 / 1000 kbps；采样率与帧长固定 48 kHz / 10 ms（v3 不再协商）
- **Opus 高级编码设置面板**：Jetpack Compose 全参数面板（复杂度/信号类型/带宽/VBR/FEC/DTX/丢包率/码率），面板内集成运行时状态卡片（音源/Opus编码/反向串流）和 1kHz 测试音开关。**手机调参实时同步到 PC 反向编码器**
- **1kHz 测试音模式**：内置正弦波发生器，无需麦克风即可调试端到端音频链路
- **JNI `@Synchronized` 互斥锁**：杜绝多线程并发闪退
- **网络无缝热重连**：改 IP 不重启录音流
- **UDP 广播自动发现**：v3 控制包发现局域网内 Windows 接收端，端口取自 DISCOVER_REPLY
- **P2P 双向保活**：每 ~1s（100 帧 × 10ms）发送 TYPE_CONNECT（携带反向端口 + Opus 编码参数）；CONNECT_ACK 由独立守护线程接收（500ms `soTimeout`），不再占用音频流水线
- **优雅断连重连**：断连不退出采集，持续发 CONNECT 保活等待重连，收到 ACK 自动恢复；3 秒无 ACK 标记断连
- **持久化设备 ID**：8 字节唯一标识，SharedPreferences 存储，连接鉴权用

![Android 主界面](assets/android-modern-main.webp)

---

## 五、Windows 接收端

- **Rust + iced 原生 UI**：暗色主题，实时音量电平显示
- **反向串流按钮**：UI 按钮控制 PC→Phone 反向音频启停（开机自启右侧），设置持久化到注册表
- **5 独立 Opus 解码器**：支持所有采样率无缝切换
- **丢包恢复（事件驱动 PLC）**：ReorderBuffer 确认丢包时按确切帧数补偿，Deep PLC（神经网络）运行时激活；整流停滞（DTX/断流）由兜底计时器处理，2 秒后静音
- **相位连续流式重采样**：EMA 自适应漂移补偿，±0.2% 区间防止变调
- **反向链路诊断**：每 30s 输出发包数/字节数/编码错误/超限丢弃/丢样/发送错误；反向码率由 CONNECT 同步（0=自动→512 kbps，上限 1000）
- **VB-Cable 自动检测**：无虚拟声卡时回退到默认输出设备
- **系统托盘集成**：关闭按钮隐藏到托盘，右键菜单退出，双击恢复窗口；UI 显示 VB-Cable 状态、P2P 连接状态（●已占用/●已连接/●空闲）、反向串流状态
- **P2P 独占通信系统**：
  - 双状态机：广播（Ready 周期广播/Silent 停止）+ 连接（Idle 监听/Busy 独占）
  - TYPE_CONNECT 唯一准入，READY 时拒绝所有音频
  - 设备 ID 1对1 过滤 + 并发互斥锁防抢占，重启 Win 即清空非法绑定
  - **防抢占静默拒绝**：DEVICE_BUSY 时异设备 TYPE_CONNECT → device_id 不匹配 → 不回复 CONNECT_ACK → 新设备 `lastAckTime` 不刷新 → `p2pConnected` 永 false → 新设备不编不送音频（但持续发 CONNECT 等待，由 Win 端静默丢弃）；已有设备保活和音频流完全不受影响
- **双向保活**：CONNECT → CONNECT_ACK，1 秒保活周期；300ms 接收超时，1 秒无包标记断连
- **控制面 + 媒体面分离**：15B 控制头（`0x03`）与 12B RTP 头（PT=111）共用 socket，按首字节解复用
- **二进制发现服务**：监听 44043，v3 DISCOVER_REQ/REPLY（回复带 listen_port + 设备名）
- **广播状态机**：READY 时每秒广播 TYPE_DISCOVER_REPLY 到 255.255.255.255:44043；BUSY 时自动静音
- **注册表持久化配置**：开机自启、监听地址、端口、设备 ID（8 字节 u64 存档）、反向串流开关
- **初始化状态检测**：Opus 解码器 / WASAPI 音频引擎初始化失败时实时反馈到 UI
- **Windows 防火墙自动放行**（静默 `NETSH`，无 CMD 窗口）
- **单实例互斥锁检测**：`CreateMutexW` 防止重复启动

![Windows 接收端 UI](assets/windows-main.webp)

---

## 六、Opus 编码器参数说明（`android/` 设置面板）

Android App 内置 Opus 高级编码设置面板，以下为各参数详解：

| 参数 | 默认值 | 可选值 | 说明 |
|------|--------|--------|------|
| **编码复杂度** | 10（默认最高品质） | 1~10 (滑动条) | 1=最快速(省CPU)，10=最佳质量(费CPU)。局域网推荐较高复杂度 |
| **信号类型** | 语音 | 语音 / 音乐 | 语音模式启用系统硬件降噪(NS+AGC)+AEC；音乐模式裸麦直出 |
| **音频带宽** | 全频带(FB) | NB 8k / MB 12k / WB 16k / SWB 24k / FB 48k | 限制编码频率范围，越低越省码率。语音推荐 WB 或 SWB |
| **VBR (可变码率)** | 关闭(CBR) | 开启(VBR) / 关闭(CBR) | VBR 在静音时自动降低码率节省带宽；开启 DTX 或自动码率时强制 VBR |
| **VBR 码率约束** | 无约束 | 无约束 / 约束 | 约束时 VBR 不超过目标码率；无约束时允许临时突增。自动码率下强制无约束 |
| **DTX (不连续传输)** | 关闭 | 开启 / 关闭 | 开启后静音段停止传输数据，大幅节省带宽。开启后强制 VBR |
| **FEC (前向纠错)** | 关闭(0)（低延迟默认） | 关闭(0) / 开启(2) | 开启后丢包容忍度提高，适合弱网环境。局域网推荐关闭以降低延迟。开启时编码器附带 DRED 深度冗余（两端 1.6.1） |
| **预期丢包率** | 0%（低延迟默认） | 0~30% (滑动条) | 告知编码器预期丢包率，FEC 开启时按此比例插入冗余数据。局域网设置 0% 可避免冗余编码 |
| **码率** | 自动（512 kbps） | 自动(0) / 512 / 768 / 960 / 1000 kbps | 协议把上限钳制在 1000 kbps：1024 kbps 在 10 ms 帧下是 1280 字节，越过 Opus 1275 字节上限。实际码率仍受 Opus 自身限制（10 ms 立体声约 512 kbps 封顶） |
| **采样率** | 48000 Hz | 固定不可调 | 采集固定 48kHz，协议层支持动态切换(8k/12k/16k/24k/48k) |

> 所有参数在编码运行中可热修改，无需重启采集流（通过 `encoder.update()` 每帧检测变更毫秒级同步）。

![Android 高级设置面板](assets/android-modern-settings.webp)
