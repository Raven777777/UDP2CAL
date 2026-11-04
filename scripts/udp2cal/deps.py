"""依赖补装与项目文件自修复。

处理三类实际存在的不一致：
1. gradle wrapper 不完整（缺 wrapper jar 或 properties）；
2. local.properties 里的 sdk.dir 缺失或指向不存在的目录；
3. 构建所需 SDK 组件（platform / build-tools / ndk / cmake）缺失。

所有对已有文件的修改都会先写 `<file>.bak` 备份。
"""

from __future__ import annotations

import datetime
import os
import re
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


_ESCAPES = {"t": "\t", "n": "\n", "r": "\r", "f": "\f"}
_ESCAPES_REVERSE = {value: "\\" + key for key, value in _ESCAPES.items()}


def _join_surrogates(text: str) -> str:
    r"""把 `\uD83D\uDE00` 这类 UTF-16 代理对合并成一个字符。"""
    out = []
    index = 0
    while index < len(text):
        char = text[index]
        if (
            "\ud800" <= char <= "\udbff"
            and index + 1 < len(text)
            and "\udc00" <= text[index + 1] <= "\udfff"
        ):
            out.append(chr(0x10000 + ((ord(char) - 0xD800) << 10) + (ord(text[index + 1]) - 0xDC00)))
            index += 2
            continue
        out.append(char)
        index += 1
    return "".join(out)


def _unescape_property(value: str) -> str:
    r"""按 Java Properties.load 的规则还原转义（含 \uXXXX 与代理对）。"""
    out = []
    index = 0
    while index < len(value):
        char = value[index]
        if char != "\\" or index + 1 >= len(value):
            out.append(char)
            index += 1
            continue
        nxt = value[index + 1]
        if nxt == "u":
            digits = value[index + 2:index + 6]
            if len(digits) == 4 and all(c in "0123456789abcdefABCDEF" for c in digits):
                out.append(chr(int(digits, 16)))
                index += 6
                continue
            # 非法 \u 序列：保留反斜杠，避免丢信息
            out.append(char)
            index += 1
            continue
        out.append(_ESCAPES.get(nxt, nxt))
        index += 2
    return _join_surrogates("".join(out))


def _escape_property(value: str) -> str:
    r"""写成 ASCII 安全的转义形式。

    Java 的 Properties.load 固定按 ISO-8859-1 读文件，中文用户名/emoji 必须转
    成 \uXXXX（超出 BMP 的拆成代理对），否则报 "SDK location not found"。
    """
    out = []
    for char in value:
        if char == "\\":
            out.append("\\\\")
        elif char == ":":
            out.append("\\:")
        elif char in _ESCAPES_REVERSE:
            out.append(_ESCAPES_REVERSE[char])
        elif " " <= char <= "~":
            out.append(char)
        else:
            code = ord(char)
            if code > 0xFFFF:
                code -= 0x10000
                out.append("\\u{:04X}".format(0xD800 + (code >> 10)))
                out.append("\\u{:04X}".format(0xDC00 + (code & 0x3FF)))
            else:
                out.append("\\u{:04X}".format(code))
    return "".join(out)


def resolve_sdk(project_dir: Path, detected: envmod.SdkInfo, logger) -> envmod.SdkInfo:
    """确定唯一生效的 SDK。

    build.gradle.kts 的 ndkRoot() 优先读 local.properties，若这里继续用检测到的
    另一个 SDK，NDK/CMake 会被装到 Gradle 根本不看的地方，永远报「找不到 NDK」。
    因此：local.properties 指向有效 SDK 时以它为准，否则用检测到的。
    """
    current = read_sdk_dir(project_dir)
    if not current:
        return detected
    path = Path(current)
    if not path.is_dir() or not envmod.is_valid_sdk(path):
        logger.warn(
            "local.properties 的 sdk.dir 无效: {}（改用 {}）".format(current, detected.root)
        )
        return detected
    if path.resolve() != detected.root.resolve():
        logger.warn(
            "local.properties 的 sdk.dir({}) 与检测到的 SDK 不同，按 Gradle 的优先级使用它".format(current)
        )
    return envmod.SdkInfo(root=path, source="local.properties")


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
            if current_path.resolve() == sdk.root.resolve() and envmod.is_valid_sdk(current_path):
                logger.detail("local.properties: sdk.dir 正确")
                return True
            if not envmod.is_valid_sdk(current_path):
                logger.warn("local.properties 的 sdk.dir 不是有效的 SDK: {}，将改写".format(current))
            else:
                logger.warn(
                    "local.properties 的 sdk.dir({}) 与本次使用的 SDK 不同，将改写为 {}".format(
                        current, sdk.root
                    )
                )
        else:
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


