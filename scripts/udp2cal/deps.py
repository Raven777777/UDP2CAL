"""依赖补装与项目文件自修复。

处理三类实际存在的不一致：
1. android_old 缺少 gradle/wrapper（只有 gradlew 脚本，无 wrapper jar）；
2. android_old/local.properties 里的 sdk.dir 指向不存在的目录；
3. 构建所需 SDK 组件（platform / build-tools / ndk / cmake）缺失。

所有对已有文件的修改都会先写 `<file>.bak` 备份。
"""

from __future__ import annotations

import datetime
import shutil
from pathlib import Path
from typing import List, Optional

from . import config, env as envmod, runner


def _backup(path: Path) -> Optional[Path]:
    """生成带时间戳的备份，避免覆盖上一次备份。"""
    if not path.exists():
        return None
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    target = path.with_name("{}.{}.bak".format(path.name, stamp))
    shutil.copy2(str(path), str(target))
    return target


# ---------------------------------------------------------------- Gradle wrapper


def wrapper_ready(project_dir: Path) -> bool:
    wrapper = project_dir / "gradle" / "wrapper"
    return (wrapper / "gradle-wrapper.jar").exists() and (wrapper / "gradle-wrapper.properties").exists()


def ensure_gradle_wrapper(project_dir: Path, logger) -> bool:
    """补齐缺失的 gradle wrapper，来源优先取仓库内其他项目的同名文件。"""
    if wrapper_ready(project_dir):
        logger.detail("gradle wrapper: 完整")
        return True

    sources: List[Path] = []
    for key, target in config.ANDROID_TARGETS.items():
        other = Path(target["dir"])
        if other.resolve() == project_dir.resolve():
            continue
        if wrapper_ready(other):
            sources.append(other / "gradle" / "wrapper")

    if not sources:
        logger.error("{} 缺少 gradle/wrapper，且仓库内无可复制的来源".format(project_dir.name))
        logger.hint("请在 {} 下执行一次 `gradle wrapper --gradle-version 8.9`".format(project_dir.name))
        return False

    src = sources[0]
    dst = project_dir / "gradle" / "wrapper"
    dst.mkdir(parents=True, exist_ok=True)
    for name in ("gradle-wrapper.jar", "gradle-wrapper.properties"):
        source_file = src / name
        if source_file.exists():
            shutil.copy2(str(source_file), str(dst / name))
    logger.warn("已从 {} 复制 gradle wrapper 到 {}".format(src.parent.parent.name, project_dir.name))
    return wrapper_ready(project_dir)


# ---------------------------------------------------------------- local.properties


def _unescape_property(value: str) -> str:
    out = []
    index = 0
    while index < len(value):
        char = value[index]
        if char == "\\" and index + 1 < len(value):
            out.append(value[index + 1])
            index += 2
            continue
        out.append(char)
        index += 1
    return "".join(out)


def _escape_property(value: str) -> str:
    return value.replace("\\", "\\\\").replace(":", "\\:")


def read_sdk_dir(project_dir: Path) -> Optional[str]:
    props = project_dir / "local.properties"
    if not props.exists():
        return None
    try:
        for line in props.read_text(encoding="utf-8", errors="replace").splitlines():
            stripped = line.strip()
            if not stripped or stripped.startswith("#") or "=" not in stripped:
                continue
            key, _, value = stripped.partition("=")
            if key.strip() == "sdk.dir":
                return _unescape_property(value.strip())
    except OSError:
        return None
    return None


def ensure_local_properties(project_dir: Path, sdk: envmod.SdkInfo, logger) -> bool:
    """校验并修正 local.properties 中的 sdk.dir（修改前备份）。"""
    props = project_dir / "local.properties"
    current = read_sdk_dir(project_dir)
    backup: Optional[Path] = None
    if current:
        current_path = Path(current)
        if current_path.is_dir():
            if current_path.resolve() == sdk.root.resolve():
                logger.detail("local.properties: sdk.dir 正确")
                return True
            logger.warn(
                "local.properties 的 sdk.dir({}) 与本次使用的 SDK 不同，保持原样".format(current)
            )
            return True
        logger.warn("local.properties 的 sdk.dir 指向不存在的目录: {}".format(current))
    elif props.exists():
        logger.detail("local.properties: 未设置 sdk.dir，将补写")

    if props.exists():
        backup = _backup(props)
        if backup:
            logger.hint("已备份至 {}".format(backup.name))

    lines = []
    if props.exists():
        try:
            lines = props.read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError:
            lines = []

    entry = "sdk.dir={}".format(_escape_property(str(sdk.root)))
    replaced = False
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("#") or "=" not in stripped:
            continue
        key = stripped.split("=", 1)[0].strip()
        if key == "sdk.dir":
            lines[index] = entry
            replaced = True
            break
    if not replaced:
        lines.append(entry)

    props.write_text("\n".join(lines).rstrip("\n") + "\n", encoding="utf-8")
    logger.ok("已写入 sdk.dir={}".format(sdk.root))
    return True


# ---------------------------------------------------------------- SDK 组件


def ensure_sdk_components(
    sdk: envmod.SdkInfo,
    target_key: str,
    logger,
    env: Optional[dict] = None,
) -> bool:
    missing = envmod.missing_components(sdk, target_key)
    if not missing:
        logger.detail("SDK 组件: 齐全")
        return True

    logger.warn("缺少 SDK 组件: {}".format(", ".join(missing)))
    manager = envmod.find_sdkmanager(sdk)
    if manager is None:
        logger.error("未找到 sdkmanager，无法自动安装")
        logger.hint("请通过 Android Studio SDK Manager 安装上述组件")
        return False

    logger.info("调用 sdkmanager 安装（首次安装可能耗时数分钟）")
    stdin_text = "y\n" * 60
    cmd = [str(manager), "--sdk_root={}".format(sdk.root)] + missing
    result = runner.run(cmd, logger=logger, env=env, stdin_text=stdin_text, tail_lines=80)
    if not result.ok:
        logger.error("sdkmanager 安装失败（退出码 {}）".format(result.code))
        logger.hint("请手动执行: {} --sdk_root={} {}".format(manager, sdk.root, " ".join(missing)))
        return False

    still_missing = envmod.missing_components(sdk, target_key)
    if still_missing:
        logger.error("安装后仍缺少: {}".format(", ".join(still_missing)))
        return False
    logger.ok("SDK 组件安装完成")
    return True


# ---------------------------------------------------------------- Rust target


def ensure_rust_target(rust: envmod.RustInfo, triple: str, logger, env: Optional[dict] = None) -> bool:
    installed = envmod.installed_rust_targets(rust)
    if triple in installed:
        logger.detail("Rust target {}: 已安装".format(triple))
        return True
    if rust.rustup is None:
        logger.error("未找到 rustup，无法自动添加 target {}".format(triple))
        return False

    logger.info("rustup target add {}".format(triple))
    result = runner.run([str(rust.rustup), "target", "add", triple], logger=logger, env=env, tail_lines=40)
    if not result.ok:
        logger.error("添加 target 失败（退出码 {}）".format(result.code))
        return False
    logger.ok("Rust target {} 已就绪".format(triple))
    return True
