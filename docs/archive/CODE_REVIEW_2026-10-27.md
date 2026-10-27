# 代码审查报告 — UDP2CAL v1.5.1（2026-10-27）

> 范围：`protocol/`（lib + cdylib，`udp2cal-protocol` v1.5.1）与 `windows/`（bin `udp2cal` v1.5.1），共 15 个自有 `.rs` 文件 / 9789 行。
> 排除：`vendor/`（audiopus 补丁副本）、`opus-src/`、`android/`、`scripts/`、`docs/`、`target/`。
> 分支：`review/auto-fix`（基线 `main` = `7f3deca`），11 个独立提交，`git diff main..HEAD` = 201 insertions / 86 deletions / 9 files。

---

## 0. 环境与门禁降级说明

| 项 | 结论 |
|---|---|
| 形态 | 双 crate 平铺仓库（非 workspace）：`protocol/` = lib + cdylib（+ 可选 `jni` feature），`windows/` = bin（iced 0.14 UI + tokio 多线程 runtime） |
| 关键依赖 | tokio 1.40、cpal 0.15、audiopus 0.3.0-rc.0（`[patch.crates-io]` → `vendor/`）、windows 0.58、socket2 0.6、winreg 0.52、tray-icon 0.24 |
| edition / MSRV | edition 2021；**无 `rust-version`、无 `resolver`、无 workspace**（见 T-04） |
| release profile | opt-level=3、lto=true、codegen-units=1、`panic="unwind"`、strip="symbols"（`panic=unwind` 使 `catch_unwind`/`Drop` 回滚可用） |
| 工具可用性 | fmt / clippy / test / audit 可用；**miri、deny、machete、semver-checks 环境不支持**（无 nightly，`error: the 'miri' component ... is not available for the 'stable-x86_64-pc-windows-msvc' toolchain`）；**仓库无 CI 配置** |

**准出门禁**：基线可编译，`protocol` 74 passed / `windows` 21+2 passed / 0 failed，允许进入功能修复；但基线 clippy 为红（protocol 8 error / windows 4 error），故第一批**只做 clippy 清零**，之后才动语义。

---

## 1. 发现的问题（11 项 + 8 项待确认）

| 级别 | 数量 | 条目 |
|---|---|---|
| P1 | 1 | F-01 依赖漏洞 crossbeam-epoch |
| P2 | 3 | F-02 `build_control_to` 溢出可绕过校验并 panic；F-03 `recv_from` 错误静默 + 忙等；F-04 7 处 Mutex 中毒即静默跳过 |
| P3 | 7 | F-05~F-09 lint / unsafe 文档 / 加固；F-10 全量 fmt 不符；F-11 无 CI/MSRV/resolver |

---

## 2. 已修改的问题（7 项，均带验证证据）

### F-02（P2 / L1）`protocol/protocol.rs:204` 整数溢出绕过缓冲区校验

```rust
// 改前：offset + total > dest.len()   ← release 下回绕，绕过后 dest[offset] panic
let end = offset.checked_add(total)?;
if end > dest.len() {
    return None;
}
```

证据：新增回归测试 `build_control_to_rejects_overflowing_offset`；把该行 sed 回原写法后测试报
`panicked at protocol.rs:200: attempt to add with overflow`，修复后 75 测试全过。
安全性：仅把「回绕后误判为合法」改为「显式返回 None」，与函数文档契约一致，合法输入路径输出字节不变。

### F-03（P2 / L1）`windows/src/main.rs:1949-1951` socket 错误静默丢弃 + 忙等

改前 `Ok(Ok(v)) => v, _ => continue`：把 `WSAECONNRESET`（Windows 上 ICMP 端口不可达回灌）当无事发生并立即重试 → 无退避忙等可吃满一个核，且日志无痕。
改后：错误分支 `logf!` + 限流闸门 + `tokio::time::sleep(Duration::from_millis(10))`；超时分支行为不变。

### F-01（P1 / L1）`crossbeam-epoch 0.9.18 → 0.9.20`

RUSTSEC-2026-0204（`fmt::Pointer` 对 Atomic/Shared 的非法解引用），路径 `crossbeam-deque ← rayon-core ← rayon ← image/exr/rav1e`。
仅动 `windows/Cargo.lock` 两行；`cargo audit` 由 `1 vulnerability found!` → 0 漏洞。

