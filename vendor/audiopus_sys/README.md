# audiopus_sys（vendor 副本）

`audiopus_sys` 0.2.2 的本地副本，基于 crates.io 发布版修改，通过 `windows/Cargo.toml` 的
`[patch.crates-io]` 引用，**不要同步上游覆盖本目录**。

## 本地修改

仅改 `build.rs`，两点：

1. **Opus 源码解析**（`opus_source_path()`）：优先 `OPUS_SOURCE_DIR` 环境变量，
   默认回落到仓库根的共享源码树 `../../opus-src`（Opus 1.6.1），
   最后才是上游兜底 `./opus`（本副本不含内置源码，该路径不存在）。
2. **CMake 构建配置**：
   - 强制 `CMAKE_BUILD_TYPE=Release`——dev profile 下默认 Debug（/MDd）会引入
     `__imp__CrtDbgReportW` 等调试 CRT 符号，与 rustc 链接的 /MD 不兼容（LNK2001）
   - `OPUS_DEEP_PLC=ON` + `OPUS_DRED=ON`（ML 特性，Android 端 CMake 同步开启；
     OSCE / QEXT 保持关闭）

上游的 `opus/` 内置源码目录未随本副本入库，因此不设置任何来源时构建会 panic
（`canonicalize().expect`），这是预期行为：请使用默认的 `opus-src` 回落。

其余源码（`src/lib.rs` 预生成 FFI 绑定）与上游一致。许可证见 [LICENSE.md](LICENSE.md)（ISC）。
