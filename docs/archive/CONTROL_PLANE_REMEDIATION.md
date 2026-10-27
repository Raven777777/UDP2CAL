# 控制面与网络架构审查及分阶段整改计划

> 状态：**阶段性归档：首轮审查和低风险修复已完成；v4 认证、媒体保护及实机验证未完成。本文不表示所有缺陷已关闭。**
> 本文是审查快照，不是协议规范。现行 v3 线格式、字段语义和状态机以 [PROTOCOL_V3_RTP.md](../PROTOCOL_V3_RTP.md) 为准；实现位置以 [ARCHITECTURE.md](../ARCHITECTURE.md) 为准。后续若恢复整改，应重新审视本计划与当前代码。
> 本轮及后续首批整改已记录审查结果、修正文档冲突并实现部分低风险硬化（CONNECT 字段验证、v3 ACK 对端检查、绑定端点固定、防火墙 Private profile）。这些检查不是身份认证；协议层伪造、重放和媒体注入问题仍未解决。

## 1. 结论摘要

当前实现适合**可信局域网、单 Windows 接收端与单 Android 发送端**的使用假设。它不是经过认证的控制面：设备 ID、源地址和 SSRC 用于发现、绑定与误连防护，但不能证明对端身份。代码和文档都承认控制包明文、没有认证与密钥交换；但两端还没有一致执行身份与端点校验，导致当前实现弱于协议描述。

需要分别处理三类问题：

1. **实现缺陷**：端点与身份校验不完整、两端不对称；字段验证宽松；防火墙作用域过宽。
2. **设计限制**：单设备独占、IPv4 广播发现、无跨路由发现、明文且无认证/重放保护。
3. **文档缺陷（本轮已修复）**：原控制协议码率字段写最大 1000 kbps，与实现 512 kbps 冲突；架构文档曾容易让人误解为共享协议实现意味着两端会话逻辑一致。

## 2. 已确认的问题与代码落点

### 2.1 控制面身份与会话绑定

- **Android ACK 校验（部分已修复）**：`android/app/src/main/java/com/udp2cal/app/UdpSender.kt` 现在验证源 IP/端口；从发现列表选取目标时也验证 ACK header `device_id`。手工输入目标没有可信预期 ID，只能校验 IP/端口。ACK 结构仍没有 nonce / 会话 ID，延迟或重放的旧 ACK 仍无法与当前连接区分。实现：`UdpSender.kt` / `Prefs.kt` / `MainActivity.kt`；协议目标：`docs/PROTOCOL_V3_RTP.md` §3.3、§5.5。
- **Windows v3 端点绑定（已加固但未认证）**：`windows/src/main.rs` 现在要求同 ID peer 的 CONNECT 来自原始完整 `SocketAddr`，未经认证的 IP/端口变化不再覆盖绑定；端点变化需等待 3 秒超时后重连。device_id 仍是明文公开标识，精确源地址检查也不防同网段伪造。实现：`is_bound_peer_source()` 与 `handle_control`；有对应单元测试。
- **没有密码学身份认证或重放保护**：控制包、RTP 均为明文；device_id 和 SSRC 不是凭证；无 MAC、密钥协商、nonce 或会话代次。协议文档将其列为非目标（`docs/PROTOCOL_V3_RTP.md` §1.2、§12）。这属于协议能力边界，不应靠单纯增加 ID / 地址比较误称为已解决。
- **控制状态与协议行为分散**：线格式由 `protocol/` 共享，但连接状态机分布在 Android `CaptureService.kt` / `UdpSender.kt` 和 Windows `main.rs`。共享序列化实现不保证两端状态迁移、重连和身份策略一致。

### 2.2 网络架构

