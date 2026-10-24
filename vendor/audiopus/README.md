# audiopus（vendor 副本）

`audiopus` 0.3.0-rc.0 的本地副本，基于 crates.io 发布版修改，通过 `windows/Cargo.toml` 的
`[patch.crates-io]` 引用。上游绑定基于 Opus 1.3.1 头文件生成，为配合本项目使用
`opus-src/`（1.6.1）的 ML 特性做了以下扩展，**不要同步上游覆盖本目录**。

## 本地修改

相对上游仅有两处新增，均在 `src/coder/`：

| 文件 | 新增 | 说明 |
|------|------|------|
| `coder/decoder.rs` | `Decoder::set_complexity(u8)` | Opus 1.5 新增的解码器复杂度接口（请求码 4010，上游已绑定常量）；≥5 时启用 Deep PLC |
| `coder/encoder.rs` | `Encoder::set_dred_duration(i32)` | Opus 1.5 的 DRED 冗余时长（10ms 帧，0–100）；请求码 4050 不在 1.3.1 头文件中，函数内本地定义 |

其余源码与上游一致。许可证见 [LICENSE.md](LICENSE.md)（ISC）。

## 配套

- 共享 Opus 源码树：仓库根 `opus-src/`
- 构建脚本扩展（`OPUS_DEEP_PLC` / `OPUS_DRED` / 强制 Release）：见 `../audiopus_sys/build.rs`
- 使用方式与参数语义：见 `docs/BUILD.md`「Opus ML 特性」一节
