# UDP2CAL 全流程审查报告（2026-10-28）

> 归档材料。现行为真源仍是代码与 [PROTOCOL_V3_RTP.md](../PROTOCOL_V3_RTP.md)；
> 修复根因摘要见 [BUGFIX.md](../BUGFIX.md)，版本记录见 [CHANGELOG.md](../CHANGELOG.md)。

## 范围与基线

- 基线：`main` / `9359d9f`
- 实施分支：`review/audit-2026-10-28`
- 范围：`protocol/`、`windows/`、Android Kotlin/C/JNI 包装、构建配置
- 排除：`target/`、`vendor/`、`opus-src/`、Android 生成目录
- 项目形态：两个平级 Rust crate（协议库 + Windows GUI 二进制）与 Android Compose 应用
- 运行时：Windows tokio；Android Coroutines；音频热路径使用原生线程和阻塞 I/O
- FFI：JNI、WASAPI、Win32、注册表与防火墙

## 基线状态

| 门禁 | 结果 |
|---|---|
| protocol fmt / Clippy | 通过 / 0 警告 |
| protocol tests | 80 passed（独立 target；默认 target 有生成 DLL 链接锁） |
| Windows fmt / Clippy | 通过 / 0 警告 |
| Windows tests | 42 单测 + 2 集成通过 |
| Android JVM/JNI tests | 63 passed（JDK 17） |
| RustSec | 0 vulnerabilities / 9 warnings |

工具缺失：`cargo-machete`、`cargo-deny`、`cargo-semver-checks`、`cargo-geiger`、
`cargo-hack`、`cargo-llvm-cov`、`cargo-fuzz`、`adb`。稳定版 Windows MSVC
工具链没有 Miri 组件；真机音频、WASAPI 热插拔与多网卡抓包未执行。

## 问题统计

按根因合并，共确认 23 项：

| 级别 | 总数 | 已修 | 保留 |
|---|---:|---:|---:|
| P0 | 0 | 0 | 0 |
| P1 | 6 | 1 | 5 |
| P2 | 13 | 5 | 8 |
| P3 | 4 | 2 | 2 |

另有 4 项待确认，不计入上述总数。

## 已修问题

| 优先级 | 问题 | 修复 | 提交 |
|---|---|---|---|
| P1 | Android 服务直接创建 `AudioRecord`，Lint 报 3 个 `MissingPermission` | 创建前显式检查 `RECORD_AUDIO`，失败返回稳定错误 | `6b182c2` |
| P2 | Windows 未执行规范“全零 device_id 无效” | CONNECT 与 DISCOVER_REQ 在进入状态机前统一校验 | `ed6688a` |
| P2 | 发现限流表按源 IP 增长，只有周期清理，没有容量上限 | 增加 1024 源硬上限，保留已有源的 5 次/秒规则 | `942484f` |
| P2 | `anyhow`、`memmap2`、`tokio` 命中 RustSec unsound advisory | 更新至兼容修复版本，无 major 升级 | `8dbd9f2` |
| P3 | `HiResTimer` 无条件声明不必要的 `Send + Sync` | 删除两个 unsafe impl，改为线程局部类型 | `cdb6b4f` |
| P3 | `StepAbsorber::last_water_ms` 只写不读 | 删除私有死字段并修正注释 | `be3a218` |
| P2 | Android `ConnectPayload.isValid()` 与 Rust 校验不一致 | 补齐复杂度、signal、bandwidth、保留 flags、丢包率校验 | `8a535f2` |
| P2 | 持久化复杂度读取时未钳制，损坏配置可永久连接失败 | 读写在同一个纯函数中钳制到 1–10 | `e341d8c` |

## Unsafe 审计

- 分类①合法且论证充分：Win32/WASAPI、窗口枚举、JNI 数组搬运与句柄关闭。
- 分类②合法但缺注释：本轮为 0；`clippy::undocumented_unsafe_blocks` 在两个 Rust crate 上通过。
- 分类③存疑：JNI 无类型句柄和并发 destroy 生命周期、导出入口无 `catch_unwind`。
- 分类④确认 UB：0。
- 本轮未新增 unsafe，删除了 2 个不必要的 unsafe impl。
- Miri 未执行：稳定版 Windows MSVC 工具链没有该组件。

## 保留风险

### P1

1. 首次 CONNECT 仍只凭公开 `device_id` 绑定，没有配对或认证；同网段主机可抢占绑定。
2. Android 旧反向播放协程 1 秒未退出时仍启动新协程，可能短暂出现两个消费者。
3. Android `onDestroy` 在主线程 `runBlocking`；阻塞 AudioTrack 写入和停止路径可能超过 1 秒。
4. Kotlin native handle 没有引用计数或统一关闭锁，close 与 native 调用仍存在竞态。
5. JNI `jlong` 句柄没有类型标签，导出入口没有 panic 边界。

### P2

- C 普通解码入口未要求返回恰好 480 采样/声道。
- C 编码器忽略 `opus_encoder_ctl` 返回值，热更新恒报成功。
- Android 反向 RTP 只校验来源 IP，不校验端口；与 Windows 的完整端点策略不对称。
- Windows 反向发送线程的同步 `join` 由控制面调用，可能阻塞实时接收任务。
- Windows 输出流错误回调仍吞掉错误，缺少设备健康上报与自动恢复。
- 未配置发布密钥时 Release 回落到本机 debug 签名，只适合自测。
- Android Lint 仍是非阻断门禁。
- 两个 Rust crate 没有 MSRV 字段，仓库没有 CI feature/MSRV 矩阵。

### P3

- Android 发现异常使用 `printStackTrace()`，用户可见诊断不足。
- Android Lint 仍有 unused resources、图标密度、备份规则与 wakelock 警告。

## 待确认

- 多网卡 Windows 主机回 ACK 时，Android 对源 IP 字符串的严格比较是否会导致无法连接。
- Android 反向流是否应校验来源端口，还是需要兼容 NAT 端口变化。
- debug 签名回落是否仅用于内部自测。
- 剩余 RustSec 警告中 `glib`/`lru` 的 target-all 可达性与上游迁移路线。

## 终态验证

| 门禁 | 基线 | 终态 |
|---|---:|---:|
| protocol tests | 80 | 80 |
| Windows tests | 42 + 2 | 44 + 2 |
| Android tests | 63 | 64 |
| Android Lint | — | 0 errors / 16 warnings |
| RustSec | 0 vuln / 9 warnings | 0 vuln / 6 warnings |

- protocol / Windows fmt 与 Clippy（含 unsafe lint）通过。
- Windows `cargo build --release --locked` 通过。
- Android `assembleDebug` 通过，包含 CMake/C/JNI 编译。
- Android Debug APK SHA-256：
  `ac3693ec7df318393d3d6bfe742f7708af1663f4acedca3d6a8a912ad5ff748d`
- Windows release EXE SHA-256：
  `d99247725837ea53bad9c9a3638c887d0e5bc3e8833ff5db544f691c607fc7f0`
- 未做性能优化：没有 profiling 或 Criterion 依据。

## 后续建议

1. 引入配对确认或预共享密钥认证。
2. 统一 JNI 句柄类型、引用计数和 panic 边界。
3. 重构 Android AudioTrack 取消/收尾协议，移出主线程阻塞等待。
4. 为 Android C 解码器增加固定 10 ms 帧长的原生回归测试并严格拒绝其它长度。
5. 显式化 MSRV、CI feature 矩阵和 Android Lint 硬门禁。