- **单客户端独占**：Windows 只有 Ready / Busy 绑定，不支持并发 peer 或显式仲裁；异设备 CONNECT 静默丢弃。属当前产品拓扑限制，不是多客户端架构。
- **发现仅覆盖 IPv4 广播域**：Android 使用 `255.255.255.255:44043`；不跨路由/VLAN，也没有 IPv6 发现。已知地址的单播 DISCOVER 不等于跨网自动发现。
- **防火墙 profile（实现已收窄，待实机验证）**：`windows/src/firewall.rs` 入站规则现在限定 `profile=private`，并保留程序与 UDP 端口约束。公共 profile 不应放行；Windows 实际 profile 切换和提示 UX 仍需验证。
- **控制与媒体共享 Windows UDP 接收循环**：`windows/src/main.rs` 同一 tokio 接收流解复用控制和 RTP；控制处理会更新绑定、重置缓冲/反向发送器并发送 ACK。共享 socket 是当前设计，但处理隔离与耗时边界需评估，避免控制操作阻塞媒体包处理。

### 2.3 控制协议与规范一致性

- **CONNECT 字段校验（首批已收紧）**：`protocol/protocol.rs` 的 `ConnectPayload::validate()` 现在拒绝 complexity > 10、signal > 2、bandwidth > 4、非零保留 opus_flags 位和 packet_loss > 100；complexity=0 仍表示默认，bitrate > 512 仍按规范钳制。Rust 单元测试覆盖这些边界；跨端状态测试仍待补齐。
- **码率上限文档冲突（本轮已修正文档）**：协议实现 `BITRATE_MAX_KBPS = 512`，但 CONNECT 表格/规则原写最大 1000。真实有效上限为 512；协议文件已同步到 512。检查点：`docs/PROTOCOL_V3_RTP.md` §5.4；实现：`protocol/protocol.rs` 的 `BITRATE_MAX_KBPS` / `clamp_bitrate_kbps()`。
- **ACK 与请求缺少关联**：ACK 不包含 CONNECT nonce / 会话 ID，无法识别旧会话 ACK；该问题与认证和重放保护一并设计，不建议在 v3 上临时叠加不兼容字段。

## 3. 分阶段整改与验收门

执行规则：**每阶段先明确协议决策，再改规范，再实现与测试，最后更新架构/变更文档。** 未通过当前阶段验收，不进入依赖该阶段的后续工作。线协议破坏性变更应明确升版；不得让 v3 的隐式新旧语义并存。

### 阶段 0 — 基线与文档对齐（已完成首轮）

- [x] 记录架构、网络和控制协议问题及代码落点。
- [x] 修正控制协议文档中 1000 / 512 kbps 冲突，以实现上限 512 kbps 为准。
- [x] 对 CONNECT 字段范围与两端状态机做首轮复核；剩余状态机差异仍列为后续阶段任务。
- **验收（首轮通过）**：码率上限文档/实现一致，CONNECT 已复核字段范围；剩余状态机差异已记录于后续阶段。

### 阶段 1 — v3 行为一致性与低风险校验（部分完成）

- [x] 明确 CONNECT 字段规则：complexity 0 或 1..=10、signal 0..=2、bandwidth 0..=4、保留 opus_flags 位必须为 0、packet_loss 0..=100；非法项拒绝。bitrate > 512 继续按规范钳制。
- [x] Android ACK 校验源 IP/端口；发现选择时校验 Windows device_id。
- [x] Windows 绑定期间拒绝完整源端点变化；覆盖源 IP、端口变化与 ID 不匹配的测试。
- [x] Rust CONNECT 字段边界测试、Android ACK peer predicate 测试。
- [ ] 完成 Android/Windows 超时、重连、SSRC 状态迁移的跨端集成测试，并在 Android 构建环境验证 JVM 测试。
- **验收**：规范矩阵逐字段有测试；错误输入不会建立或改变绑定；正常续租、断连、重连和反向开关行为可复现。

### 阶段 2 — 确定安全模型并设计下一协议版本

