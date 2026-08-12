# Windows Window Demo

独立的 Rust + iced + tray-icon 窗口行为示例
## 运行

在 Windows 上执行：

```powershell
cargo run --manifest-path .\windows_window_demo\Cargo.toml
```

或双击 `build_debug.bat` 编译 Debug 版本，输出到 `target\debug`。

双击 `build_release.bat` 编译 Release 版本，输出到 `target\release`。

## 行为

- 最大化按钮切换窗口最大化状态。
- 全屏按钮切换独占全屏和普通窗口。
- 最小化按钮保留任务栏图标并最小化窗口。
- 默认点击窗口 `X` 直接退出；勾选“关闭时最小化到托盘”后，点击 `X` 隐藏到托盘。
- “隐藏到托盘”按钮始终直接隐藏窗口，不退出进程。
- 托盘图标双击恢复并聚焦窗口。
- 托盘图标右键菜单中的“退出”结束进程。
