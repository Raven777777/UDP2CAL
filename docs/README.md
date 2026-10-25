# UDP2CAL 文档中心

> 当前版本 **v1.2.0**（协议 v3）· 本目录是项目技术文档的唯一入口。
>
> UDP2CAL 是双向、低延迟的局域网音频串流系统：Android 手机与 Windows PC 之间
> 通过**私有控制面 + 标准 RTP/Opus 媒体面**（协议 v3）互传音频流。
>
> 代码是实现真源，文档只解释**为什么这样做**和**怎么用**；每类知识只有一份文档负责。

---

## 一、按角色找入口

| 我是… | 先读 | 再读 |
| --- | --- | --- |
| 只想用起来 | 根目录 [README](../README.md)「快速开始」 | [FEATURES.md](FEATURES.md) |
| 要自己编译 | [BUILD.md](BUILD.md) | [ARCHITECTURE.md](ARCHITECTURE.md) |
| 要改协议 / 加一端 | [PROTOCOL_V3_RTP.md](PROTOCOL_V3_RTP.md) | [ARCHITECTURE.md](ARCHITECTURE.md) → [BUGFIX.md](BUGFIX.md) |
| 要查延迟 / 爆音 | [LATENCY_CALIBRATION.md](LATENCY_CALIBRATION.md) | [BUGFIX.md](BUGFIX.md) |
| 要发布 / 合规 | [BUILD.md](BUILD.md) §五 · [LEGAL.md](LEGAL.md) | [CHANGELOG.md](CHANGELOG.md) |

---

## 二、文档索引

| 文档 | 一句话 | 读者 |
| --- | --- | --- |
| [PROTOCOL_V3_RTP.md](PROTOCOL_V3_RTP.md) | **现行协议规范**：RTP 媒体面、控制面、抖动缓冲与播放头时钟、常量与验收 | 协议实现者 |
| [ARCHITECTURE.md](ARCHITECTURE.md) | 项目结构、技术栈、运行时行为、**代码落在哪**、命名约定 | 维护者、二次开发者 |
| [FEATURES.md](FEATURES.md) | 两端功能清单、Opus 参数说明、界面截图 | 使用者、调参用户 |
| [BUILD.md](BUILD.md) | 环境要求、`build.bat` 全部命令、产物清单、构建排障 | 首次构建者、发布者 |
| [LATENCY_CALIBRATION.md](LATENCY_CALIBRATION.md) | 延迟的三个口径、`playhead_ts` 与 DMA 缓冲、一次实测校准的流程 | 排障者、调优 |
| [CHANGELOG.md](CHANGELOG.md) | 版本历史与重大变更（按版本倒序） | 全部 |
| [BUGFIX.md](BUGFIX.md) | **现行代码**的 bug 根因档案 | 全部 |
| [LEGAL.md](LEGAL.md) | 版权、GPL-3.0、第三方组件许可 | 分发者、合规审查 |
| [archive/](archive/README.md) | 历史存档：v2 协议、迁移计划、旧修复记录、历次审查报告 | 追溯 |

---

## 三、单一真源

改动前先确认该写在哪儿；**不要在第二处复制一份**——两处各写一份的结果是慢慢跑偏
（历史上协议第七章和架构文档就各存了一份时钟模型，改一处忘一处）。

| 主题 | 唯一真源 | 其他文档的正确做法 |
| --- | --- | --- |
| 线格式、字段语义、状态机、参数值 | `PROTOCOL_V3_RTP.md` | 写"见协议 §x.y"，不复制表格 |
| 代码在哪个文件/线程/函数 | `ARCHITECTURE.md` | 引用，不复述实现细节 |
| 用户能感知的功能与调参项 | `FEATURES.md` | — |
| 某版本改了什么 | `CHANGELOG.md` | 一句话概括 + 链接 |
| 某个 bug 为什么发生 | `BUGFIX.md` | 一句话概括 + 链接 |
| 构建命令与产物 | `BUILD.md` | — |

> 判定标准：**读者会照着它写代码吗？** 会 → 协议规范；只想知道"为什么"或"在哪里" → 其他文档。

---

## 四、目录结构

```
docs/
├─ README.md                  # 本文件（索引 + 单一真源约定）
├─ PROTOCOL_V3_RTP.md         # 现行协议规范
├─ ARCHITECTURE.md            # 架构与实现映射
├─ FEATURES.md                # 功能与参数
├─ BUILD.md                   # 构建与产物
├─ LATENCY_CALIBRATION.md     # 延迟口径与实测校准
├─ CHANGELOG.md               # 版本历史
├─ BUGFIX.md                  # 现行代码的根因档案
├─ LEGAL.md                   # 许可证与合规
├─ archive/                   # 历史存档（不维护，不作为规范引用；见 archive/README.md）
│  ├─ PROTOCOL_V2.md                   # v2 协议（与 v3 不互通）
│  ├─ PLAN_V3_MIGRATION.md             # v2→v3 迁移计划与逐文件清单（已完成）
│  ├─ BUGFIX_V2_ERA.md                 # v3 重写之前的 bug 记录
│  ├─ CODE_REVIEW_2026-10-23.md        # 第一轮审查（43 项）
│  ├─ CODE_REVIEW_ROUND2_2026-10-23.md # 第二轮审查（24 项）
│  └─ CODE_REVIEW_2026-10-25.md        # 第三轮审查（22 项）
└─ assets/                    # 界面截图（webp）
```

---

## 五、维护约定

- **历史材料一律进 `archive/`**，并在文件顶部写明"归档原因 + 现行真源在哪"；
- **改动协议行为**要同步：`PROTOCOL_V3_RTP.md`（规范）、`ARCHITECTURE.md`（若涉及实现位置）、
  `CHANGELOG.md`（变更条目）、两端代码与测试；
- **不贴大段代码**：写"函数名 + 为什么"，代码会变，文档不会跟着变。
