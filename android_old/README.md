# UDP2CAL (Android Old) — 已停止开发

> ⚠️ **本目录已停止开发**（自 v1.1.1 起），不再接受功能更新、兼容性修复与构建支持，仅作归档保留。
>
> **v1.1.2 起，`windows/` 与 `protocol/` 已彻底删除低性能适配代码，本目录产物无法连接 v1.1.2 及以后的 PC 端。**
>
> 需要可用版本请成对使用 **[Release v1.1.1](https://github.com/Raven777777/UDP2CAL/releases/tag/v1.1.1)** 的 Windows 端与已签名 APK。

适用于 Android 6~8（API 21~26）旧设备 / 翻盖机的 UDP2CAL 客户端，支持双向音频串流。传统 View 布局，48kHz 全频带 + 40ms 帧合并编码 + CPU 自动降级，实测 CPU ~14%（Sharp NP805SH / 骁龙210 / 1GB RAM）。

**完整说明已迁移至 [`../docs/ANDROID_OLD.md`](../docs/ANDROID_OLD.md)** —— 定位对比、性能实测数据、关键优化项、延迟指标、技术要点、构建方式与已知限制。

## 构建（归档，不再维护）

```bat
gradlew assembleRelease -PtargetAbi=armeabi-v7a   :: 32位
gradlew assembleRelease -PtargetAbi=arm64-v8a     :: 64位
```

APK 路径：`app/build/outputs/apk/release/app-release.apk`

> 上述命令不保证在当前代码基线上仍可成功执行。**推荐做法：直接从 [Release v1.1.1](https://github.com/Raven777777/UDP2CAL/releases/tag/v1.1.1) 下载预编译 APK，无需自行构建。**
