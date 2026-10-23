# 系统架构与实现细节

> 相关文档：[协议规范](PROTOCOL.md) · [构建说明](BUILD.md) · [功能特性](FEATURES.md)

---

## 一、项目结构

```
udp2cal/
├─ android/        # Android 发送端（Kotlin + JNI Opus，面向现代设备）
├─ android_old/    # Android 发送端低性能版（已停止开发、仅归档；v1.1.2 起 PC 端不再兼容）
│  └─ app/src/main/java/com/udp2cal/app/
│     ├─ service/CaptureService.kt  # 采集服务（48kHz、40ms帧合并、ACK限频、CPU自动降级）
│     ├─ AudioPlayer.kt             # 反向音频播放器（48kHz 单声道输出）
│     ├─ UdpSender.kt               # UDP 发送器（drainAck 5ms超时）
│     ├─ MainActivity.kt            # 一键连接 + 十字键交互
│     └─ native/Opus*.kt            # Opus 编码器/解码器 JNI 封装（立体声解码）
├─ windows/        # Windows 接收端（Rust + iced + cpal）
│  ├─ src/
│  │  ├─ main.rs       # 主循环与 iced UI、托盘、UDP 接收流、音频工作线程
│  │  ├─ audio.rs      # WASAPI 输出 + 流式重采样
│  │  ├─ capture.rs    # WASAPI Loopback + Opus 编码 + 反向 UDP 发送
│  │  ├─ config.rs     # 注册表配置读写
│  │  ├─ decoder.rs    # Opus 解码器封装
│  │  ├─ firewall.rs   # 防火墙规则
│  │  └─ protocol.rs   # 协议重新导出
│  └─ Cargo.toml
├─ protocol/       # UDP 协议编解码（Rust crate，唯一真源）
├─ docs/           # 项目文档（本目录）
├─ build.bat       # 统一构建入口（Windows / Android / ADB / 体检）
├─ scripts/        # 构建脚本实现（Python 3.8+）
│  ├─ build.py
│  └─ udp2cal/     # 环境探测、依赖补装、Gradle/Cargo 封装
├─ icon.png        # 图标源文件（构建期嵌入 EXE，不可移动）
├─ LICENSE
└─ README.md
```

---

## 二、技术栈

| 层级 | 技术选型 | 运行时更新策略（免重启） |
| --- | --- | --- |
| **UI 层（Android 现代版）** | Jetpack Compose + Material 3 | 采用 `Flow` 细粒度订阅与局部缓存变量；Opus 高级编码设置全参数面板（复杂度/信号/带宽/VBR/FEC/DTX/丢包率） |
| **UI 层（Android Old）** | 传统 View + AppCompat | 已归档；v1.1.2 起与当前 PC 端不互通 |
| **UI 层（Windows）** | Iced (Rust) 360×300 固定窗口 | 模块化深色卡片布局 + 动态 VU 色彩表（绿/橙/红）+ 实时显示 VB-Cable 检测状态 + P2P 占用状态 + 反向串流状态；`run_with(session_id)` 强制状态隔离 |
| **网络层** | Kotlin Coroutines + UDP Socket / Rust async | 动态比对 `Prefs`，静默热重连不断流。v2 二进制广播自动发现 |
| **音频采集（现代版）** | AudioRecord（isVoiceMode 驱动 VOICE_COMMUNICATION / MIC，固定 48kHz） | **生产-消费双协程** + `ShortArrayPool` 零分配帧复用（池容量 5）；Opus 信号类型变更时自动重建 AudioRecord，保留网络层 |
| **音频采集（Old 低性能版）** | AudioRecord（VOICE_COMMUNICATION 优先，48kHz FB 全频带） | 已归档；**生产-消费双协程** + **40ms 帧合并编码**（2 帧打包，JNI 动态帧长，25 帧/秒）；ACK 限频检查（每 10 帧）；CPU 自动降级（`/proc/self/stat` 每 2s 检测）|
| **核心算法** | —（无任何软件音频处理） | 仅维护 `isVoiceMode` 一个布尔状态；硬件降噪由安卓系统全权处理 |
| **编码层** | libopus (JNI) | `encoderEncodeTo` 直接写入 & 静态 1276B + 动态双重边界守卫 + `@Synchronized` 互斥锁；FEC=2（允许 CELT + FEC）突破 300kbps SILK 天花板 |
| **发送层** | UDP DatagramSocket | 双缓冲乒乓 + `send(offset,length)` 零拷贝，防脏数据；非阻塞 1ms 超时 drainAck（收 CONNECT_ACK）|
| **Windows 音频引擎** | cpal / WASAPI — 全局单例 Audio Worker 守护线程 | 仅在 `init_audio_worker()` 初始化一次，通过 `SyncChannel<AudioMessage>` 接收，UI 启停不影响底层音频线程 |

