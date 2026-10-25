"""UDP2CAL 统一构建入口。

两种工作方式：
    无参数        交互式菜单（等价原 build_android.bat / build_windows.bat / debug_adb.bat）
    带参数        纯命令行（适合批处理与 CI），例如：
                  build.bat android --modern --abi arm64-v8a --release
                  build.bat win --x64 --release --upx
                  build.bat adb --all

本文件不依赖第三方库，Python 3.8+ 可直接运行。
"""

from __future__ import annotations

import argparse
import platform
import sys
from pathlib import Path

_SCRIPTS_DIR = Path(__file__).resolve().parent
if str(_SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(_SCRIPTS_DIR))

import udp2cal  # noqa: E402
from udp2cal import adb as adbmod  # noqa: E402
from udp2cal import android as androidmod  # noqa: E402
from udp2cal import config, deps  # noqa: E402
from udp2cal import env as envmod  # noqa: E402
from udp2cal import proxy as proxymod  # noqa: E402
from udp2cal import windows_build as winmod  # noqa: E402
from udp2cal.logger import C, Logger  # noqa: E402


# ---------------------------------------------------------------- 公共

def make_logger(name: str, verbose: bool) -> Logger:
    return Logger(name=name, log_dir=config.LOG_DIR, verbose=verbose)


def _apply_common(ctx, args) -> None:
    ctx.proxy_mode = getattr(args, "proxy", "direct")
    ctx.mirror_mode = getattr(args, "mirror", "auto")
    ctx.install_deps = not getattr(args, "no_install_deps", False)
    ctx.java_home = getattr(args, "java_home", None)
    ctx.sdk_dir = getattr(args, "sdk_dir", None)
    ctx.adb_path = getattr(args, "adb", None)
    ctx.verbose = getattr(args, "verbose", False)


class Ctx:
    """轻量的参数容器（替代 argparse.Namespace，便于交互模式复用）。"""

    def __init__(self, **kwargs) -> None:
        self.proxy_mode = "direct"
        self.mirror_mode = "auto"
        self.install_deps = True
        self.java_home = None
        self.sdk_dir = None
        self.adb_path = None
        self.verbose = False
        self.__dict__.update(kwargs)


# ---------------------------------------------------------------- doctor

def _check_projects(logger) -> int:
    """检查各 Android 工程目录的完整性，返回问题数（doctor 与 --fix 复检共用）"""
    problems = 0
    for key, target in config.ANDROID_TARGETS.items():
        project = Path(target["dir"])
        if not project.exists():
            logger.error("{} 目录缺失: {}".format(key, project))
            problems += 1
            continue
        if deps.wrapper_ready(project):
            logger.ok("{} gradle wrapper 完整".format(project.name))
        else:
            logger.warn("{} 缺少 gradle/wrapper".format(project.name))
            problems += 1
        sdk_dir_value = deps.read_sdk_dir(project)
        if sdk_dir_value is None:
            logger.warn("{} local.properties 未设置 sdk.dir".format(project.name))
            problems += 1
        elif not Path(sdk_dir_value).is_dir():
            logger.warn("{} local.properties 的 sdk.dir 无效: {}".format(project.name, sdk_dir_value))
            problems += 1
        else:
            logger.ok("{} sdk.dir 有效".format(project.name))
    return problems


