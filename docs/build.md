# UDP2Mic 项目文档
> 最后更新: 2026-06-04 | 状态: **v1.0.4 — 并发安全、免重启热更新、无缝热重连**

> 项目简介与快速开始见根目录: [README.md](../README.md)

## 项目概述

UDP2Mic 是一个局域网麦克风系统：


```
Android 手机 → [AGC (动态平滑)] → [噪声门] → Opus (CBR/VBR 热调节) → UDP (热重连) → Windows PC → WASAPI → VB-Cable
```

**双端均已编译通过。RNN/TFLite 降噪已彻底移除，当前管线为 纯动态 AGC + 噪声门。**

---

## 一键构建

### Windows

```powershell
build_windows.bat
# 产物: udp2mic.exe (项目根目录)
```

**前提**: Rust 1.96+, VS Build Tools 2022 (C++桌面开发), CMake 3.22+

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
| **网络层** | Kotlin Coroutines + UDP Socket | 动态比对 `Prefs`。网络配置改变时，在处理帧隙**静默关闭并无缝重连**，不断音频流 |
| **音频采集** | AudioRecord (MediaRecorder.AudioSource.UNPROCESSED) | 硬件流常驻，除非切换测试音算法，否则在 App 生命周期内绝不物理销毁重建 |
| **核心算法** | 智能解耦 AGC + 动态追踪噪声门 | 攻击/释放非对称平滑，自动上限锁定 100x。关闭转开启时边缘触发重置为 10x |
| **编码层** | libopus (JNI 绑定) | **JNI 线程同步互斥锁保护**。参数指纹 Hash 变更时执行毫秒级原地 `update` |

---

## 自动协商与热同步机制 (v1.0.4)
| 协商项 | 策略 |
| --- | --- |
| 采样率 | 固定使用 48kHz（按 48k→24k→16k→8k 优先级硬匹配） |
| 自动码率安全防线 | `Prefs` 码率为 0 时激活。根据采样率动态分配合理默认值（48kHz 对应 64kbps），防止向 JNI 传递 0kbps 导致死锁 |
| 传输机制 | 码率和采样率通过 6 字节包头实时打包传给接收端，无需带外协商 |
| 接收端解析 | `protocol::resolve_bitrate()` 自动解析; 5个独立 Opus 解码器; AudioWriter 自动重采样 |

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
| Android 包名 | `com.udp2mic.app` |
| SharedPreferences | `udp2mic_prefs` |
| 通知频道 | `udp2mic_capture` |