---

## 三、Windows 端依赖

### 构建依赖（`[build-dependencies]`）

| Crate | 用途 |
|-------|------|
| `winres` | 将 ICO 资源嵌入 EXE 文件 |
| `image` | 构建时将 `icon.png` 转为 256×256 ICO |

`build.rs` 流程：`icon.png` → Lanczos3 缩放 256×256 → `icon.ico` → `winres::WindowsResource` 嵌入 EXE PE 资源。

### 运行时依赖（`[dependencies]`）

| Crate | 版本 | 用途 |
|-------|------|------|
| `udp2cal-protocol` | local | 局域网 UDP 协议（包头编解码、重排序缓冲区） |
| `iced` | 0.14 | GUI 框架（tiny-skia 软件渲染，tokio 运行时） |
| `tokio` | 1.40 | 异步 UDP 收发、超时控制、事件流 |
| `cpal` | 0.15 | WASAPI 音频播放 |
| `audiopus` | 0.3.0-rc.0 | Opus 音频编解码 |
| `windows` | 0.58 | Win32 API（单实例互斥、窗口控制、COM/WASAPI） |
| `winreg` | 0.52 | 注册表配置持久化 |
| `tray-icon` | 0.24 | 系统托盘图标、右键菜单、双击恢复 |
| `image` | 0.24 | 运行时解码内嵌 `icon.png` 为 RGBA |

### 关键依赖配置

```toml
[profile.release]
opt-level = "s"       # 按体积优化
lto = true            # 链接时优化
codegen-units = 1     # 单代码生成单元
panic = "unwind"      # Panic 展开
strip = "symbols"     # 剥离调试符号

[dependencies]
iced = { version = "0.14", default-features = false, features = ["tokio", "tiny-skia", "image"] }
tokio = { version = "1.40", features = ["net", "sync", "rt", "macros", "time"] }
windows = { version = "0.58", features = [
    "Win32_Security", "Win32_System_Com", "Win32_System_LibraryLoader",
    "Win32_System_Threading", "Win32_System_Registry",
    "Win32_UI_WindowsAndMessaging", "Win32_UI_Shell",
    "Win32_UI_Input_KeyboardAndMouse", "Win32_Graphics_Gdi",
    "Win32_Foundation", "Win32_Media_Audio", "Win32_Media_KernelStreaming",
] }
```

> `iced` 必须禁用默认特性（避免 wgpu 后端引发 `windows` crate 版本冲突），显式启用 `tokio` + `tiny-skia` 渲染器。
>
> `tokio` 必须显式启用 `time` 和 `net` 特性，否则 `tokio::time::sleep` 和 `tokio::net::UdpSocket` 不可用。

---

## 四、自动协商与热同步机制

| 协商项 | 策略 |
| --- | --- |
| 采样率（现代版） | Android 固定 48kHz 采集；接收端 5 独立 Opus 解码器自动匹配（8k/12k/16k/24k/48k） |
| 反向串流编码 | PC 固定 48kHz 立体声全频带，码率与复杂度由手机端 CONNECT 携带的 Opus 参数同步 |
| 自动码率安全防线 | Prefs 码率 0 时根据采样率动态分配默认值（48kHz→512k，24kHz→256k，16kHz→128k，≤12kHz→64k）；Rust `compute_bitrate_id()` / `default_bitrate_for_sr()` 与 Kotlin 100% 对齐 |
| 传输机制 | 15 字节包头（Big-Endian）实时携带码率、采样率、8 字节设备 ID；单一版本，无兼容分支 |
| 接收端解析 | `resolve_bitrate()` + 5 独立 Opus 解码器 + AudioWriter 相位连续重采样 |
| **局域网自动发现** | Windows 监听 44043，v2 二进制协议。Android 端 IP 框右侧图标一键搜索 |
| **心跳与保活** | Android 每 ~1s（50 帧）发 CONNECT（携带反向端口 + Opus 编码参数同步到 PC 反向端）；Win 端 300ms 接收超时 + 1s 无包标记断连；Android 3s 无 ACK 标记断连 |
| **反向串流保活** | 保活 CONNECT 携带反向端口，Win 重启后自动恢复反向串流。仅端口真正变化时重启发送器，防止每秒卡顿 |
| **模式切换** | 语音↔音乐模式切换时保留 UdpSender(网络层) 和 reverseDecoder，重建 AudioPlayer(路由/声道)，零网络中断 |

---

## 五、运行时行为

| 操作 | 行为 |
|------|------|
| 点击关闭按钮 (X) | 隐藏窗口到系统托盘，任务栏消失 |
| 点击最小化按钮 (—) | 最小化到任务栏 |
| 左键单击托盘图标 | 无操作（仅右键显示菜单） |
| 双击托盘图标 | 恢复并置顶主窗口 |
| 托盘右键 → 退出 | 退出程序 |