### F-04（P2 / L1）Mutex 中毒策略统一

7 处 `if let Ok(..) = ..lock()` 改为 `unwrap_or_else(|e| e.into_inner())`，与文件内既有约定（`p2p_lock`）一致。原来中毒时：日志闸门全静默、反向发送线程不重启、编码器配置不再更新。附带改进：`stop()`（join 线程）现在在锁外执行。

### F-05 / F-06（P3 / L0）clippy 清零 12 条

`assert!(常量比较)` → `const { assert!(..) }`、`&vec![..]` → `&[..]`、`field_reassign_with_default` → 结构体更新语法、`Ok(samples) if samples == 480` → `Ok(480)`、单分支 `match` → `if let`、`% 50 == 0` → `is_multiple_of(50)`、`build_stream` 9 参数 → 新增私有 `struct StreamShared`（`windows/src/audio.rs:503`，函数降为 3 参数，回调体一字未改）。

### F-07 / F-08 / F-09（P3 / L0）unsafe 文档与固化

- `windows/src` 13 处 unsafe 全部补 `// SAFETY:`；
- `protocol/jni.rs` 76 处 `unsafe fn` 用模块级 4 条契约 + 关键基元（`fns` / `read_bytes` / `lock` / 句柄解引用 / `jbCreate` / `jbDestroy`）定向注释；
- `protocol/protocol.rs:26` 加 `#![deny(unsafe_code)]`，`protocol/jni.rs` 为唯一 `#![allow(unsafe_code)]` 例外。

`clippy -W clippy::undocumented_unsafe_blocks` 两 crate 均为 0。

---

## 3. 未修改的问题及原因

- **F-10 全量 fmt**：仓库**无 rustfmt.toml**，实测六种 `max_width × use_small_heuristics` 组合最好仍有 protocol 39 / windows 70 个 hunk → 代码为手写风格，从未跑过 rustfmt。强制格式化 = 1218 行纯空白 diff + 破坏 blame，属维护者风格决策而非缺陷修复 → 未执行。命令：`cd protocol && cargo fmt && cd ../windows && cargo fmt`，建议单独 format-only 提交并加 `.git-blame-ignore-revs`。
- **T-01 jni 重复 `jbDestroy` UAF**：句柄生命周期归 Kotlin，Rust 侧无法防御（需 generation / 句柄表），L3。
- **T-02 `let _ = self.config.save()` ×3**：保存失败对用户不可见，修它要改 UI 错误提示语义，L2。
- **T-03 9 条 audit allowed warning**：`paste` / `proc-macro-error` / `ttf-parser`（unmaintained）、`anyhow` RUSTSEC-2026-0190、`glib` 2024-0429、`lru` 2026-0253、`memmap2` 2026-0186、`tokio` 1.52.3 的 `AsyncFd`（2026-0386，本项目 Windows 走 IOCP 不触发）、`clipboard-win` 5.4.1（yanked）——全为传递依赖，需 major 升级，L4 不自动执行。

红线遵守：未用 `unwrap/expect` 消错、未为消警告加 `#[allow]`、未改任何测试断言、未动公共 API / serde 格式（→ semver-checks 门禁本就不适用）、未混提机械与语义改动。

---

## 4. unsafe 审计结果（四分类）

| 位置 | 数量 | 结论 |
|---|---|---|
| `windows/src/capture.rs` | 8 | ② 合法但缺注释 → 已补。`read_stereo` 的 `from_raw_parts` 长度取 `GetBuffer` 返回值、类型由 `self.format` 决定、SILENT/空指针不读；`parse_mix_format` 有 null / 通道数 / 采样率 / 位深白名单校验；`CoInitializeEx` 由 `ComGuard` 配对 |
| `windows/src/main.rs` | 4 | ② → 已补。`PCWSTR::from_raw` 的 `Vec<u16>` 均 NUL 结尾且存活于调用期；`EnumWindows` 的 `LPARAM` 指向同步栈变量 |
| `windows/src/logger.rs` | 1 | ② → 已补。`GetLocalTime` 无非空指针输出契约问题 |
| `protocol/jni.rs` | 76 | ② → 已补契约文档。`read_bytes` 的 null / 负值 / `offset+len>total` 三查完备；`Env` 仅本次调用有效、函数表恒非空、句柄所有权归 Kotlin |
| ③ 存疑 | 0 | — |
| ④ 确认 UB | 0 | — |