# ---------------------------------------------------------------- Gradle daemon 注册表


# 注册表里的字符串是「长度前缀 + UTF-8 字节」，后面**没有**终止符；真实样例中
# javaHome 之后紧跟着 javaVersion 的整数字节（首位恰好是 0x00），daemonRegistryDir
# 之后则直接是 0x01。因此按 NUL 截断会把下一条记录的首字节并进路径，拼出一个并
# 不存在的目录；这里统一砍掉结尾的控制字节。
_REGISTRY_PATH_RE = re.compile(r"[A-Za-z]:[\\/][^\x00]*")


def _java_exe_name() -> str:
    return "java.exe" if os.name == "nt" else "java"


def _strip_control_tail(text: str) -> str:
    end = len(text)
    while end and ord(text[end - 1]) < 0x20:
        end -= 1
    return text[:end]


def _norm_path(path) -> str:
    return os.path.normcase(os.path.abspath(str(path)))


def _is_under(path, parent) -> bool:
    child = _norm_path(path)
    root = _norm_path(parent)
    return child == root or child.startswith(root + os.sep)

def heal_daemon_registry(gradle_home, logger) -> int:
    """删除指向失效 JDK 的 Gradle daemon 注册表缓存。

    Gradle 8.9 的 DaemonCompatibilitySpec.jvmCompatible 会对注册表里**每一条**
    daemon 记录的 javaHome 做 `javaHome.exists() && Jvm.forHome(javaHome)`：
    目录还在、但 bin/java 已缺失（半卸载的 JDK）时 Jvm.forHome 抛
    JavaHomeException 并中断整个构建：
        The supplied javaHome seems to be invalid. I cannot find the java executable.
    目录被彻底删除时 Gradle 只判为不兼容，不会抛异常。
    注册表只是缓存（registry.bin.lock 负责跨进程互斥，保留不动），删掉后 Gradle
    会重建，因此这里是安全的自修复。

    Returns:
        实际清理掉的失效记录条数；0 表示没有失效记录或清理失败。
    """
    root = Path(gradle_home) / "daemon"
    if not root.is_dir():
        return 0

    exe = _java_exe_name()
    registry_root = root
    # 注册表里只存两个路径字段：javaHome 和 daemonRegistryDir（<gradle_home>/daemon）。
    # 后者本身也是个「有目录、没 bin/java」的目录，不排掉会被当成失效 JDK，
    # 导致每次构建都重清一次注册表；即使 GRADLE_USER_HOME 变了，名称也仍是 daemon。
    home_itself = _norm_path(gradle_home)

    healed = 0
    for version_dir in sorted(root.iterdir()):
        registry = version_dir / "registry.bin"
        if not version_dir.is_dir() or not registry.is_file():
            continue
        try:
            text = registry.read_bytes().decode("utf-8", "ignore")
        except OSError:
            continue

        stale: List[str] = []
        for match in _REGISTRY_PATH_RE.finditer(text):
            home = _strip_control_tail(match.group(0)).strip()
            if not home or _norm_path(home) == home_itself:
                continue
            if _is_under(home, registry_root) or Path(home).name.lower() == "daemon":
                continue
            path = Path(home)
            # 目录已彻底消失：Gradle 不会对这条记录调用 Jvm.forHome，无需处理。
            if not path.is_dir():
                continue
            if (path / "bin" / exe).is_file():
                continue
            stale.append(home)
        if not stale:
            continue

        try:
            registry.unlink()
        except OSError as exc:
            logger.warn("Gradle {} 的 daemon 注册表清理失败: {}".format(version_dir.name, exc))
            continue
        healed += len(stale)
        logger.warn(
            "已清理 Gradle {} 的 daemon 注册表缓存（{} 条记录指向失效 JDK）".format(
                version_dir.name, len(stale)
            )
        )
        for home in sorted(set(stale)):
            logger.detail("  失效 JDK: {}".format(home))
    return healed


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