def doctor(args) -> int:
    logger = make_logger("doctor", getattr(args, "verbose", False))
    logger.banner("UDP2CAL 环境体检")

    logger.section("运行时")
    logger.info("Python  {}  ({})".format(platform.python_version(), sys.executable))
    logger.info("系统    {} {}".format(platform.system(), platform.release()))
    if sys.version_info < (3, 8):
        logger.error("Python 版本过低，需要 3.8+")
        return 1

    logger.section("代理")
    proxymod.report(logger)

    logger.section("JDK（受支持区间 {}-{}）".format(config.JDK_MIN_MAJOR, config.JDK_MAX_MAJOR))
    javas = envmod.detect_javas()
    envmod.describe_javas(logger, javas)
    chosen = envmod.select_java(logger, preferred=getattr(args, "java_home", None), candidates=javas)
    if chosen and chosen.supported:
        logger.info("将使用: {}".format(chosen.describe()))
    elif chosen:
        logger.warn("将降级使用: {}（不保证可构建）".format(chosen.describe()))

    logger.section("Android SDK")
    roots = [Path(args.sdk_dir)] if getattr(args, "sdk_dir", None) else []
    sdks = envmod.detect_sdks(roots)
    if not sdks:
        logger.error("未找到 Android SDK")
        logger.hint("设置 ANDROID_HOME 或使用 --sdk-dir 指定")
    else:
        for sdk in sdks:
            logger.ok("{}  (来源: {})".format(sdk.root, sdk.source))
        primary = sdks[0]
        for key in config.ANDROID_TARGETS:
            missing = envmod.missing_components(primary, key)
            label = config.ANDROID_TARGETS[key]["label"]
            if missing:
                logger.warn("{} 缺少: {}".format(label, ", ".join(missing)))
            else:
                logger.ok("{} 组件齐全".format(label))

    logger.section("Windows 工具链")
    rust = envmod.detect_rust()
    if rust:
        logger.ok("Rust  {}".format(rust.version or rust.rustc))
        if rust.rustup:
            targets = envmod.installed_rust_targets(rust)
            for arch, triple in config.WINDOWS_TARGETS.items():
                if triple in targets:
                    logger.ok("target {} 已安装".format(triple))
                else:
                    logger.warn("target {} 未安装（可用 doctor --fix 补齐）".format(triple))
    else:
        logger.error("未找到 Rust（https://rustup.rs）")

    cmake_path, cmake_version = envmod.detect_cmake()
    if cmake_path:
        logger.ok("CMake {} ({})".format(cmake_version or "?", cmake_path))
    else:
        logger.error("未找到 CMake")

    msvc = envmod.detect_msvc()
    if msvc:
        logger.ok("MSVC  {}".format(msvc))
    else:
        logger.warn("未检测到 MSVC 工具链")

    adb = envmod.detect_adb(sdks[0] if sdks else None, getattr(args, "adb", None))
    if adb:
        logger.ok("adb   {}".format(adb))
    else:
        logger.warn("未找到 adb（platform-tools 未安装？）")

    logger.section("项目自检")
    problems = _check_projects(logger)

    if getattr(args, "fix", False):
        logger.section("自动修复")
        if not sdks:
            logger.error("缺少 SDK，无法修复 local.properties")
        else:
            for target in config.ANDROID_TARGETS.values():
                project = Path(target["dir"])
                if not project.exists():
                    continue
                deps.ensure_gradle_wrapper(project, logger)
                deps.ensure_local_properties(project, sdks[0], logger)
        if rust and rust.rustup:
            for triple in config.WINDOWS_TARGETS.values():
                deps.ensure_rust_target(rust, triple, logger)
        # 修复后必须复检：否则摘要恒为"发现 N 项待处理"，与实际情况不符
        logger.section("修复后复检")
        problems = _check_projects(logger)

    logger.blank()
    if problems == 0:
        logger.ok("体检完成，未发现阻塞项")
    elif getattr(args, "fix", False):
        logger.warn("体检完成，仍有 {} 项无法自动修复，请按上方提示手动处理".format(problems))
    else:
        logger.warn("体检完成，发现 {} 项待处理；可执行 `build.bat doctor --fix`".format(problems))
    logger.info("日志: {}".format(logger.path))
    logger.close()
    return 0


# ---------------------------------------------------------------- 子命令

def cmd_win(args) -> int:
    logger = make_logger("windows", getattr(args, "verbose", False))
    ctx = Ctx()
    _apply_common(ctx, args)
    ctx.arch = "x86" if getattr(args, "x86", False) else "x64"
    ctx.win_type = "Debug" if getattr(args, "debug", False) else "Release"
    ctx.upx = True if getattr(args, "upx", False) else False
    try:
        return winmod.build(ctx, logger)
    finally:
        _finish(logger)


def cmd_android(args) -> int:
    logger = make_logger("android", getattr(args, "verbose", False))
    ctx = Ctx()
    _apply_common(ctx, args)
    ctx.target = "modern"
    ctx.abi = getattr(args, "abi", None) or "arm64-v8a"
    ctx.build_type = "Debug" if getattr(args, "debug", False) else "Release"
    try:
        return androidmod.build(ctx, logger)
    finally:
        _finish(logger)


def cmd_adb(args) -> int:
    logger = make_logger("adb", getattr(args, "verbose", False))
    ctx = Ctx()
    _apply_common(ctx, args)
    ctx.target = "modern"
    ctx.abi = getattr(args, "abi", None) or "arm64-v8a"
    ctx.adb_build = getattr(args, "build", False)
    ctx.adb_install = getattr(args, "install", False)
    ctx.adb_clear = getattr(args, "clear", False)
    ctx.adb_monitor = getattr(args, "monitor", False)
    ctx.adb_winlog = getattr(args, "winlog", False)
    ctx.adb_all = getattr(args, "all", False)
    try:
        return adbmod.run_adb(ctx, logger)
    finally:
        _finish(logger)


def _finish(logger: Logger) -> None:
    path = logger.path
    logger.close()
    if path:
        print("\n日志: {}".format(path))


# ---------------------------------------------------------------- 交互菜单

