"""环境探测：JDK / Android SDK / Rust / CMake / MSVC / adb。

JDK 选择规则是本模块的核心：
    AGP 8.7.0 + Gradle 8.9 只能运行在 JDK 17~22 上。
    低于 17 或高于 22 的 JDK（例如 JDK 25）会被标记为不兼容并跳过，
    而不是交给 Gradle 去崩在 "Unsupported class file major version"。
"""

from __future__ import annotations

import glob
import os
import re
import shutil
from dataclasses import dataclass, field
from pathlib import Path
from typing import List, Optional

from . import config, runner

# ---------------------------------------------------------------- 数据结构


@dataclass
class JavaInfo:
    home: Path
    version: str = ""
    major: int = 0
    source: str = ""

    @property
    def java_exe(self) -> Path:
        return self.home / "bin" / "java.exe"

    @property
    def supported(self) -> bool:
        return config.JDK_MIN_MAJOR <= self.major <= config.JDK_MAX_MAJOR

    def describe(self) -> str:
        return "JDK {} @ {}".format(self.version or "?", self.home)


@dataclass
class SdkInfo:
    root: Path
    source: str = ""

    @property
    def adb(self) -> Path:
        return self.root / "platform-tools" / "adb.exe"

    def has_component(self, *parts: str) -> bool:
        return (self.root.joinpath(*parts)).exists()


@dataclass
class RustInfo:
    cargo: Path
    rustc: Path
    rustup: Optional[Path] = None
    version: str = ""


@dataclass
class Report:
    """供 doctor 汇总使用的探测结果。"""

    javas: List[JavaInfo] = field(default_factory=list)
    sdks: List[SdkInfo] = field(default_factory=list)
    rust: Optional[RustInfo] = None
    cmake: Optional[Path] = None
    cmake_version: str = ""
    msvc: Optional[Path] = None
    adb: Optional[Path] = None


# ---------------------------------------------------------------- JDK


def _parse_major(version: str) -> int:
    text = (version or "").strip()
    if not text:
        return 0
    if text.startswith("1."):  # 1.8.0_302 形式
        text = text[2:]
    head = re.split(r"[._\-+]", text)[0]
    try:
        return int(head)
    except ValueError:
        return 0


def _read_release(home: Path) -> tuple:
    release = home / "release"
    if not release.exists():
        return "", ""
    version = implementor = ""
    try:
        for line in release.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith("JAVA_VERSION="):
                version = line.split("=", 1)[1].strip().strip('"')
            elif line.startswith("IMPLEMENTOR="):
                implementor = line.split("=", 1)[1].strip().strip('"')
    except OSError:
        pass
    return version, implementor


def _java_version_via_cli(java_exe: Path) -> str:
    if not java_exe.exists():
        return ""
    result = runner.capture([str(java_exe), "-version"], timeout=20)
    text = "\n".join(result.lines)
    match = re.search(r'version\s+"([^"]+)"', text)
    return match.group(1) if match else ""


def _probe_java(home: Path, source: str) -> Optional[JavaInfo]:
    if not (home / "bin" / "java.exe").exists():
        return None
    version, _ = _read_release(home)
    if not version:
        version = _java_version_via_cli(home / "bin" / "java.exe")
    return JavaInfo(home=home, version=version, major=_parse_major(version), source=source)


def detect_javas(extra_homes: Optional[List[Path]] = None) -> List[JavaInfo]:
    """扫描本机所有可用 JDK，按版本降序返回。"""
    candidates: List[tuple] = []

    env_home = os.environ.get("JAVA_HOME", "").strip().strip('"')
    if env_home:
        candidates.append((Path(env_home), "JAVA_HOME"))

    on_path = shutil.which("java")
    if on_path:
        try:
            candidates.append((Path(on_path).resolve().parent.parent, "PATH"))
        except OSError:
            pass

    for pattern in config.JDK_SEARCH_PATTERNS:
        for hit in glob.glob(config.expand(pattern)):
            candidates.append((Path(hit), "扫描"))
    for home in extra_homes or []:
        candidates.append((Path(home), "命令行指定"))

    result: List[JavaInfo] = []
    seen = set()
    for home, source in candidates:
        try:
            key = str(home.resolve()).lower()
        except OSError:
            key = str(home).lower()
        if key in seen:
            continue
        seen.add(key)
        info = _probe_java(home, source)
        if info:
            result.append(info)

    result.sort(key=lambda j: j.major, reverse=True)
    return result