### UI 状态指示

| 指示器 | 含义 |
|--------|------|
| `● 等待连接 / ● 已断开` | 红色 — 正在监听但无数据流 |
| `● 已连接` | 绿色 — 收到音频数据 |
| `● 已占用 / ● 已占用 {device_id}` | 橙色 — TYPE_CONNECT 独占绑定，异设备 CONNECT 将被静默拒绝 |
| `● 空闲` | 绿色 — 已启动但无可信连接 |
| `● VB-Cable` / `● VB-Cable 未安装` | 绿色/橙色 — 虚拟声卡检测状态 |
| `● 广播中` / `● 已静音` | 绿色/灰色 — 广播状态机 Ready/Silent |
| `[停止] [开机自启] [反向串流]` | 反向串流按钮 — 停止时可用点击切换，运行时锁定。开启后显示 `● 反向串流已启用` |

### 配置持久化

- 配置存储在注册表 `HKCU\Software\UDP2CAL`
- 监听 IP、端口在点击「启动」时保存；设备 ID 首次运行时自动生成（8 字节 u64）
- 开机自启开关保存到 `HKCU\...\Run` 键 + 配置键
- 反向串流开关在停止时切换并保存到注册表，Win 重启后自动恢复
- 每次启动均从停止状态开始（`is_running` 为运行时状态，不持久化）

### 局域网自动发现

- Windows 端常驻监听 UDP 端口 44043（独立线程 `start_broadcast_listener`）
- **v2 二进制协议**：手机端 `TYPE_DISCOVER_REQ` → PC 回复 `TYPE_DISCOVER_REPLY`（含端口 + 设备名）
- **广播状态机**（`start_broadcast_state_machine`）：READY 时每秒向 `255.255.255.255:44043` 广播 `TYPE_DISCOVER_REPLY`；BUSY 时自动静音
- 端口随用户修改实时更新（每次发现请求从注册表读取最新值）

### 图标

- `icon.png` 通过 `include_bytes!` 编译期嵌入 EXE
- 同时用作窗口图标和托盘图标
- `build.rs` 另将 `icon.png` 转为 ICO 嵌入 EXE 文件资源（资源管理器图标）
- 若 `icon.png` 缺失或无法解码，回退到程序绘制的 32×32 绿色圆点

### P2P 防抢占拒绝行为

当 Windows 端处于 `DEVICE_BUSY`（已绑定一台 Android）时，第三方设备手动输入 IP:端口发送 `TYPE_CONNECT`：

| 角色 | 行为 |
|------|------|
| **Windows 端** | `device_id` 不匹配 → 不回复 `CONNECT_ACK`，静默丢弃；全局状态 `GLOBAL_DEVICE_STATE` 和 `BOUND_DEVICE_ID` **完全不变** |
| **第三方 Android 端** | `drainAck()` 1ms 超时永不返回 true → `p2pConnected` 永为 false → **不编码不发送任何音频数据**；持续发 CONNECT 等待（由 Win 端静默丢弃，不影响已有连接） |
| **已有 Android 端** | 正常收到 `CONNECT_ACK`（device_id 匹配保活分支），音频流正常解码播放，**完全不受影响** |

> 唯一例外：极低概率的 8 字节 device_id 碰撞会导致 Windows 误判为同设备保活，回复 `CONNECT_ACK` 到第三方。但由于第三方无合法音频采集流，仍无法发送音频数据。

### 释放独占的三种途径

1. **Windows 端点击「停止」** → 调用 `reset_to_ready()` 清空 `BOUND_DEVICE_ID` + 恢复 `DEVICE_READY`
2. **已有 Android 端 1 秒无包超时** → `reset_to_ready()` 自动释放
3. **重启 Windows 程序** → 全局状态初始化回到 `DEVICE_READY`

---

## 六、Windows 端架构精要

### 数据流

```
[Android 手机] ──Opus 编码 UDP──→ [Windows PC]
    ↓
UDP 接收协程 → SyncChannel → 常驻 Audio Worker 线程（解码 + WASAPI 播放）
    ↓
VB-Cable / 扬声器 → 微信 / Zoom / OBS / 游戏
                                     ↑
[Windows PC] ──反向 WASAPI Loopback──┘
    ↓
WASAPI 捕获 → Opus 编码 → UDP → [Android 手机]
```

### 全局单例音频守护线程

音频引擎（`audio`）和解码器（`decoder`）在整个软件生命周期内只初始化一次。无论 UI 如何点击「启动/停止」，底层音频守护线程不受影响。

