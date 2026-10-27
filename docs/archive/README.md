# 历史存档

> 这里的文件**不再维护，也不作为规范引用**。保留只为追溯。
> 现行文档见 [../README.md](../README.md)。

| 文件 | 是什么 | 为什么在这里 |
| --- | --- | --- |
| [CONTROL_PLANE_REMEDIATION.md](CONTROL_PLANE_REMEDIATION.md) | 私有控制面 / 网络架构审查快照、首批整改记录与剩余风险 | 首轮整改已归档；v4 认证、媒体保护和实机验证未完成；恢复工作时需按当前代码重新审查 |
| [PROPOSAL_LATENCY_V1.4.md](PROPOSAL_LATENCY_V1.4.md) | 延迟优化提案（关扫描 + DSCP EF + 目标水位 150 + 阶跃吸收） | 已于 v1.4.0 落地，现行真源是协议 §7.10 与 [LATENCY_CALIBRATION.md](../LATENCY_CALIBRATION.md) |
| [PROTOCOL_V2.md](PROTOCOL_V2.md) | v2 私有协议（20 ms 帧、自定义音频头） | v3 干净断代，与现行协议**不可互通** |
| [PLAN_V3_MIGRATION.md](PLAN_V3_MIGRATION.md) | v2→v3 的落地顺序与逐文件改动清单 | 迁移已于 v1.2.0 完成 |
| [PLAN_RECEIVE_CLOCK_2026-10-27.md](PLAN_RECEIVE_CLOCK_2026-10-27.md) | 接收端时钟/播放控制审查与整改计划（三时钟、慢 ASRC、持续裁剪、PLC 期限、DRED 取舍） | 整改已全部落地；现行真源是协议 §7.9–§7.10 与 [LATENCY_CALIBRATION.md](../LATENCY_CALIBRATION.md) |
| [PLAN_ASRC_REFACTOR.md](PLAN_ASRC_REFACTOR.md) | 音频同步向纯 ASRC 收敛的五阶段计划（含实机微调结果） | P0–P5 已于 v1.3.0 完成，现行真源是协议 §7.9–§7.10 |
| [BUGFIX_V2_ERA.md](BUGFIX_V2_ERA.md) | v3 重写之前的 bug 根因记录 | 涉及的模块已被整体删除 |
| [CODE_REVIEW_2026-10-23.md](CODE_REVIEW_2026-10-23.md) | 第一轮全量审查（43 项） | 一次性材料 |
| [CODE_REVIEW_ROUND2_2026-10-23.md](CODE_REVIEW_ROUND2_2026-10-23.md) | 第二轮全量审查（24 项） | 一次性材料 |
| [CODE_REVIEW_2026-10-25.md](CODE_REVIEW_2026-10-25.md) | 第三轮全量审查（22 项） | 一次性材料；结论已并入 BUGFIX |
