"""Windows 可执行文件构建（Rust + MSVC）。

保留原 build_windows.bat 的关键环境设置：
    CMAKE_POLICY_VERSION_MINIMUM=3.5
    CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP=none
这两项是 cmake 依赖与 release 符号处理能通过的前提，不做所谓"优化"。
"""

from __future__ import annotations

import glob
import shutil
from pathlib import Path
from typing import List, Optional

from . import config, deps, env as envmod, proxy as proxymod, runner


def find_upx() -> Optional[Path]:
    """在仓库根与 PATH 中定位 upx.exe（兼容任意版本目录名）。"""
    for pattern in ("upx*/upx.exe", "upx*/upx64.exe", "upx.exe"):
        hits = sorted(glob.glob(str(config.ROOT / pattern)))
        if hits:
            return Path(hits[-1])
    found = shutil.which("upx")
    return Path(found) if found else None


def _cargo_env(java: Optional[envmod.JavaInfo], proxy) -> dict:
    env = runner.clean_env(
        {
            "CMAKE_POLICY_VERSION_MINIMUM": "3.5",
            "CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP": "none",
            "CARGO_TERM_COLOR": "always",
        },
        unset=("_JAVA_OPTIONS", "JAVA_TOOL_OPTIONS"),
    )
    proxymod.apply_env(env, proxy)
    if java is not None:
        runner.prepend_path(env, java.home / "bin")
    return env


def _mirror_args() -> List[str]:
    return [
        "--config",
        'source.crates-io.replace-with="{}"'.format(config.CARGO_MIRROR_NAME),
        "--config",
        'source.{}.registry="{}"'.format(config.CARGO_MIRROR_NAME, config.CARGO_MIRROR_REGISTRY),
    ]