def select_java(
    logger,
    preferred: Optional[str] = None,
    candidates: Optional[List[JavaInfo]] = None,
) -> Optional[JavaInfo]:
    """选出可用的 JDK；返回 None 表示需要用户安装 JDK 17~22。"""
    pool = list(candidates) if candidates is not None else detect_javas()
    if preferred:
        forced = Path(preferred)
        info = _probe_java(forced, "命令行指定")
        if info is None:
            logger.error("--java-home 指向的目录不是有效 JDK: {}".format(forced))
            return None
        if not info.supported:
            logger.error(
                "命令行指定的 JDK {} 超出 Gradle 8.9 支持范围（{}~{}），已拒绝使用".format(
                    info.major, config.JDK_MIN_MAJOR, config.JDK_MAX_MAJOR
                )
            )
            return None
        pool = [info] + pool

    supported = [j for j in pool if j.supported]
    if supported:
        env_home = os.environ.get("JAVA_HOME", "").strip().strip('"').lower()
        for item in supported:
            if env_home and str(item.home).lower() == env_home:
                return item
        return supported[0]

    if pool:
        highest = pool[0]
        logger.warn(
            "未找到 JDK {}-{}；当前最高版本为 JDK {}，Gradle 8.9 存在启动失败风险".format(
                config.JDK_MIN_MAJOR, config.JDK_MAX_MAJOR, highest.major
            )
        )
        logger.hint("建议安装 JDK 17 或 21：winget install Microsoft.OpenJDK.21")
        if highest.major >= config.JDK_MIN_MAJOR:
            return highest
    return None


# ---------------------------------------------------------------- Android SDK


def _valid_sdk(root: Path) -> bool:
    return root.is_dir() and any((root / sub).exists() for sub in config.SDK_COMPONENT_SUBDIRS)


def detect_sdks(extra_roots: Optional[List[Path]] = None) -> List[SdkInfo]:
    """按优先级返回候选 SDK 目录。"""
    candidates: List[tuple] = []
    if extra_roots:
        for root in extra_roots:
            candidates.append((Path(root), "命令行指定"))

    for key in ("ANDROID_HOME", "ANDROID_SDK_ROOT"):
        value = os.environ.get(key, "").strip().strip('"')
        if value:
            candidates.append((Path(value), key))

    for pattern in config.SDK_SEARCH_PATTERNS:
        for hit in glob.glob(config.expand(pattern)):
            candidates.append((Path(hit), "扫描"))

    result: List[SdkInfo] = []
    seen = set()
    for root, source in candidates:
        try:
            key = str(root.resolve()).lower()
        except OSError:
            key = str(root).lower()
        if key in seen:
            continue
        seen.add(key)
        if _valid_sdk(root):
            result.append(SdkInfo(root=root, source=source))
    return result


def required_components(target_key: str) -> List[tuple]:
    """返回构建目标所需的 SDK 组件 (包名, 校验路径分段)。"""
    target = config.ANDROID_TARGETS[target_key]
    items = [
        ("platforms;{}".format(target["compile_sdk"]), ("platforms", target["compile_sdk"])),
        ("build-tools;{}".format(config.ANDROID_BUILD_TOOLS), ("build-tools", config.ANDROID_BUILD_TOOLS)),
        ("platform-tools", ("platform-tools", "adb.exe")),
        ("cmake;{}".format(config.ANDROID_CMAKE_VERSION), ("cmake", config.ANDROID_CMAKE_VERSION)),
    ]
    if target["ndk"]:
        items.append(("ndk;{}".format(target["ndk"]), ("ndk", target["ndk"])))
    return items