def _ask(prompt: str, valid, default=None) -> str:
    while True:
        try:
            raw = input(prompt).strip()
        except (EOFError, KeyboardInterrupt):
            print()
            raise SystemExit(0)
        if not raw and default is not None:
            return default
        if raw in valid:
            return raw
        print("  输入无效，请从 {} 中选择".format("/".join(valid)))


def _pause() -> None:
    try:
        input("\n按回车键继续...")
    except (EOFError, KeyboardInterrupt):
        print()


def interactive() -> int:
    # 用不带日志文件的 Logger 输出，重定向到管道时自动去掉颜色
    ui = Logger(name="menu", log_dir=None)
    ui.raw("")
    ui.raw(C.CYAN + "=" * 44 + C.RESET)
    ui.raw(C.CYAN + "   UDP2CAL 统一构建入口" + C.RESET)
    ui.raw(C.CYAN + "=" * 44 + C.RESET)
    ui.raw("")
    ui.raw("  [1] Windows 构建 (Rust / MSVC)")
    ui.raw("  [2] Android APK 构建 (Gradle)")
    ui.raw("  [3] ADB 调试")
    ui.raw("  [4] 环境体检")
    ui.raw("  [0] 退出")
    ui.raw("")
    choice = _ask("选择 [0-4]: ", ("0", "1", "2", "3", "4"), default="0")

    if choice == "0":
        return 0
    if choice == "1":
        return _menu_windows()
    if choice == "2":
        return _menu_android()
    if choice == "3":
        return _menu_adb()
    return _menu_doctor()


def _menu_windows() -> int:
    print()
    print("构建模式:")
    print("  [1] Release  (优化，体积小)")
    print("  [2] Debug    (含调试信息)")
    mode = _ask("[1/2]: ", ("1", "2"), default="1")

    print()
    print("目标架构:")
    print("  [1] x86_64  (64 位)")
    print("  [2] i686    (32 位)")
    arch = _ask("[1/2]: ", ("1", "2"), default="1")

    upx = False
    upx_path = winmod.find_upx()
    if upx_path and mode == "1":
        print()
        print("检测到 UPX: {}".format(upx_path))
        print("  [1] 压缩可执行文件")
        print("  [2] 不压缩")
        upx = _ask("[1/2]: ", ("1", "2"), default="2") == "1"

    ctx = Ctx(arch="x86" if arch == "2" else "x64", win_type="Debug" if mode == "2" else "Release", upx=upx)
    logger = make_logger("windows", False)
    try:
        code = winmod.build(ctx, logger)
    finally:
        _finish(logger)
    _pause()
    return code


def _menu_android() -> int:
    print()
    print("选择 ABI 与构建类型:")
    print("  [1] arm64-v8a   (64 位, Release)")
    print("  [2] armeabi-v7a (32 位, Release)")
    print("  [3] arm64-v8a   (64 位, Debug)")
    print("  [4] armeabi-v7a (32 位, Debug)")
    print()
    mapping = {
        "1": ("modern", "arm64-v8a", "Release"),
        "2": ("modern", "armeabi-v7a", "Release"),
        "3": ("modern", "arm64-v8a", "Debug"),
        "4": ("modern", "armeabi-v7a", "Debug"),
    }
    choice = _ask("选择 [1-4]: ", tuple(mapping), default="1")
    target, abi, build_type = mapping[choice]

    ctx = Ctx(target=target, abi=abi, build_type=build_type)
    logger = make_logger("android", False)
    try:
        code = androidmod.build(ctx, logger)
    finally:
        _finish(logger)
    _pause()
    return code


def _menu_adb() -> int:
    print()
    print("ADB 操作:")
    print("  [1] 编译 Debug APK")
    print("  [2] 安装 APK 到设备")
    print("  [3] 清空 Logcat 缓冲")
    print("  [4] 启动 Logcat 监控")
    print("  [5] 打开 Windows 调试日志")
    print("  [9] 全部执行")
    choice = _ask("选择: ", ("1", "2", "3", "4", "5", "9"), default="9")

    ctx = Ctx(target="modern", abi="arm64-v8a")
    ctx.adb_build = choice in ("1", "9")
    ctx.adb_install = choice in ("2", "9")
    ctx.adb_clear = choice in ("3", "9")
    ctx.adb_monitor = choice in ("4", "9")
    ctx.adb_winlog = choice in ("5", "9")
    ctx.adb_all = choice == "9"

    logger = make_logger("adb", False)
    try:
        code = adbmod.run_adb(ctx, logger)
    finally:
        _finish(logger)
    _pause()
    return code


def _menu_doctor() -> int:
    args = Ctx(verbose=False, fix=False)
    code = doctor(args)
    _pause()
    return code


# ---------------------------------------------------------------- 参数解析