def build(ctx, logger) -> int:
    arch = getattr(ctx, "arch", "x64")
    build_type = getattr(ctx, "win_type", "Release")
    triple = config.WINDOWS_TARGETS[arch]
    profile = "debug" if build_type == "Debug" else "release"

    logger.banner("UDP2CAL Windows Build")
    logger.info("Mode:   {}".format(build_type))
    logger.info("Target: {}  (_{})".format(triple, "x64" if arch == "x64" else "x86"))

    project_dir = config.WINDOWS_DIR
    if not project_dir.exists():
        logger.error("目录不存在: {}".format(project_dir))
        return 1

    # ---------------- 步骤 1：工具链
    logger.step(1, 3, "工具链检查")
    rust = envmod.detect_rust()
    if rust is None:
        logger.error("未找到 Rust 工具链")
        logger.hint("安装: https://rustup.rs  (winget install Rustlang.Rustup)")
        return 1
    logger.ok("Rust: {}".format(rust.version or rust.rustc))

    cmake_path, cmake_version = envmod.detect_cmake()
    if cmake_path is None:
        logger.error("未找到 CMake（构建脚本依赖 cmake）")
        logger.hint("安装: winget install Kitware.CMake，并确保加入 PATH")
        return 1
    logger.ok("CMake: {} ({})".format(cmake_version or "?", cmake_path))

    msvc = envmod.detect_msvc()
    if msvc is None:
        logger.warn("未检测到 MSVC 工具链，链接阶段可能失败")
        logger.hint("安装 Visual Studio Build Tools（含 C++ 生成工具）")
    else:
        logger.ok("MSVC: {}".format(msvc))

    proxy = proxymod.resolve(getattr(ctx, "proxy_mode", "direct"), logger)
    mirror_mode = getattr(ctx, "mirror_mode", "auto")

    if getattr(ctx, "install_deps", True):
        dep_env = _cargo_env(None, proxy)
        if not deps.ensure_rust_target(rust, triple, logger, env=dep_env):
            return 1
    else:
        logger.detail("已禁用依赖自动安装")

    # ---------------- 步骤 2：编译
    logger.step(2, 3, "cargo build ({})".format(build_type))
    stale = project_dir / "target" / triple / profile / "udp2cal.exe"
    if stale.exists():
        try:
            stale.unlink()
            logger.detail("已清理旧产物: target/{}/{}/udp2cal.exe".format(triple, profile))
        except OSError:
            pass

    env = _cargo_env(None, proxy)

    cmd = [str(rust.cargo), "build", "--target", triple]
    if build_type == "Release":
        cmd.append("--release")
    if mirror_mode == "force":
        cmd += _mirror_args()
        logger.info("已强制启用 Cargo 镜像: {}".format(config.CARGO_MIRROR_REGISTRY))

    result = runner.run(cmd, logger=logger, cwd=project_dir, env=env, tail_lines=250)

    if not result.ok and mirror_mode == "auto" and result.network_failed:
        logger.blank()
        logger.warn("检测到 crates.io 访问失败，切换国内镜像重试")
        result = runner.run(cmd + _mirror_args(), logger=logger, cwd=project_dir, env=env, tail_lines=250)

    if not result.ok:
        logger.blank()
        logger.error("编译失败（退出码 {}）".format(result.code))
        _print_failure_tail(result, logger)
        return result.code if result.code else 1

    # ---------------- 步骤 3：产物处理
    logger.step(3, 3, "收集产物")
    source = project_dir / "target" / triple / profile / "udp2cal.exe"
    if not source.exists():
        logger.error("未找到构建产物: {}".format(source))
        return 1

    size_kb = source.stat().st_size // 1024
    logger.ok("原始产物: {} KB".format(size_kb))

    suffix = "_x64" if arch == "x64" else "_x86"
    tag = "-debug" if build_type == "Debug" else ""
    upx_tag = ""

    use_upx = getattr(ctx, "upx", None)
    upx_path = find_upx()
    if use_upx is None:
        use_upx = False
    release_like = build_type != "Debug"

    if use_upx and release_like:
        if upx_path is None:
            logger.warn("未找到 upx.exe，跳过压缩")
        else:
            logger.info("UPX 压缩: {}".format(upx_path))
            upx_result = runner.run(
                [str(upx_path), "--best", "--quiet", str(source)],
                logger=logger,
                env=env,
                tail_lines=40,
            )
            if upx_result.ok:
                compressed_kb = source.stat().st_size // 1024
                logger.ok("UPX: {} KB -> {} KB".format(size_kb, compressed_kb))
                upx_tag = ".upx"
                size_kb = compressed_kb
            else:
                logger.warn("UPX 压缩失败，保留未压缩产物")
    elif use_upx and not release_like:
        logger.detail("Debug 构建不进行 UPX 压缩")

    output = config.ROOT / "udp2cal{}{}{}.exe".format(suffix, tag, upx_tag)
    try:
        shutil.copy2(str(source), str(output))
    except OSError as exc:
        logger.error("复制产物失败: {}".format(exc))
        return 1

    final_kb = output.stat().st_size // 1024
    logger.blank()
    from .logger import C

    logger.raw(C.GREEN + "=" * 44)
    logger.raw(C.GREEN + "   BUILD SUCCESS")
    logger.raw(C.GREEN + "   mode:   {}".format(build_type))
    logger.raw(C.GREEN + "   target: {}".format(triple))
    logger.raw(C.GREEN + "   output: {}".format(output.name))
    logger.raw(C.GREEN + "   size:   {} KB".format(final_kb))
    logger.raw(C.GREEN + "=" * 44 + C.RESET)
    return 0


def _print_failure_tail(result: runner.RunResult, logger) -> None:
    interesting = [
        line
        for line in result.lines
        if any(token in line.lower() for token in ("error", "failed", "cannot find", "undefined reference"))
    ]
    shown = interesting[-15:] if interesting else result.lines[-15:]
    if shown:
        logger.info("关键错误行:")
        for line in shown:
            logger.hint(line.strip()[:200])
    if logger.path:
        logger.info("完整日志: {}".format(logger.path))
