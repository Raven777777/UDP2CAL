# 许可证与合规

> 相关文档：[文档索引](README.md)

---

## 一、版权与授权

**© 2026 UDP2CAL Contributors.**

本程序为自由软件，以 **GNU General Public License v3.0** 发布。

- 完整许可证文本：[`LICENSE`](../LICENSE)
- 你可以在 GPL-3.0 条款下自由复制、分发和修改本程序
- 分发衍生作品时须保留同样的授权条款并提供源代码

> *Copyright (C) 2026 UDP2CAL Contributors*
>
> *This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, version 3.*

---

## 二、第三方开源组件

本程序使用了以下开源软件（按功能分类）：

| 库/项目 | 许可证 | 用途 |
|---------|--------|------|
| [**libopus**](https://opus-codec.org/) (1.6.1) | BSD-3-Clause | Opus 音频编解码核心（仓库根 `opus-src/` 共享源码树） |
| [**iced**](https://iced.rs/) (0.14) | MIT | Windows 端 GUI 框架 |
| [**tokio**](https://tokio.rs/) (1.40) | MIT | 异步 UDP 收发与超时控制 |
| [**cpal**](https://github.com/RustAudio/cpal) (0.15) | MIT/Apache-2.0 | WASAPI 音频播放 |
| [**audiopus**](https://github.com/djg/audiopus-rs) (0.3.0-rc.0) | ISC | Rust Opus 编解码封装（`vendor/audiopus` 副本含本地扩展，见其 README） |
| [**audiopus_sys**](https://github.com/serenity-rs/audiopus_sys) (0.2.2) | ISC | Opus 构建与 FFI 绑定（`vendor/audiopus_sys` 副本改读共享 `opus-src/`，见其 README） |
| [**windows-rs**](https://github.com/microsoft/windows-rs) (0.58) | MIT | Windows Win32/COM API |
| [**Jetpack Compose**](https://developer.android.com/jetpack/compose) + **Material 3** | Apache-2.0 | Android 端现代 UI |
| [**Kotlin Coroutines**](https://kotlinlang.org/docs/coroutines-overview.html) | Apache-2.0 | Android 异步编程 |

---

## 三、授权兼容性说明

本程序以 **GPL-3.0** 整体发布，上述库作为独立作品各自受其原始许可证保护，以目标代码形式动态/静态链接。

| 上游许可证 | 与 GPL-3.0 兼容性 | 说明 |
|-----------|------------------|------|
| BSD-3-Clause | 兼容 | libopus，可并入 GPL-3.0 作品 |
| MIT | 兼容 | 保留原始版权与许可声明即可 |
| ISC | 兼容 | audiopus / audiopus_sys（功能上等同 MIT 简化版），保留声明即可 |
| Apache-2.0 | 兼容（GPL-3.0） | 需保留 NOTICE 与专利条款；不与 GPL-2.0 兼容 |

> 分发二进制产物时，须一并提供本项目的 GPL-3.0 授权文本与对应源代码获取方式。

---

## 四、合规要点（分发前自检）

1. 二进制包内附 `LICENSE`（GPL-3.0 全文）
2. Android 关于页显示 GPL v3 授权文字，不得出现「未经授权 禁止转载 修改 二次发布」等与 GPL 冲突的表述
3. 保留第三方组件的版权声明与许可证文本
4. 提供获取对应源代码的明确途径