`protocol/rtp.rs`、`protocol/protocol.rs`、`windows/src/{config,decoder,diag}.rs` 零 unsafe，已用 `#![deny(unsafe_code)]` 固化（windows 其余模块见 T-08）。

---

## 5. 性能优化结果

未执行任何性能优化——仓库无 criterion 基准、无 profiling 证据，按规约拒绝无依据微优化。
唯一相关改动是零成本参数打包重构（`StreamShared`），热路径（`AudioWriter::write` 多相 FIR、`Ring` 原子环、`run_loop` 发送循环）逐行未变；`audio.rs` 11 个数值测试（重采样帧率 47975±3、44.1k 上采 44077±4、12k 平坦度 <0.1 dB、块边界无咔哒）仍全过。

---

## 6. 安全检查结果

- **注入**：无 SQL；`netsh` 以参数数组传递（`windows/src/firewall.rs`），无 shell 拼接；日志/诊断路径固定在 `%LOCALAPPDATA%\UDP2CAL\`，无用户可控路径拼接。
- **输入边界**：控制面 `ControlHeader::decode` / `parse_control` 严格校验版本 / 长度 / `payload_len ≤ 512`；RTP `parse` 校验 V=2 / P / X / CC / PT=111 / SSRC≠0 / payload 1..=1275；`demux` 先看首字节分流；无递归解析、无 `read_to_end`，收包缓冲固定 2048。
- **panic 面**：生产路径除 JNI 函数表调用外无 `unwrap/expect/panic!/unreachable!/todo!`。
- **认证/机密**：v3 无加密无认证（设计如此，`device_id` 是公开标识），最小权限靠 `is_bound_peer_source`（已绑定设备换端点一律拒绝）+ 「反向编码配置更新必须在门之后」的护栏；本机无 TLS / 密码学代码面。
- **依赖**：audit 0 漏洞（修复 1）。
- **DoS 面**：错误包日志 1 s 闸门；发现设备令牌桶 5/s、5 s 过期、30 s 清理。

---

## 7. 基线 vs 终态

| 门禁 | 基线 `main` | 终态 `review/auto-fix` | 判定 |
|---|---|---|---|
| protocol clippy `-D warnings` | 8 error | **0** | 改善 |
| windows clippy `-D warnings` | 4 error | **0** | 改善 |
| protocol test | 74 passed / 0 failed | **75 passed / 0 failed**（+1 回归测试） | 不减 |
| windows test | 21 unit + 2 integration / 0 failed | **21 + 2 / 0 failed** | 持平 |
| protocol `test --release` | 未执行 | 75 passed | 通过 |
| windows `check --release` | 未执行 | 通过（1m01s） | 通过 |
| doc-test | 0 | 0 | 通过 |
| `cargo audit` | windows 1 漏洞 | **0 漏洞** / 9 allowed warning | 改善 |
| `clippy -W undocumented_unsafe_blocks` | windows 1 | **0**（两 crate） | 改善 |
| `cargo fmt --check` | 红（protocol 47 / windows 78 hunk） | 仍红（49 / 77，新增注释未实质增加） | 未修，需授权（F-10） |
| miri / deny / machete / semver-checks | 工具缺失 | 未执行 | 环境不支持 |

---

## 8. 仍然存在的风险

1. `cargo fmt --check` 仍为红，任何「验证全绿」的 CI 会失败（历史风格问题，非本次引入）。
2. JNI 句柄裸指针 + Kotlin 侧负责 `close()`：重复 destroy 即 UAF，Rust 侧不可防。
3. 9 条传递依赖 advisory（含 1 条 yanked、1 条 tokio AsyncFd）仍在树内。
4. 实时音频路径（抖动缓冲、ASRC 环、阶跃吸收）只有单测，无 loom / proptest / 压测，并发不变量靠人工论证。
5. 无 CI 门禁 ⇒ 上述改进没有回归防线。

---

## 9. 后续建议（按优先级）

1. 加 CI：`--locked` 下跑 clippy `-D warnings` + test + audit，fmt 先单独成 job 并 `continue-on-error`。
2. 若接受风格统一：`cargo fmt` 单独提交 + `.git-blame-ignore-revs`。
3. 补 `rust-version`（实测 1.99 通过；建议按 `is_multiple_of` / `const {}` 定 1.87+）与 `resolver = "2"`。
4. 处理 T-03（跟随上游 major 升级）、T-01（Kotlin 侧 close 幂等或 Rust 侧换代句柄）。
5. 用 `proptest` 覆盖 `parse_control` / `demux` / `extend_seq` / `extend_ts`，用 `loom` 覆盖 `Ring` 的 flush 竞态。

---

## 10. 回滚指引（逐提交独立可回滚）

| 提交 | 内容 | 风险 |
|---|---|---|
| `ed0b83c` / `d2c09d2` | clippy 清零 | 无（测试 / 等价改写） |
| `5f42742` | `StreamShared` 打包 | 无（纯参数重构） |
| `d27a7ff` / `929aa90` | SAFETY 注释 + jni 契约 | 无（纯注释） |
| `179f8ac` | `checked_add` 溢出修复 | 无（+1 测试） |
| `ed80330` | crossbeam-epoch → 0.9.20 | 极低（仅 lockfile） |
| `466e31f` | recv 错误退避 | 低（错误路径新增 10 ms 延迟） |
| `33ce6bc` | `#![deny(unsafe_code)]` | 无（编译期约束） |
| `b236514` | 拆超宽行 | 无（纯格式） |
| `4c21676` | Mutex 中毒恢复 | 低（仅中毒分支语义） |

