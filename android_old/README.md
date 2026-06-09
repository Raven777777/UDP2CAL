# UDP2CAL (Android Old)

适用于 Android 6~8（API 21~26）旧设备的 UDP2CAL 客户端。支持**双向音频串流**。

## 功能

- 自动搜索局域网内的 PC 接收端（UDP 广播发现）
- 一键自动连接，无需手动输入 IP/端口
- 音源优先尝试 VOICE_COMMUNICATION（系统硬件降噪），失败自动回退 MIC 裸采集
- **低延迟优化**：AudioTrack 缓冲 300ms→80ms，AudioRecord 缓冲减半，保活/CPU 检测低频化
- 低性能设备默认配置（Opus 复杂度 1、DTX 开启、VBR 自动码率）
- **双向音频串流** — 手机麦克风 → PC VB-Cable，PC 扬声器 → 手机听筒
- **独立音频路径** — 正向/反向使用独立 UDP 端口 + 独立协程，互不阻塞
- **自动反向端口协商** — CONNECT 包携带反向端口，Windows 端自动发现
- 十字键上下音量控制，适配翻盖机
- 左右键显示程序介绍页面

## 延迟优化（2026-06-09）

| 优化项 | 优化前 | 优化后 |
|:------|:------:|:------:|
| AudioTrack 缓冲 | 300ms + minBufSize×2 ≈ 600ms | **80ms** + minBufSize ≈ 120ms |
| AudioRecord 缓冲 | minBufSize×2 ≈ 80ms | **minBufSize** ≈ 40ms |
| 正向延迟 | ~600ms | **~120ms** |
| 反向延迟 | ~600ms | **~150ms** |

## 构建

```bat
:: armeabi-v7a（32位，推荐旧设备）
gradlew assembleRelease -PtargetAbi=armeabi-v7a

:: arm64-v8a（64位）
gradlew assembleRelease -PtargetAbi=arm64-v8a
```

APK 路径：`app/build/outputs/apk/release/app-release.apk`

## 技术要点

- 传统 Android View 布局（无 Jetpack Compose）
- minSdk=21, targetSdk=26, compileSdk=34
- Kotlin + AndroidX AppCompat
- Opus 编码器/解码器通过 JNI（C/NDK）编译
- 正向/反向双向 Opus 编解码（独立 socket + 独立协程）
- AudioTrack 听筒播放（USAGE_MEDIA，不改 AudioManager 模式）
