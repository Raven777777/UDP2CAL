"""ADB 调试流程：编译 Debug APK、安装、清理日志、Logcat 监控。

对应原 debug_adb.bat 的 --build / --install / --clear / --monitor / --winlog / --all。
"""

from __future__ import annotations

import copy
import subprocess
from pathlib import Path
from typing import List, Optional

from . import android as androidmod, config, env as envmod, runner

PACKAGE_NAME = "com.udp2cal.app"
LOGCAT_TAGS = "OpusEncoder:V CaptureService:V MainActivity:V"
WIN_DEBUG_LOG = config.WINDOWS_DIR / "udp2cal_debug.log"


def _resolve_adb(ctx, logger) -> Optional[Path]:
    roots = [Path(ctx.sdk_dir)] if getattr(ctx, "sdk_dir", None) else []
    sdks = envmod.detect_sdks(roots)
    adb = envmod.detect_adb(sdks[0] if sdks else None, getattr(ctx, "adb_path", None))
    if adb is None:
        logger.error("未找到 adb")
        logger.hint("在 Android SDK 的 platform-tools 目录中，或用 --adb 指定路径")
        return None
    logger.detail("adb: {}".format(adb))
    return adb


def _list_devices(adb: Path) -> List[str]:
    result = runner.capture([str(adb), "devices"], timeout=30)
    serials = []
    for line in result.lines:
        parts = line.split()
        # 跳过 "List of devices attached" 与 adb 守护进程启动提示
        if len(parts) >= 2 and parts[1] == "device":
            serials.append(parts[0])
    return serials


def _ensure_device(adb: Path, logger) -> bool:
    serials = _list_devices(adb)
    if not serials:
        logger.error("未检测到 Android 设备")
        logger.hint("确认 1) USB 调试已开启  2) adb 驱动已安装  3) 已在设备上授权本机")
        return False
    for serial in serials:
        logger.ok("设备已连接: {}".format(serial))
    return True


def _install(adb: Path, apk: Path, logger) -> bool:
    logger.info("安装 {} ...".format(apk.name))
    result = runner.run([str(adb), "install", "-r", str(apk)], logger=logger, tail_lines=40)
    if result.ok:
        logger.ok("安装成功")
        return True

    logger.warn("安装失败，可能存在签名冲突，尝试卸载后重装")
    runner.run([str(adb), "uninstall", PACKAGE_NAME], logger=logger, tail_lines=20)
    retry = runner.run([str(adb), "install", "-r", str(apk)], logger=logger, tail_lines=40)
    if retry.ok:
        logger.ok("安装成功")
        return True

    logger.error("安装失败")
    return False


def run_adb(ctx, logger) -> int:
    actions = {
        "build": bool(getattr(ctx, "adb_build", False)),
        "install": bool(getattr(ctx, "adb_install", False)),
        "clear": bool(getattr(ctx, "adb_clear", False)),
        "monitor": bool(getattr(ctx, "adb_monitor", False)),
        "winlog": bool(getattr(ctx, "adb_winlog", False)),
    }
    if getattr(ctx, "adb_all", False):
        actions = {key: True for key in actions}

    if not any(actions.values()):
        _print_usage(logger)
        return 0

    logger.banner("udp2cal ADB 调试")

    # 仅打开 Windows 日志 / 只编译时不需要设备，也不应因 adb 缺失而失败
    needs_device = actions["install"] or actions["clear"] or actions["monitor"]
    adb: Optional[Path] = None
    if needs_device:
        adb = _resolve_adb(ctx, logger)
        if adb is None:
            return 1
        if not _ensure_device(adb, logger):
            return 1

    steps = [name for name, enabled in actions.items() if enabled]
    total = len(steps)
    index = 0

    if actions["build"]:
        index += 1
        logger.step(index, total, "编译 Debug APK")
        sub_ctx = copy.copy(ctx)
        sub_ctx.target = getattr(ctx, "target", "modern")
        sub_ctx.build_type = "Debug"
        sub_ctx.abi = getattr(ctx, "abi", None) or "arm64-v8a"
        sub_ctx.logger = logger
        code = androidmod.build(sub_ctx, logger)
        if code != 0:
            logger.error("编译失败，中止后续步骤")
            return code

    apk: Optional[Path] = None
    if actions["install"]:
        index += 1
        logger.step(index, total, "安装 APK 到设备")
        assert adb is not None
        project_dir = config.ANDROID_TARGETS[getattr(ctx, "target", "modern")]["dir"]
        apk = androidmod.find_apk(project_dir, "Debug")
        if apk is None:
            logger.error("未找到 Debug APK，请先执行 --build")
            logger.hint("期望路径: {}/app/build/outputs/apk/debug/app-debug.apk".format(project_dir))
            return 1
        if not _install(adb, apk, logger):
            return 1

    if actions["clear"]:
        index += 1
        logger.step(index, total, "清空 Logcat 缓冲")
        assert adb is not None
        runner.run([str(adb), "logcat", "-c"], logger=logger, tail_lines=10)
        logger.ok("缓冲已清空")

    if actions["winlog"]:
        index += 1
        logger.step(index, total, "打开 Windows 调试日志")
        _open_winlog(logger)

    if actions["monitor"]:
        index += 1
        logger.step(index, total, "Logcat 监控")
        assert adb is not None
        _monitor(adb, logger)

    return 0


def _monitor(adb: Path, logger) -> None:
    logger.blank()
    logger.info("请在手机上：打开 udp2cal → 开启「调试模式」→ 点击「开始采集」")
    logger.info("按 Ctrl+C 退出监控")
    logger.blank()
    try:
        runner.run(
            [str(adb), "logcat", "-v", "time", "-s"] + LOGCAT_TAGS.split(),
            logger=logger,
            tail_lines=10,
        )
    except KeyboardInterrupt:
        logger.blank()
        logger.ok("已停止 Logcat 监控")


def _open_winlog(logger) -> None:
    if not WIN_DEBUG_LOG.exists():
        logger.warn("Windows 调试日志尚未生成（需在 Windows 端开启调试模式）")
        logger.hint("期望路径: {}".format(WIN_DEBUG_LOG))
        return
    logger.info("打开 {}".format(WIN_DEBUG_LOG))
    try:
        subprocess.Popen(["notepad.exe", str(WIN_DEBUG_LOG)])
    except OSError as exc:
        logger.warn("无法启动记事本: {}".format(exc))


def _print_usage(logger) -> None:
    logger.blank()
    logger.info("用法: build.bat adb [选项]")
    logger.blank()
    logger.info("选项:")
    logger.info("  --build     编译 Debug APK")
    logger.info("  --install   安装 APK 到设备")
    logger.info("  --clear     清空 Logcat 缓冲")
    logger.info("  --monitor   启动 Logcat 监控 (OpusEncoder / CaptureService / MainActivity)")
    logger.info("  --winlog    打开 Windows 调试日志 (udp2cal_debug.log)")
    logger.info("  --all       执行全部上述操作")
    logger.info("  --old       调试 android_old（默认 android）")
    logger.info("  --abi ABI   目标 ABI: arm64-v8a(默认) | armeabi-v7a")
    logger.blank()
    logger.info("示例:")
    logger.hint("build.bat adb --all        一键编译 + 安装 + 监控")
    logger.hint("build.bat adb --monitor    仅启动 Logcat 监控")
