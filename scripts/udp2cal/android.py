"""Android APK 构建。

流程：JDK 筛选 → SDK 定位 → 依赖自修复 → Gradle 构建（直连失败自动切镜像）。
"""

from __future__ import annotations

import shutil
from pathlib import Path
from typing import List, Optional

from . import config, deps, env as envmod, proxy as proxymod, runner

_INIT_SCRIPT = """// 由 scripts/build.py 自动生成：直连官方仓库失败时切换到国内镜像
// 该文件为临时产物，每次构建覆盖，不参与版本管理。

def mirrorUrls = [
{mirrors}
]

settingsEvaluated {{ settings ->
    def rewrite = {{ handler ->
        def previous = handler.toList()
        handler.clear()
        mirrorUrls.each {{ url ->
            handler.maven {{ repo ->
                repo.name = 'mirror_' + Math.abs(url.hashCode())
                repo.url = url
            }}
        }}
        previous.each {{ handler.add(it) }}
    }}
    try {{
        rewrite(settings.pluginManagement.repositories)
        rewrite(settings.dependencyResolutionManagement.repositories)
    }} catch (Throwable ignored) {{
        // 镜像注入失败不应阻断构建
    }}
}}
"""


def _write_init_script(logger) -> Optional[Path]:
    """生成镜像 init 脚本，返回路径。"""
    config.CACHE_DIR.mkdir(parents=True, exist_ok=True)
    path = config.CACHE_DIR / "init-mirrors.gradle"
    lines = "\n".join("    '{}',".format(url) for _, url in config.MAVEN_MIRRORS)
    try:
        path.write_text(_INIT_SCRIPT.format(mirrors=lines), encoding="utf-8")
    except OSError as exc:
        logger.warn("无法写入镜像配置脚本: {}".format(exc))
        return None
    return path


def _gradle_env(java: envmod.JavaInfo, sdk: envmod.SdkInfo, proxy, mirror_script: Optional[Path]) -> dict:
    jvm_args = proxymod.jvm_args(proxy)
    gradle_opts = ["-Dfile.encoding=UTF-8"] + jvm_args
    env = runner.clean_env(
        {
            "JAVA_HOME": str(java.home),
            "ANDROID_HOME": str(sdk.root),
            "ANDROID_SDK_ROOT": str(sdk.root),
            "GRADLE_OPTS": " ".join(gradle_opts),
            "GRADLE_USER_HOME": _gradle_home(),
        },
        unset=("_JAVA_OPTIONS", "JAVA_TOOL_OPTIONS"),
    )
    proxymod.apply_env(env, proxy)
    if mirror_script is not None:
        env["UDP2CAL_MIRROR_INIT"] = str(mirror_script)
    return runner.prepend_path(env, java.home / "bin", sdk.root / "platform-tools", sdk.root / "cmake" / config.ANDROID_CMAKE_VERSION / "bin")


def _tool_env(base: dict, proxy) -> dict:
    """给 sdkmanager 等 Java 工具注入代理（Gradle 自身用 GRADLE_OPTS）。"""
    if proxy is not None:
        base["JAVA_TOOL_OPTIONS"] = " ".join(proxymod.jvm_args(proxy))
    return base


def _gradle_home() -> str:
    """沿用当前 Gradle 用户目录，避免重复下载发行版。"""
    import os

    return os.environ.get("GRADLE_USER_HOME") or str(Path.home() / ".gradle")


def find_apk(project_dir: Path, build_type: str) -> Optional[Path]:
    """在 AGP 的输出目录中定位目标 APK（目录名大小写因 AGP 版本而异）。"""
    apk_root = project_dir / "app" / "build" / "outputs" / "apk"
    if not apk_root.exists():
        return None
    lowered = build_type.lower()
    candidates: List[Path] = []
    for path in apk_root.rglob("*.apk"):
        parent = path.parent.name.lower()
        name = path.name.lower()
        if lowered in parent or lowered in name:
            candidates.append(path)
    if not candidates:
        candidates = sorted(apk_root.rglob("*.apk"), key=lambda p: p.stat().st_mtime, reverse=True)
    if not candidates:
        return None
    return max(candidates, key=lambda p: p.stat().st_mtime)


def preflight(ctx, logger) -> Optional[dict]:
    """公共前置检查，返回 {java, sdk} 或 None。"""
    javas = envmod.detect_javas()
    java = envmod.select_java(logger, preferred=ctx.java_home, candidates=javas)
    if java is None:
        logger.error("未找到可用的 JDK（需要 {}-{}）".format(config.JDK_MIN_MAJOR, config.JDK_MAX_MAJOR))
        return None
    if java.supported:
        logger.ok("Java: {}".format(java.describe()))
    else:
        logger.warn("Java: {} （超出支持区间，构建可能失败）".format(java.describe()))

    roots = [Path(ctx.sdk_dir)] if getattr(ctx, "sdk_dir", None) else []
    sdks = envmod.detect_sdks(roots)
    if not sdks:
        logger.error("未找到 Android SDK")
        logger.hint("设置环境变量 ANDROID_HOME，或在 --sdk-dir 中指定路径")
        return None
    sdk = sdks[0]
    logger.ok("SDK: {}".format(sdk.root))

    if java.supported and java.major < 21:
        logger.detail("提示: Gradle 8.9 在 JDK 17/21 上均可运行")
    return {"java": java, "sdk": sdk}


