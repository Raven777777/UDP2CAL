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
| **Android** | **Android 10 (API 29)+** | arm64-v8a (64-bit) / armeabi-v7a (32-bit) | minSdk=29, targetSdk=35, 需要 `RECORD_AUDIO` 权限。Jetpack Compose UI |
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

`--modern` 为默认且唯一目标（`android/`）。

### Opus 源码共用（Windows / Android 同源）

两端均从仓库根目录的 `opus-src/`（Opus 1.6.1）构建 libopus，不再使用 `audiopus_sys` crate 内置的 Opus 1.3.1：

- **Android**：`android/app/src/main/cpp/CMakeLists.txt` 通过 `add_subdirectory(../../../../../opus-src)` 直接构建；OSCE / DRED / QEXT 等 ML 增强特性保持默认关闭。
- **Windows**：`windows/Cargo.toml` 的 `[patch.crates-io]` 将 `audiopus_sys` 重定向到 `vendor/audiopus_sys` 副本（**该目录必须入库**，patch 依赖本地路径）。其 build.rs 解析 Opus 源码的优先级：
  1. `OPUS_SOURCE_DIR` 环境变量（显式覆盖）
  2. `<repo>/opus-src`（默认，自动解析，日常构建无需任何设置）
  3. `./opus`（上游兜底，本副本中不存在）
- Windows 端 CMake 构建配置**强制 Release**：dev profile 下 CMake 默认 Debug（/MDd 调试 CRT），Opus 1.6.x 会引入 `__imp__CrtDbgReportW` 等调试 CRT 符号，与 rustc 链接的 /MD 发行版 CRT 不兼容，导致 `LNK2001/LNK1120` 链接失败。
- `audiopus` 0.3.0-rc.0 的预生成绑定兼容 1.6.1 的 C ABI；1.6 新增 API（Opus HD 等）未暴露，且本项目不使用。

### Opus ML 特性（Deep PLC + DRED）

两端均以 `OPUS_DEEP_PLC=ON` + `OPUS_DRED=ON` 构建（OSCE / QEXT 保持关闭），产物体积各增约 2 MB：

| 特性 | 生效位置 | 运行时条件 | 说明 |
|------|---------|-----------|------|
| 经典 PLC | 解码端 | 播放头到期 | 缺口在 `next_ts <= playhead_ts + PLC_LEAD_MS` 时补一帧 480 采样；连续 12 帧后转静音但仍留在 PLAYING。**没有**丢包计时器、没有兜底定时器 —— 触发点由声卡播放头决定（协议 7.8） |
| 抖动缓冲 | `protocol/rtp.rs` | 自动 | 固定 64 槽、按扩展时间戳索引；序号只用于重复检测。v3 **不再**动态扩缩重排窗口 |
| **Wi-Fi 省电锁（Android）** | CaptureService | 采集期间 | 采集期间持有 `WIFI_MODE_FULL_LOW_LATENCY` WifiLock，阻止网卡进入省电聚合（周期性 100~500ms 延迟簇的根源），停止采集释放 |
| Deep PLC | 解码端 | 解码复杂度 ≥ 5 | 两端在解码器创建时设置（Windows `OpusDecoder::new()`、Android `opus_jni_decoder.c`）；v3 固定单解码器 48 kHz 单声道 |
| DRED | 编码端→解码端 | FEC 开启时激活 | 编码器附带最长 500 ms（50×10ms 帧）深度冗余，码流 +12~32 kb/s；Windows 反向编码器与 Android 编码器均跟随 FEC 开关（`fec ? 50 : 0`）。1.5 与 1.6 的 DRED 互不兼容，两端同为 1.6.1 无此问题 |

DRED 数据在 Opus 码流内传输，`protocol` 层与旧版本兼容（旧解码端忽略冗余层）。

`audiopus` 副本（`vendor/audiopus`）为支持上述特性做了本地扩展：`Decoder::set_complexity`（1.5 新增的解码器复杂度接口）与 `Encoder::set_dred_duration`（1.3.1 头文件无此请求码 4050，本地定义）。

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
| `udp2cal_arm64-v8a_Release.apk` | ~2.7 MB | Android 客户端 (64位，已签名；签名来源见下) |
| `udp2cal_armeabi-v7a_Release.apk` | ~2.7 MB | Android 客户端 (32位，已签名；签名来源见下) |

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
