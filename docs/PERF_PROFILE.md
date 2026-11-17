# 性能剖析：工具链与实测基线

> 适用：v1.5.2（协议 v3，48 kHz / 单声道 / 10 ms 帧）。
> 测试基线 commit：`51b6c53f3e753cdab7a516c73739aeb5cdacd35d`
> （其后至测试时仅有 README 调整与本会话的 clippy 风格修复、依赖清单变更，不影响音频路径）。
> 本文回答两个问题：**怎么测**（工具链与本机的坑）、**测到了什么**（CPU/依赖/基准实测基线）。
> 延迟绝对值与网络侧实测见 [LATENCY_CALIBRATION.md](LATENCY_CALIBRATION.md)。

---

## 1. 工具链（2026-11-18 起可用）

| 工具 | 用途 | 备注 |
| --- | --- | --- |
| clippy / cargo-audit / cargo-machete / cargo-deny | 静态审查 | 根目录 `deny.toml`：放行宽松许可证，豁免 4 个无修复的 iced 传递依赖（pin-utils / proc-macro-error / paste / ttf-parser，均已注明原因） |
| samply | 采样剖析（Windows ETW → Firefox Profiler） | 录制流程与坑见 §2 |
| hyperfine | 命令行基准 | 配合 §4 的基准二进制 |
| tokio-console | async 任务监控 | **未接入**：需要给应用加 `console-subscriber`（建议做成 feature，默认关闭），尚未做 |
| pktmon | UDP 抓包 | Wi-Fi 抓包环境限制见 [LATENCY_CALIBRATION.md](LATENCY_CALIBRATION.md)；Npcap 在本机不可用 |

---

## 2. samply 在本机的可用流程（实测踩坑记录）

samply 0.13.1 的 `record` 在 Windows 是非官方支持路径，直接照 Linux 文档用会失败。可用流程：

```bash
# 启动录制（150 s 后需要手动收尾，见下）
samply record --save-only --unstable-presymbolicate \
  -o target/samply-profile.json target/release/udp2cal.exe
# ……串流/复现……然后收尾：
logman stop "NT Kernel Logger" -ets      # 停内核采样会话（samply 用的就是它）
taskkill /IM udp2cal.exe /F              # 结束应用，samply 随之处理 ETL 落盘
```

三个坑：

1. **`-d/--duration` 在 Windows 上不生效**：samply 会一直等子进程退出。收尾必须先
   `logman stop` 内核会话、再杀应用。直接强杀 samply 本体会在处理 ETL 时报
   "句柄无效"（0x80070006）并丢掉整份档案。
2. **kernel.etl 极大**：全系统 1000 Hz 采样，2.5 分钟 ≈ 2.7 GB，分析完要删。
3. **Rust 自产 PDB 在 sidecar 里只有 `fun_<rva>` 占位名**（系统模块符号正常）。
   用 VS 自带的 llvm-symbolizer 自行解析（地址 = RVA + ImageBase，本机
   ImageBase = 0x140000000，从 PE 头读）：

   ```bash
   printf '0x1405431c0\n' | \
     "C:/Program Files/Microsoft Visual Studio/18/Community/VC/Tools/MSVC/14.5x/bin/Hostx64/x64/llvm-symbolizer.exe" \
     --obj=target/release/udp2cal.exe -f
   ```

   部分函数会回退成 "main"（最近导出符号），但真实热点名（Opus SIMD、iced libpulp）
   都能出来。产物：`target/samply-profile.json`（`samply load` 可交互查看）。

---

## 3. 采样剖析基线（2026-11-18，150 s 录制 + 串流会话）

**结论：CPU 非瓶颈。** 全进程 CPU 累计 ≈53 s / 14 min（≈6% 单核）；所有线程
98% 以上的采样点停在内核等待（`NtWaitForAlertByThreadId` /
`NtWaitForMultipleObjects` / `MsgWaitForMultipleObjectsEx`）。

| 线程 | 空闲占比 | 有效工作热点（自耗时 / 150 s） |
| --- | --- | --- |
| `udp2cal-decode` | 98.8%（等通道） | Opus SILK 解码内部（`decode_pulses`/`hysteresis_decision`/`isqrt32`，patch 后 audiopus_sys）≈1.1 s；iced libpulp 渲染 SIMD ≈0.3 s |
| `reverse-capture`（反向采集/编码） | 98.5%（等事件） | Opus CELT 编码 SIMD（`op_pvq_search_sse2`/`celt_inner_prod_sse`/`celt_preemphasis`/`run_analysis`）≈2 s |
| `cpal_wasapi_out` | 99.7%（等事件） | 回调内几乎为零（全部 <200 ms） |
| `tokio-rt-worker` ×16 | 全部空闲 | 负载可忽略 |
| 主线程（iced UI） | 98.3%（消息泵） | 无热点 |

