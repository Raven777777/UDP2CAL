# UDP2Mic 项目文档
> 最后更新: 2026-06-05 | 状态: **v1.0.6 — Windows 端边缘场景容错强化、竞态消除、NaN 防线**

> 项目简介与快速开始见根目录: [README.md](../README.md)

## 项目概述

UDP2Mic 是一个局域网麦克风系统：

```
Android 手机 → [AGC (样点级插值平滑)] → [噪声门] → Opus (CBR/VBR 热调节) → UDP (热重连 + 广播自动发现) → Windows PC → WASAPI → VB-Cable
```

**Pipeline 已实现 ByteArray + ShortArray 完全零堆分配（仅池热身期 3 次构造）。双端均已编译通过。**

---

## 一键构建

### Windows

```powershell
build_windows.bat
# 产物: udp2mic.exe (项目根目录)
```

**前提**: Rust 1.96+, VS Build Tools 2022 (C++桌面开发), CMake 3.22+

> 若使用 CMake ≥ 4.0，编译前需设置环境变量 `CMAKE_POLICY_VERSION_MINIMUM=3.5`，否则 `audiopus_sys` 构建脚本会因 CMake 版本兼容性检查失败。

### Android

```batch
build_android.bat
# 产物: udp2mic-release.apk (项目根目录，已签名)
```

**前提**: JDK 17 (Temurin), Android SDK 35, NDK 27.0.12077973

> 构建脚本会自动将产物复制到项目根目录。

---

## 编译产物
| 文件 | 大小 | 说明 |
| --- | --- | --- |
| `udp2mic.exe` | ~4.8 MB | Windows 接收端 |
| `udp2mic-release.apk` | ~2.4 MB | Android 发送端 (已签名) |

---

## 技术栈与高阶机制
| 层级 | 技术选型 | 运行时更新策略（免重启） |
| --- | --- | --- |
| **UI 层** | Jetpack Compose | 采用 `Flow` 细粒度订阅与局部缓存变量，杜绝高频重绘引发的滑块卡顿 |
| **网络层** | Kotlin Coroutines + UDP Socket | 动态比对 `Prefs`，静默热重连不断流。支持 UDP 广播自动发现 |
| **音频采集** | AudioRecord (UNPROCESSED) | **生产-消费双协程** + `ShortArrayPool` 帧复用，零分配 |
| **核心算法** | 智能解耦 AGC + 动态追踪噪声门 | **样点级线性插值平滑**消除帧边界爆音，自动上限 100x |
| **编码层** | libopus (JNI) | **`encoderEncodeTo` 直接写入 & 双重边界守卫** + `@Synchronized` 互斥锁 |
| **发送层** | UDP DatagramSocket | **乒乓缓冲区 + `send(offset,length)` 零拷贝**，防脏数据 |

---

## 自动协商与热同步机制 (v1.0.5)
| 协商项 | 策略 |
| --- | --- |
| 采样率 | 固定 48kHz（48k→24k→16k→8k 优先级硬匹配） |
| 自动码率安全防线 | Prefs 码率 0 时根据采样率动态分配默认值 |
| 传输机制 | 6 字节包头（Big-Endian）实时携带码率/采样率 |
| 接收端解析 | `resolve_bitrate()` + 5 独立 Opus 解码器 + AudioWriter 重采样 |
| **局域网自动发现** | Windows 监听 44043，回复 `"UDP2MIC_REPLY:{port}"`。Android 端 IP 框右侧图标一键搜索 |

---

## 命名约定
| 位置 | 值 |
| --- | --- |
| 项目/EXE/APK | `UDP2Mic` / `udp2mic.exe` |
| Rust crate | `udp2mic` / `udp2mic-protocol` |
| 注册表 | `HKCU\Software\UDP2Mic` |
| 开机自启 Run 键 | `UDP2Mic` |
| 防火墙规则 | `UDP2Mic 局域网麦克风` |
| 单实例互斥锁 | `UDP2Mic_SingleInstance_Mutex` |
| 线程名 | `udp2mic-audio`, `udp2mic-fw` |
| 发现服务端口 | `44043` |
| Android 包名 | `com.udp2mic.app` |
| SharedPreferences | `udp2mic_prefs` |
| 通知频道 | `udp2mic_capture` |