def missing_components(sdk: SdkInfo, target_key: str) -> List[str]:
    missing = []
    for package, parts in required_components(target_key):
        if not sdk.has_component(*parts):
            missing.append(package)
    return missing


def find_sdkmanager(sdk: SdkInfo) -> Optional[Path]:
    latest = sdk.root / "cmdline-tools" / "latest" / "bin" / "sdkmanager.bat"
    if latest.exists():
        return latest
    for pattern in ("cmdline-tools/*/bin/sdkmanager.bat", "tools/bin/sdkmanager.bat"):
        hits = sorted(glob.glob(str(sdk.root / pattern)))
        if hits:
            return Path(hits[-1])
    return None


# ---------------------------------------------------------------- 其他工具链


def detect_rust() -> Optional[RustInfo]:
    cargo = shutil.which("cargo")
    rustc = shutil.which("rustc")
    if not cargo or not rustc:
        return None
    version = ""
    result = runner.capture([rustc, "--version"], timeout=20)
    if result.ok and result.lines:
        version = result.lines[0].strip()
    return RustInfo(
        cargo=Path(cargo),
        rustc=Path(rustc),
        rustup=Path(shutil.which("rustup")) if shutil.which("rustup") else None,
        version=version,
    )


def installed_rust_targets(rust: RustInfo) -> List[str]:
    if not rust.rustup:
        return []
    result = runner.capture([str(rust.rustup), "target", "list", "--installed"], timeout=30)
    if not result.ok:
        return []
    return [line.strip() for line in result.lines if line.strip()]


def detect_cmake() -> tuple:
    found = shutil.which("cmake")
    if not found:
        return None, ""
    result = runner.capture([found, "--version"], timeout=20)
    version = ""
    if result.lines:
        match = re.search(r"cmake version ([\d.]+)", result.lines[0], re.IGNORECASE)
        if match:
            version = match.group(1)
    return Path(found), version


def detect_msvc() -> Optional[Path]:
    """通过 vswhere 或目录扫描定位 MSVC 工具链。"""
    vswhere = Path(r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe")
    if vswhere.exists():
        result = runner.capture(
            [
                str(vswhere),
                "-latest",
                "-products",
                "*",
                "-requires",
                "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                "-property",
                "installationPath",
            ],
            timeout=30,
        )
        if result.ok and result.lines and result.lines[0].strip():
            vc_root = Path(result.lines[0].strip()) / "VC" / "Tools" / "MSVC"
            hits = sorted(vc_root.glob("*/bin/Hostx64/x64/cl.exe"))
            if hits:
                return hits[-1].parent

    for pattern in (
        r"C:\Program Files\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Hostx64\x64\cl.exe",
        r"C:\Program Files (x86)\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Hostx64\x64\cl.exe",
    ):
        hits = sorted(glob.glob(pattern))
        if hits:
            return Path(hits[-1]).parent
    return None


def detect_adb(sdk: Optional[SdkInfo] = None, override: Optional[str] = None) -> Optional[Path]:
    if override:
        candidate = Path(override)
        return candidate if candidate.exists() else None
    if sdk and sdk.adb.exists():
        return sdk.adb
    found = shutil.which("adb")
    return Path(found) if found else None


# ---------------------------------------------------------------- 汇总


def collect(logger=None) -> Report:
    report = Report()
    report.javas = detect_javas()
    report.sdks = detect_sdks()
    report.rust = detect_rust()
    report.cmake, report.cmake_version = detect_cmake()
    report.msvc = detect_msvc()
    report.adb = detect_adb(report.sdks[0] if report.sdks else None)
    return report


def describe_javas(logger, javas: List[JavaInfo]) -> None:
    if not javas:
        logger.error("未找到任何 JDK")
        logger.hint("安装 JDK 21：winget install Microsoft.OpenJDK.21")
        return
    for item in javas:
        if item.supported:
            logger.ok("{}  [可用]".format(item.describe()))
        else:
            reason = "低于下限" if item.major < config.JDK_MIN_MAJOR else "超出 Gradle 8.9 上限（最高支持 22）"
            logger.warn("{}  [跳过: {}]".format(item.describe(), reason))