整体回滚：`git reset --hard 7f3deca`，或 `git revert --no-commit 7f3deca..HEAD` 一次性反做。

---

## 待确认清单

| 编号 | 问题 | 无法确认的原因 | 建议调查路径 |
|---|---|---|---|
| T-01 | `jbDestroy` 重复调用 → UAF | 句柄所有权在 Kotlin，Rust 侧无防御手段 | Kotlin `close()` 幂等 + Rust 侧句柄表 / 代数 |
| T-02 | `let _ = self.config.save()` ×3 吞错 | 需改 UI 错误提示语义 | `save()` 失败时发 `NoticeLevel::Error` |
| T-03 | 9 条 audit allowed warning（含 yanked） | 需 major 升级，L4 | 逐依赖评估上游修复版本 |
| T-04 | 无 `rust-version` / `resolver` | 兼容性决策（MSRV 提升对库是破坏性变更） | 实测最低可编译版本后加 CI 矩阵 |
| T-05 | 无 CI 配置 | 需要仓库权限 / 平台选择 | 见「后续建议 1」 |
| T-06 | `should_log_control_reject` 被 UDP 收包错误复用 | 命名语义漂移，仅风格 | 重命名为 `should_log_throttled` |
| T-07 | 抖动缓冲 / 阶跃吸收无 loom、proptest | 需新增 dev-dependency | `proptest` 先覆盖纯函数，`loom` 覆盖 `Ring` |
| T-08 | windows 其余模块未加 `deny(unsafe_code)` | capture/main/logger 含 unsafe，需模块级 allow | 对 config/decoder/diag/firewall/audio 加 `#![deny(unsafe_code)]` |

---

## 附录（同日追加）：F-10 已执行

审查完成后维护者授权执行全量格式化，故 F-10 状态由「未修，需授权」改为**已修**。

- 提交：`style: cargo fmt 全量格式化（纯格式，无逻辑变更）`，11 个文件 / 1197 插入 / 746 删除，**独立成提交，不掺任何语义改动**。
- 变更类别只有三种：换行/缩进/行宽重排、`use` 顺序按字母表重排（`reorder_imports` 默认行为）、闭包体补 `{}` 与单表达式 `match` 臂去 `{}`。
- 等价性验证：剔除 `use` 语句并去掉空白与逗号后逐文件比对 token 流——7 个文件完全一致，4 个文件仅差上述 `{}` 各 2 字节，无 token 语义变化。
- 门禁：两 crate `cargo fmt -- --check` 均干净；protocol clippy 0 告警 / 75 passed；windows clippy 0 告警 / 21+2 passed。
- 因此「第 7 节」表中的 `cargo fmt --check` 一行应读作：基线红 → 终态**绿**。
- 未添加 `.git-blame-ignore-revs`（维护者要求只做 fmt）；需要时：
  `echo 56e0c96 >> .git-blame-ignore-revs && git config blame.ignoreRevsFile .git-blame-ignore-revs`
- T-05「无 CI」按维护者决定**不处理**：不加 CI 配置，「后续建议 1」作废。