要点：

- **所有热点都在 Opus 库内部与 iced 渲染**；udp2cal 自研代码（JB 策略、协议、控制面）
  CPU 占用可忽略。
- 音频路径每 150 s 只消耗 1~2 s CPU，编解码余量巨大；延迟优化空间在
  网络抖动 / 时钟对齐 / 缓冲策略，不在算力（与 pktmon 实测 p99 14.7 ms、
  水位富余的结论一致，见 LATENCY_CALIBRATION.md）。
- WASAPI 事件驱动正常，无轮询空转。

---

## 4. hyperfine 基准（2026-11-18）

两个基准二进制（`cargo build --release --example <name>` 后由 hyperfine 驱动）：

| 二进制 | 位置 | 覆盖 |
| --- | --- | --- |
| `jb_bench` | `protocol/examples/` | JitterBuffer 策略：插入/查找/PLC 决策/hold 状态机的纯 CPU 开销 |
| `opus_bench` | `windows/examples/` | Opus 编/解码/经典 PLC/Deep PLC/FEC 单帧成本 |

### 4.1 JB 策略（jb_bench，1 000 000 帧 ≈ 2.8 小时音频/次）

| 场景 | wall | 行为校验（每帧 10 ms 设备节拍） |
| --- | --- | --- |
| inorder | 184 ± 19 ms | 0 PLC / 0 静音 |
| reorder（成对交换） | 172 ± 16 ms | 全部重排还原，无 PLC |
| loss1（1% 单帧丢） | 158 ± 4 ms | 丢帧全部由 PLC 填，无静音 |
| loss5（5% 单帧丢） | 155 ± 3 ms | 同上 |
| burst（每 5 s 一次 300 ms 洞） | 161 ± 9 ms | hold(120 ms) + 中段 PLC 把洞完整吸收，无 late/resync |

结论：**JB 策略 CPU 开销可忽略**——1M 帧全部逻辑加起来不到 0.2 s，
折合每 10 ms 帧约 0.16 µs，与采样剖析（解码线程 98.8% 空闲）互相印证。
`UDP2CAL_PLC_HOLD_MS`（10/120/600）对 CPU 无可测影响（纯策略分支）；
burst 场景在 hold=120 时 300 ms 洞先 hold 后转中段 PLC，恢复无损。

### 4.2 Opus 编解码路径（opus_bench，32 kbps Voip + in-band FEC，4000 帧 = 40 s 音频）

| 场景 | 每帧均值 | 折算实时占用* | hyperfine wall |
| --- | --- | --- | --- |
| dec（正常解码） | 34.7 µs | 0.35% | 168 ms |
| plc 经典（complexity 1） | 46 µs | 0.46% | 183 ms |
| plc Deep（complexity 5，OPUS_DEEP_PLC 已开） | 786 µs | 7.9% | 3145 ms |
| fec（decode_fec，LBRR 重建） | 782 µs | 7.8% | 3127 ms |
| enc（发送端编码） | 332 µs | 3.3% | 1330 ms |

\* 每 10 ms 帧的 CPU 时间 ÷ 10 ms。

要点：

- **Deep PLC / FEC 的成本是正常解码的 ~23 倍**（DNN 合成路径），但只在丢帧时
  触发：pktmon 实测丢包率 0.013%（LATENCY_CALIBRATION.md），折算 CPU 占用
  完全可忽略；持续 100% PLC 的极端断流下也只有 0.79 核。
- 编码比解码贵 ~10×（发送端每帧 332 µs，Android 反向编码同理量级），
  采样剖析里 `reverse-capture` 线程的 celt SIMD 热点与此一致。
- 调参参考：如果 Android 端弱机解码吃紧，可把解码复杂度降到 <5（换回经典
  PLC，46 µs/帧），代价是 PLC 听感变差——`decoder.rs` 的
  `DEEP_PLC_COMPLEXITY` 常量是唯一开关。
- `opus_bench --packets <file>` 会把生成的码流缓存到文件（首次生成约 1.4 s，
  hyperfine 的 warmup 会触发生成，后续运行直接加载），避免把 wall time
  淹没在编码准备里。