def build(ctx, logger) -> int:
    target = config.ANDROID_TARGETS[ctx.target]
    project_dir: Path = target["dir"]
    build_type = ctx.build_type
    logger.banner("UDP2CAL Android Build")
    logger.info("Project: {}  ({})".format(project_dir.name, target["label"]))
    logger.info("ABI:     {}".format(ctx.abi))
    logger.info("Type:    {}".format(build_type))
    if not project_dir.exists():
        logger.error("项目目录不存在: {}".format(project_dir))
        return 1

    logger.step(1, 3, "依赖自修复与组件校验")
    base = preflight(ctx, logger)
    if base is None:
        return 1
    java, sdk = base["java"], base["sdk"]

    proxy = proxymod.resolve(getattr(ctx, "proxy_mode", "direct"), logger)
    mirror_mode = getattr(ctx, "mirror_mode", "auto")

    if not deps.ensure_gradle_wrapper(project_dir, logger):
        return 1
    deps.ensure_local_properties(project_dir, sdk, logger)

    if getattr(ctx, "install_deps", True):
        tool_env = _tool_env(_gradle_env(java, sdk, proxy, None), proxy)
        if not deps.ensure_sdk_components(sdk, ctx.target, logger, env=tool_env):
            logger.warn("组件校验未通过，仍尝试构建（Gradle 可能自行下载）")
    else:
        logger.detail("已禁用依赖自动安装")

    # 协议层已合并为一份 Rust 实现，Gradle 在 preBuild 交叉编译 `protocol/`；
    # 缺工具链时提前给出能看懂的错误，而不是让 Gradle 报进程启动失败。
    rust = envmod.detect_rust()
    if rust is None:
        logger.error("未找到 Rust 工具链（Android 端协议层需把 protocol/ 编成 libudp2cal_protocol.so）")
        logger.hint("安装: https://rustup.rs  (winget install Rustlang.Rustup)")
        return 1
    logger.ok("Rust: {}".format(rust.version or rust.cargo))
    rust_triple = config.ANDROID_RUST_TARGETS.get(ctx.abi, "aarch64-linux-android")
    if not deps.ensure_rust_target(rust, rust_triple, logger, env=None):
        logger.hint("也可手动执行: rustup target add {}".format(rust_triple))
        return 1
    logger.step(2, 3, "Gradle assemble{} ({})".format(build_type, ctx.abi))
    task = "assemble{}".format(build_type)
    base_cmd = ["gradlew.bat", task, "-PtargetAbi={}".format(ctx.abi), "--console=plain"]

    init_script: Optional[Path] = None
    if mirror_mode == "force":
        init_script = _write_init_script(logger)
        if init_script:
            logger.info("已强制启用镜像: {}".format(config.MAVEN_MIRRORS[0][1]))
            base_cmd += ["-I", str(init_script)]

    env = _gradle_env(java, sdk, proxy, init_script)
    result = runner.run(base_cmd, logger=logger, cwd=project_dir, env=env, tail_lines=250)

    if not result.ok and mirror_mode == "auto" and result.network_failed:
        logger.blank()
        logger.warn("检测到官方仓库访问失败，切换国内镜像重试")
        init_script = _write_init_script(logger)
        if init_script:
            retry_cmd = base_cmd + ["-I", str(init_script)]
            env = _gradle_env(java, sdk, proxy, init_script)
            result = runner.run(retry_cmd, logger=logger, cwd=project_dir, env=env, tail_lines=250)

    if not result.ok:
        logger.blank()
        logger.error("构建失败（退出码 {}）".format(result.code))
        _print_failure_tail(result, logger)
        return result.code if result.code else 1

    logger.step(3, 3, "收集产物")
    apk = find_apk(project_dir, build_type)
    if apk is None:
        logger.error("构建成功但未找到 APK 产物")
        logger.hint("检查目录: {}".format(project_dir / "app" / "build" / "outputs" / "apk"))
        return 1

    output = config.ROOT / "{}_{}_{}.apk".format(target["prefix"], ctx.abi, build_type)
    try:
        shutil.copy2(str(apk), str(output))
    except OSError as exc:
        logger.error("复制产物失败: {}".format(exc))
        return 1

    size_kb = output.stat().st_size // 1024
    logger.ok("{} ({} KB)".format(output.name, size_kb))
    logger.blank()
    _success_box(logger, target, ctx.abi, build_type, output.name, size_kb)
    return 0


def _success_box(logger, target: dict, abi: str, build_type: str, name: str, size_kb: int) -> None:
    from .logger import C

    lines = [
        "BUILD SUCCESS",
        "project: {}".format(target["label"]),
        "abi:     {}".format(abi),
        "type:    {}".format(build_type),
        "output:  {}".format(name),
        "size:    {} KB".format(size_kb),
    ]
    logger.raw(C.GREEN + "=" * 44)
    for line in lines:
        logger.raw(C.GREEN + "   " + line)
    logger.raw(C.GREEN + "=" * 44 + C.RESET)


def _print_failure_tail(result: runner.RunResult, logger) -> None:
    interesting = [
        line
        for line in result.lines
        if any(token in line.lower() for token in ("error", "failed", "caused by", "what went wrong", "exception"))
    ]
    shown = interesting[-15:] if interesting else result.lines[-15:]
    if shown:
        logger.info("关键错误行:")
        for line in shown:
            logger.hint(line.strip()[:200])
    if logger.path:
        logger.info("完整日志: {}".format(logger.path))