def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="build",
        description="UDP2CAL 统一构建入口",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "示例:\n"
            "  build.bat                                   交互菜单\n"
            "  build.bat android --modern --abi arm64-v8a --release\n"
            "  build.bat win --x64 --release --upx\n"
            "  build.bat adb --all\n"
            "  build.bat doctor --fix\n"
        ),
    )
    parser.add_argument("-v", "--verbose", action="store_true", help="输出详细日志")
    parser.add_argument("--version", action="version", version="UDP2CAL build tool {}".format(udp2cal.__version__))

    sub = parser.add_subparsers(dest="command")

    def add_common(target_parser: argparse.ArgumentParser) -> None:
        target_parser.add_argument(
            "--proxy",
            default="direct",
            metavar="MODE",
            help="代理策略: direct(默认) | auto | host:port",
        )
        target_parser.add_argument(
            "--mirror",
            default="auto",
            choices=("auto", "force", "off"),
            help="镜像策略: auto(直连失败才切镜像, 默认) | force(强制镜像) | off(只用官方源)",
        )
        target_parser.add_argument("--java-home", help="指定 JDK 目录（必须为 17~22）")
        target_parser.add_argument("--sdk-dir", help="指定 Android SDK 目录")
        target_parser.add_argument("--no-install-deps", action="store_true", help="不自动补装依赖")
        target_parser.add_argument(
            "-v",
            "--verbose",
            action="store_true",
            default=argparse.SUPPRESS,
            help="输出详细日志",
        )

    p_win = sub.add_parser("win", aliases=["windows"], help="构建 Windows 可执行文件")
    win_mode = p_win.add_mutually_exclusive_group()
    win_mode.add_argument("--release", action="store_true", help="Release 构建（默认）")
    win_mode.add_argument("--debug", action="store_true", help="Debug 构建")
    win_arch = p_win.add_mutually_exclusive_group()
    win_arch.add_argument("--x64", action="store_true", help="64 位 x86_64（默认）")
    win_arch.add_argument("--x86", action="store_true", help="32 位 i686")
    p_win.add_argument("--upx", action="store_true", help="使用 UPX 压缩（仅 Release）")
    add_common(p_win)
    p_win.set_defaults(func=cmd_win)

    p_android = sub.add_parser("android", aliases=["apk"], help="构建 Android APK")
    p_android.add_argument("--modern", action="store_true", help="使用 android（Android 10+，默认且唯一目标）")
    p_android.add_argument("--abi", default="arm64-v8a", choices=config.ANDROID_ABIS, help="目标 ABI")
    android_mode = p_android.add_mutually_exclusive_group()
    android_mode.add_argument("--release", action="store_true", help="Release 构建（默认）")
    android_mode.add_argument("--debug", action="store_true", help="Debug 构建")
    add_common(p_android)
    p_android.set_defaults(func=cmd_android)

    p_adb = sub.add_parser("adb", aliases=["debug"], help="ADB 调试")
    p_adb.add_argument("--build", action="store_true", help="编译 Debug APK")
    p_adb.add_argument("--install", action="store_true", help="安装 APK")
    p_adb.add_argument("--clear", action="store_true", help="清空 Logcat 缓冲")
    p_adb.add_argument("--monitor", action="store_true", help="Logcat 监控")
    p_adb.add_argument("--winlog", action="store_true", help="打开 Windows 调试日志")
    p_adb.add_argument("--all", action="store_true", help="执行全部操作")
    p_adb.add_argument("--modern", action="store_true", help="调试 android（默认且唯一目标）")
    p_adb.add_argument("--abi", default="arm64-v8a", choices=config.ANDROID_ABIS)
    p_adb.add_argument("--adb", help="指定 adb.exe 路径")
    add_common(p_adb)
    p_adb.set_defaults(func=cmd_adb)

    p_doctor = sub.add_parser("doctor", aliases=["check"], help="环境体检")
    p_doctor.add_argument("--fix", action="store_true", help="自动修复可修复项（修改前备份）")
    p_doctor.add_argument("--java-home", help="指定 JDK 目录")
    p_doctor.add_argument("--sdk-dir", help="指定 Android SDK 目录")
    p_doctor.add_argument("--adb", help="指定 adb.exe 路径")
    p_doctor.add_argument("-v", "--verbose", action="store_true", default=argparse.SUPPRESS)
    p_doctor.set_defaults(func=doctor)

    return parser


def main(argv=None) -> int:
    config.LOG_DIR.mkdir(parents=True, exist_ok=True)
    argv = list(sys.argv[1:] if argv is None else argv)
    if not argv:
        return interactive()

    parser = build_parser()
    args = parser.parse_args(argv)
    if not getattr(args, "command", None):
        return interactive()
    return args.func(args)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("\n已中断")
        raise SystemExit(130)
