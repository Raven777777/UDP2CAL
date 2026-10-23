"""全局常量：路径、构建目标、镜像源、工具链搜索位置。

所有路径均基于仓库根目录推导，脚本可从任意工作目录调用。
"""

from __future__ import annotations

import os
from pathlib import Path

# ---------------------------------------------------------------- 路径

SCRIPTS_DIR = Path(__file__).resolve().parents[1]
ROOT = SCRIPTS_DIR.parent
LOG_DIR = SCRIPTS_DIR / "logs"
CACHE_DIR = SCRIPTS_DIR / ".cache"

# ---------------------------------------------------------------- Android 构建目标

ANDROID_TARGETS = {
    "modern": {
        "key": "modern",
        "dir": ROOT / "android",
        "prefix": "udp2cal",
        "label": "Modern (minSdk 29 / Android 10+)",
        "compile_sdk": "android-35",
        # AGP 8.7 的默认 NDK 版本；项目启用了 externalNativeBuild，必须存在
        "ndk": "27.0.12077973",
    },
}

ANDROID_ABIS = ("arm64-v8a", "armeabi-v7a")
ANDROID_BUILD_TYPES = ("Release", "Debug")
ANDROID_BUILD_TOOLS = "35.0.0"
ANDROID_CMAKE_VERSION = "3.22.1"

# ---------------------------------------------------------------- Windows 构建

WINDOWS_DIR = ROOT / "windows"
WINDOWS_TARGETS = {
    "x64": "x86_64-pc-windows-msvc",
    "x86": "i686-pc-windows-msvc",
}
WINDOWS_BUILD_TYPES = ("Release", "Debug")

# ---------------------------------------------------------------- JDK 约束
#
# AGP 8.7.0 + Gradle 8.9 的实际支持区间：
#   - Gradle 8.9 最高支持 Java 22，Java 23+ 无法启动
#   - AGP 8.7 要求至少 JDK 17
# 因此只接受 17 <= major <= 22，JDK 25 会被显式跳过。

JDK_MIN_MAJOR = 17
JDK_MAX_MAJOR = 22

JDK_SEARCH_PATTERNS = (
    r"C:\Program Files\Microsoft\jdk-*",
    r"C:\Program Files\Java\jdk-*",
    r"C:\Program Files\Java\jdk1.*",
    r"C:\Program Files\Eclipse Adoptium\jdk-*",
    r"C:\Program Files\Zulu\zulu-*",
    r"C:\Program Files\BellSoft\LibericaJDK-*",
    r"C:\Program Files\Amazon Corretto\jdk*",
    r"%LOCALAPPDATA%\Programs\Eclipse Adoptium\jdk-*",
    r"%LOCALAPPDATA%\Programs\Microsoft\jdk-*",
    r"%USERPROFILE%\.jdks\*",
    # Android Studio 自带 JBR 也一并探测（可能高于受支持上限，由版本过滤排除）
    r"C:\Program Files\Android\Android Studio\jbr",
)

# ---------------------------------------------------------------- Android SDK 搜索位置

SDK_SEARCH_PATTERNS = (
    r"%LOCALAPPDATA%\Android\Sdk",
    r"%USERPROFILE%\AppData\Local\Android\Sdk",
    r"C:\Android\sdk",
    r"C:\Android\Sdk",
    r"C:\Program Files\Android\sdk",
)

SDK_COMPONENT_SUBDIRS = ("platforms", "build-tools", "cmdline-tools", "platform-tools")

# ---------------------------------------------------------------- 代理

# 常见本地代理监听端口（按使用频率排序），用于系统未配置代理时兜底探测
COMMON_PROXY_PORTS = (10808, 10809, 7890, 7897, 1080, 8889, 2080, 33210, 8080)

PROXY_ENV_KEYS = ("HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy")

# ---------------------------------------------------------------- 镜像

# Maven 镜像按优先级排列；仅在直连失败时注入
MAVEN_MIRRORS = (
    ("aliyun-google", "https://maven.aliyun.com/repository/google"),
    ("aliyun-public", "https://maven.aliyun.com/repository/public"),
    ("aliyun-central", "https://maven.aliyun.com/repository/central"),
    ("aliyun-gradle-plugin", "https://maven.aliyun.com/repository/gradle-plugin"),
    ("tencent-public", "https://mirrors.cloud.tencent.com/nexus/repository/maven-public/"),
)

# Cargo 镜像通过 `cargo --config` 注入，不修改任何全局配置文件
CARGO_MIRROR_NAME = "udp2cal"
CARGO_MIRROR_REGISTRY = "sparse+https://rsproxy.cn/index/"

# 直连失败的特征串（小写匹配），用于决定是否切换镜像重试
NETWORK_FAILURE_MARKERS = (
    "could not resolve",
    "could not get",
    "connection timed out",
    "read timed out",
    "connect timed out",
    "connection refused",
    "network is unreachable",
    "no route to host",
    "spurious network error",
    "failed to download",
    "ssl handshake",
    "remote host terminated",
    "temporary failure in name resolution",
    "unknownhostexception",
    "unable to access",
    "timeout was reached",
)

# 判定构建失败的辅助特征（用于错误摘要）
BUILD_FAILURE_MARKERS = (
    "build failed",
    "error:",
    "cannot find symbol",
    "caused by:",
    "cmake error",
    "what went wrong",
    "execution failed for task",
)


def expand(path_pattern: str) -> str:
    """展开路径中的 Windows 环境变量（含 %VAR% 语法）。"""
    return os.path.expandvars(path_pattern)