- [x] 安全目标选择：不把“可信 LAN”当作认证；下一协议版本应抵御同网段伪造、重放及媒体注入。保留单 peer 与 IPv4 同广播域发现，暂不扩展多客户端/IPv6/跨 VLAN。
- [ ] 设计用户批准的高熵配对凭据、双向握手、密钥存储/撤销/轮换、会话密钥派生、抗重放和降级策略，并形成可审查的 v4 wire spec 后再实现。
- [ ] 将 CONNECT、ACK、端点迁移、RTP 会话绑定统一关联到认证后的会话；不得将明文 device_id、IP 或 SSRC 描述为认证凭据。
- [ ] 评估升级兼容、拒绝降级及 Android/Windows 同步发布策略；预计需要新协议版本，而非悄悄改变 v3 线格式。
- **验收**：威胁模型、线格式、状态机、错误处理、密钥生命周期及测试向量经过审查后，才实现代码。

### 阶段 3 — 实现认证会话与端点迁移

- [ ] 按阶段 2 决策实现共享协议层与两端状态机；加入过期会话、乱序/重复握手、重放、伪造 ACK、端点变化测试。
- [ ] 绑定更新必须以通过认证的会话迁移为条件；ACK 必须验证目标 peer、端口/身份与当前会话。
- [ ] 确保反向 RTP 与正向 RTP 都受同一会话代次约束，旧会话数据不能污染新会话。
- **验收**：自动化测试证明伪造/旧 ACK 不刷新连接，未经认证的 CONNECT 不能抢占或迁移绑定，正常 Wi-Fi 地址变化按设计恢复。

### 阶段 4 — 网络暴露与拓扑决策（部分完成）

- [x] Windows 防火墙入站规则从所有 profile 收窄为 Private profile，且仍限制程序和 UDP 发现/音频端口。
- [ ] 在真实 Windows 公共和私有网络 profile 上验证规则生效/不生效；定义公共网络提示行为。
- [ ] 明确单客户端独占是否是长期产品约束；若不是，先设计 peer/session 管理和资源隔离，不直接把全局单例扩成多客户端。
- [ ] 明确是否支持跨 VLAN、IPv6 或手动配置地址；只实现已确定的目标，不把广播发现描述为跨网发现。
- **验收**：防火墙与部署文档一致；目标网络矩阵（私有、公用、路由/VLAN、IPv6）有实际测试。

### 阶段 5 — 接收路径隔离与运行诊断

- [ ] 测量控制处理、缓冲重置、反向 sender 停止/重启对共享 UDP 接收循环的阻塞时间。
- [ ] 若测量表明影响实时收包，将控制消息转为有界队列/单独任务处理；定义队列满时的丢弃、去重和优先级策略。
- [ ] 增加控制面可观测性：拒绝原因计数、当前会话代次、最近合法 CONNECT/ACK 时间；不记录密钥或敏感凭证。
- **验收**：压力测试下 RTP 接收延迟/丢包不因控制风暴显著恶化；队列有界、日志可用于定位状态分歧。

## 4. 本轮范围与后续入口

### 本轮验证记录

- `cargo test --manifest-path protocol/Cargo.toml`：68 passed。
- `cargo test --manifest-path windows/Cargo.toml`：21 unit tests + 2 integration tests passed。
- `git diff --check`：通过。
- Android JVM 测试未能运行：仓库没有 `android/gradlew`，环境中也没有系统 `gradle`（`gradle: command not found`）；新增 Kotlin 测试尚未由 Gradle 编译执行。
- `cargo fmt --check` 未通过：仓库中 `protocol/jni.rs` 与 `windows/src/main.rs` 存在格式差异；尚未统一格式化，以免把无关格式改动混入整改。

本归档记录的后续方向为**安全的后继协议版本（v4）**：不把 public device_id / IP / SSRC 当凭证；需要用户批准的高熵配对凭据、认证会话、重放防护，并保护媒体流。当前第一批修改仅加固 v3 易错边界，不解决同网段伪造或媒体注入。保留现有单 peer、IPv4 广播域产品拓扑；公共网络默认不开放入站规则。若恢复该工作，先审查 v4 威胁模型、配对 UX、密钥生命周期、协议线格式和测试向量；v3 不得静默降级满足安全连接。