```
      程序启动
         │
    ┌────┴────┐
    │ Audio   │ (常驻线程，不解构)
    │ Worker  │ ← SyncChannel<AudioMessage>
    └─────────┘
         │
    ┌────┴────┐
    │ Iced UI │ ← 仅负责网络收发和数据投递
    │ Network │
    └─────────┘
```

- **常驻后台线程**：`init_audio_worker()` 在 `main()` 中调用一次，通过 `SyncChannel<AudioMessage>` 接收数据
- **`Reset` 消息**替代完全销毁重建：收到后仅清空重排序缓冲区和 RMS 统计，不解构任何音频资源
- **`session_id` 计数器**：每次启动通过自增 `session_id` 强制 Iced 视为全新订阅，避免状态残留

### UDP 接收流

`udp_receiver_stream` 通过参数传递 IP 和端口，不直接读取注册表，消除跨线程竞态：

```rust
fn subscription(&self) -> Subscription<Message> {
    let ip = self.config.listen_ip.clone();
    let port = self.config.listen_port;
    Subscription::run_with((ip, port), |data| udp_receiver_stream(data.0.clone(), data.1))
}
```

### EMA 漂移补偿防线

`audio.rs` 中使用三分量 EMA 滤波器进行漂移补偿，更新前多重校验，并结合缓冲区水位快速修正：

```rust
// ── 每 3 秒：EMA 更新漂移比率 ──
if now.duration_since(self.last_ratio_update) > std::time::Duration::from_secs(3) {
    let current_fill = self.buf.lock().map(|b| b.len()).unwrap_or(0);
    let dt = now.duration_since(self.last_fill_time).as_secs_f64();
    if dt > 2.0 && self.last_fill_sample > 0 {
        let consume_rate = (self.last_fill_sample - current_fill) as f64 / dt;
        let produce_rate = self.device_rate as f64 - consume_rate;
        if produce_rate > 1000.0 && produce_rate.is_normal() {
            let measured = self.device_rate as f64 / produce_rate;
            if measured.is_finite() && measured > 0.5 && measured < 1.5 {
                self.drift_ratio = self.drift_ratio * 0.7 + measured * 0.3;
            }
        }
    }
    // 缓冲区严重偏离时极小步长快速修正
    let fill_pct = current_fill as f64 / self.target_fill as f64;
    if fill_pct < 0.5 {
        self.drift_ratio += 0.0005;
    } else if fill_pct > 1.5 {
        self.drift_ratio -= 0.0005;
    }
    self.drift_ratio = self.drift_ratio.clamp(0.998, 1.002); // ±0.2%
}
self.effective_ratio = (source_rate as f64 / self.device_rate as f64) / self.drift_ratio;
```

- `is_normal()`：排除零、次正则、Infinity、NaN；`is_finite()`：二次确认
- `0.5 ~ 1.5` 范围限制：防止极端值大幅污染 EMA
- **缓冲区水位修正**：低于 50% 或高于 150% 时以 `±0.0005` 步长微调，防止缓冲耗尽/膨胀
- **±0.2% 硬限幅**：`clamp(0.998, 1.002)` 确保漂移补偿不引起人耳可感知的变调
- **复用重采样缓冲区**：预分配 8192 f32（32KB），避免每帧 4KB 堆分配

### 快速双击防抖

`ToggleRunning` 入口检查 200ms 防抖，避免旧 socket 未完全释放时新 socket 绑定失败。同时输入框 `PortChanged` 实时校验 `parse::<u16>()`，非法输入时边框变红。

```rust
if self.last_toggle_instant.elapsed() < Duration::from_millis(200) {
    return Task::none();
}
```

---

## 七、命名约定

| 项目 | 值 |
| --- | --- |
| 项目/EXE/APK | `UDP2CAL` / `udp2cal.exe` / `udp2cal.apk` |
| Rust crate | `udp2cal` / `udp2cal-protocol` |
| 协议 lib 名 | `udp2cal_protocol` |
| 注册表 | `HKCU\Software\UDP2CAL` |
| 开机自启 Run 键 | `UDP2CAL` |
| 防火墙规则 | `UDP2CAL 局域网音频串流` |
| 单实例互斥锁 | `UDP2CAL_SingleInstance_Mutex` |
| 发现服务端口 | `44043` |
| Android 包名 | `com.udp2cal.app` |

---

## 八、相关文档

| 文档 | 内容 |
| --- | --- |
| [PROTOCOL.md](PROTOCOL.md) | UDP v2 协议规范 |
| [BUILD.md](BUILD.md) | 构建流程与产物 |
| [FEATURES.md](FEATURES.md) | 功能特性与 Opus 参数 |
| [CHANGELOG.md](CHANGELOG.md) | 版本历史 |
| [BUGFIX.md](BUGFIX.md) | Bug 根因与修复记录 |
