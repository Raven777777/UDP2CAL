# 构建与产物

> 相关文档：[架构说明](ARCHITECTURE.md) · [文档索引](README.md)

---

## 一、环境要求

### Windows 端

| 项目 | 要求 |
|------|------|
| Rust | 1.96+ |
| 编译器 | Visual Studio Build Tools 2022（需勾选「使用 C++ 的桌面开发」） |
| CMake | 3.22+ |

### Android 端

| 项目 | 要求 |
|------|------|
| JDK | 17~22 |
| Android SDK | API 35 |
| NDK | 27.0.12077973 |

> 构建脚本会自动检查 JDK / SDK / gradle wrapper，并修正 `local.properties`；产物复制到项目根目录。

---

## 二、系统要求（运行）

| 平台 | 最低版本 | 架构 | 说明 |
|------|---------|------|------|
| **Android (android)** | **Android 10 (API 29)+** | arm64-v8a (64-bit) / armeabi-v7a (32-bit) | minSdk=29, targetSdk=35, 需要 `RECORD_AUDIO` 权限。面向现代设备，Jetpack Compose UI |
| **Android (android_old)** ⚠️ 已停止开发 | **Android 6~8 (API 21~26)** | armeabi-v7a (32-bit 推荐) / arm64-v8a | minSdk=21, targetSdk=26, 无 Compose。仅归档；v1.1.2 起 PC 端不再兼容，需成对使用 [Release v1.1.1](https://github.com/Raven777777/UDP2CAL/releases/tag/v1.1.1) |
| **Windows** | **Windows 10+** | x86_64 (64-bit) / i686 (32-bit) | 依赖 WASAPI 音频引擎 + VB-Cable 虚拟声卡；系统托盘和防火墙规则仅 Windows 可用 |

---

## 三、统一构建入口

所有构建均通过项目根目录的 `build.bat` 完成；不带参数运行可进入交互式菜单。

### Windows 接收端

```bat
build.bat win --release          :: x86_64 Release（默认）
build.bat win --x86 --release    :: i686 Release
build.bat win --debug            :: x86_64 Debug
build.bat win --x64 --release --upx
```

> 若使用 CMake ≥ 4.0，脚本已自动设置 `CMAKE_POLICY_VERSION_MINIMUM=3.5`。
>
> 若系统中安装了 `upx.exe`，可加 `--upx` 压缩 EXE；压缩后的产物后缀为 `.upx.exe`（如 `udp2cal_x64.upx.exe`）。

### Android 客户端

```bat
build.bat android --modern --abi arm64-v8a --release
build.bat android --modern --abi armeabi-v7a --release
```

> **`android_old/` 已停止开发**，自 v1.1.1 起不再提供构建支持；自 v1.1.2 起 `windows/` 与 `protocol/` 已删除低性能适配，产物与当前 PC 端不互通。下述旧命令仅作历史归档保留，不保证可用。需要旧版配对产物请从 [Release v1.1.1](https://github.com/Raven777777/UDP2CAL/releases/tag/v1.1.1) 下载。

```bat
:: 归档命令，不再维护
build.bat android --old    --abi arm64-v8a --release
build.bat android --old    --abi armeabi-v7a --debug
```

`android_old/` 已停止开发；旧版源码若需自行尝试构建，方式为：

```bat
cd android_old
gradlew assembleRelease -PtargetAbi=armeabi-v7a
```

### 环境体检 / ADB 调试

```bat
build.bat doctor --fix      :: 检查并自动修复可修复项
build.bat adb --build       :: 编译 Debug APK
build.bat adb --install     :: 安装到设备
build.bat adb --monitor     :: Logcat 监控
build.bat adb --all         :: 编译 + 安装 + 清缓冲 + 监控 + 打开 Windows 日志
```

---

## 四、编译产物

| 文件 | 大小 | 说明 |
| --- | --- | --- |
| `udp2cal_{x64,x86}.exe` | ~6.3 MB | Windows 接收端（架构后缀，UPX 压缩后为 `.upx.exe`）|
| `udp2cal_{x64,x86}-debug.exe` | ~22 MB | Windows 调试版 |
| `udp2cal_arm64-v8a_Release.apk` | ~2.7 MB | Android 现代版 (64位，已签名；签名来源见下) |
| `udp2cal_armeabi-v7a_Release.apk` | ~2.7 MB | Android 现代版 (32位，已签名；签名来源见下) |
| `udp2cal_old_{arm64-v8a,armeabi-v7a}_Release.apk` | ~2.65 MB | Android Old 低性能版（**已停止开发**，历史产物，见 [Release v1.1.1](https://github.com/Raven777777/UDP2CAL/releases/tag/v1.1.1)）|

### 发布签名

仓库内**不含任何密钥库**，签名信息由 `android/local.properties` 或环境变量提供：

```properties
# android/local.properties（不入库）
RELEASE_STORE_FILE=lanmic-debug.keystore   # 相对 android/ 目录
RELEASE_STORE_PASSWORD=******
RELEASE_KEY_ALIAS=lanmic
RELEASE_KEY_PASSWORD=******
```

对应环境变量为 `UDP2CAL_STORE_FILE / UDP2CAL_STORE_PASSWORD / UDP2CAL_KEY_ALIAS / UDP2CAL_KEY_PASSWORD`。

- **已配置** → Release 使用该密钥签名，可覆盖升级旧版本（需与旧 APK 同一密钥）。
- **未配置** → 回落到本机调试密钥 `~/.android/debug.keystore`（每台机器不同、不入库），构建仍可通过且 APK 可安装，但**仅供自测**；构建日志会打印警告。
- 若设备上已装过用其他密钥签名的旧版，安装会报 `INSTALL_FAILED_UPDATE_INCOMPATIBLE`，需先 `adb uninstall com.udp2cal.app`。

---

## 五、发布产物清单

```
udp2cal_x64.exe               # Windows 接收端 64-bit
udp2cal_x64.upx.exe           # UPX 压缩版（额外产物）
udp2cal_x86.exe               # Windows 接收端 32-bit
udp2cal_x86.upx.exe           # UPX 压缩版（额外产物）
udp2cal_arm64-v8a.apk         # Android 发送端 64-bit（已签名，ProGuard 缩减）
udp2cal_armeabi-v7a.apk       # Android 发送端 32-bit（已签名，ProGuard 缩减）
icon.png                      # 图标源文件（已内嵌到 exe，分发时可选附带）
```

> `icon.png` 必须保留在项目根目录：`windows/build.rs` 在构建期将其转换为 256×256 ICO 嵌入 PE 资源，`windows/src/main.rs` 通过 `include_bytes!("../../icon.png")` 编译期嵌入窗口/托盘图标。移动该文件会导致 Windows 端构建失败。

---

## 六、构建脚本结构

```
build.bat               # 统一构建入口（Windows / Android / ADB / 体检）
scripts/
├─ build.py             # 主流程编排
└─ udp2cal/             # 环境探测、依赖补装、Gradle/Cargo 封装
```

> Python 3.8+ 运行 `scripts/build.py`；`build.bat` 仅负责定位 Python 解释器并转发参数。

---

## 七、常见构建问题

| 现象 | 原因 | 处理 |
|------|------|------|
| CMake 报 `Compatibility with CMake < 3.5 has been removed` | CMake ≥ 4.0 的兼容性策略变更 | 脚本已自动设置 `CMAKE_POLICY_VERSION_MINIMUM=3.5`；手动构建时需自行设置 |
| `icon.png` 缺失导致构建失败 | 图标被移动或重命名 | 恢复 `icon.png` 到项目根目录 |
| Android 构建找不到 SDK / NDK | `local.properties` 路径错误 | 执行 `build.bat doctor --fix` 自动修正 |
| Gradle wrapper 版本不匹配 | 本地 gradle 缓存异常 | 脚本会检查 wrapper；必要时删除 `~/.gradle/caches` 后重试 |
| `lintVitalAnalyzeRelease` 报 `IncompatibleClassChangeError: KaCallableMemberCall` | AGP 8.7 的 lint 与 Kotlin 2.0 分析 API 不兼容（Google issue 374783344），崩溃点为 `NullSafeMutableLiveData` 检查 | 已在 `app/build.gradle.kts` 禁用该检查；升级到 AGP 8.8.2+ 后可删除该行恢复 |
| `--proxy` 构建报 `'127.0.0.1' 不是内部或外部命令` | `-Dhttp.nonProxyHosts` 中的 `\|` 被 cmd 当作管道符 | 已修复（`scripts/udp2cal/proxy.py` 为该参数加引号）；旧脚本可改用 `--proxy direct` 绕过 |
