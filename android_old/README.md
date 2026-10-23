# UDP2CAL (Android Old)

适用于 Android 6~8（API 21~26）旧设备 / 翻盖机的 UDP2CAL 客户端，支持双向音频串流。传统 View 布局，48kHz 全频带 + 40ms 帧合并编码 + CPU 自动降级，实测 CPU ~14%（Sharp NP805SH / 骁龙210 / 1GB RAM）。

**完整说明已迁移至 [`../docs/ANDROID_OLD.md`](../docs/ANDROID_OLD.md)** —— 定位对比、性能实测数据、关键优化项、延迟指标、技术要点、构建方式与已知限制。

## 快速构建

```bat
gradlew assembleRelease -PtargetAbi=armeabi-v7a   :: 32位，推荐旧设备
gradlew assembleRelease -PtargetAbi=arm64-v8a     :: 64位
```

APK 路径：`app/build/outputs/apk/release/app-release.apk`

推荐使用根目录统一入口：`build.bat android --old --abi armeabi-v7a --release`
