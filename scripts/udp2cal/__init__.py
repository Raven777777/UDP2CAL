"""UDP2CAL 构建工具包。

模块划分::

    config   常量、路径、镜像与 ABI 映射
    logger   彩色控制台输出 + 日志留档
    runner   子进程执行封装（流式输出、结果回传）
    proxy    系统代理探测
    env      JDK / Android SDK / Rust / CMake / MSVC / adb 探测
    deps     依赖补装与项目文件自修复
    android  Android APK 构建
    windows_build  Windows 可执行文件构建
    adb      ADB 调试流程
"""

__version__ = "1.0.0"
