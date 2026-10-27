"""子进程执行封装。

关键点：
- 逐行流式回显，长构建不会"假死"；
- 同时保留尾部若干行，用于失败诊断与网络故障识别；
- Windows 上 .bat/.cmd 必须经 cmd /c 调用。
"""

from __future__ import annotations

import locale
import os
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable, List, Sequence, Union

from . import config


@dataclass
class RunResult:
    """一次外部命令的执行结果。"""

    code: int
    lines: List[str] = field(default_factory=list)
    command: str = ""

    @property
    def ok(self) -> bool:
        return self.code == 0

    @property
    def tail(self) -> str:
        return "\n".join(self.lines)

    def matched(self, markers: Iterable[str]) -> bool:
        text = self.tail.lower()
        return any(m.lower() in text for m in markers)

    @property
    def network_failed(self) -> bool:
        return self.matched(config.NETWORK_FAILURE_MARKERS)


def _decode(raw: bytes) -> str:
    """按 UTF-8 → 系统代码页 的顺序尝试解码。"""
    for encoding in ("utf-8", "mbcs", locale.getpreferredencoding(False)):
        try:
            return raw.decode(encoding)
        except (UnicodeDecodeError, LookupError):
            continue
    return raw.decode("utf-8", "replace")


def needs_shell(program: str) -> bool:
    return program.lower().endswith((".bat", ".cmd"))


def build_command(cmd: Sequence[str]) -> Union[str, List[str]]:
    """返回可直接交给 `subprocess` 的命令行。

    `.bat`/`.cmd` 需要包一层 `cmd /c`，否则 CreateProcess 无法直接执行。

    返回 `str` 时表示已按 cmd.exe 的规则拼好（Windows 上传 `str` 给 Popen 会
    原样交给 CreateProcess，不再二次转义）。

    cmd.exe 的引号规则：命令串里**超过两个**引号时，cmd 会剥掉首尾各一个再解析。
    因此 `["cmd", "/c", bat, "--flag", "a b"]` 被 list2cmdline 拼成
    `cmd /c "C:\\..\\sp ace\\gw.bat" --flag "a b"` 后，cmd 先剥引号再按空格切分，
    会在 bat 路径的空格处断开（实测报错 `'C:/Users/.../sp' is not recognized`）。
    官方写法是把整条命令再包一层引号：`cmd /c ""bat" "arg with space""`。
    """
    parts = [str(c) for c in cmd]
    if parts and needs_shell(parts[0]):
        return 'cmd /c "{}"'.format(subprocess.list2cmdline(parts))
    return parts


def _terminate(proc: subprocess.Popen) -> None:
    """结束子进程（Windows 上连同 cmd /c 派生的子进程一起结束）。"""
    if proc.poll() is not None:
        return
    try:
        if os.name == "nt":
            subprocess.run(
                ["taskkill", "/F", "/T", "/PID", str(proc.pid)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=10,
            )
        else:
            proc.terminate()
    except (OSError, subprocess.SubprocessError):
        pass
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            proc.kill()
        except OSError:
            pass


def _display(cmd: Sequence[str]) -> str:
    """把命令还原成便于写日志的一行（不做 cmd 包装）。"""
    return subprocess.list2cmdline([str(c) for c in cmd])


def run(
    cmd: Sequence[str],
    logger=None,
    cwd: Path | str | None = None,
    env: dict | None = None,
    tail_lines: int = 200,
    stdin_text: str | None = None,
    quiet: bool = False,
) -> RunResult:
    """执行命令并流式回显输出。

    Args:
        cmd: 命令与参数列表（不要传 shell 字符串）。
        logger: Logger 实例，用于逐行回显。
        cwd: 工作目录。
        env: 完整的环境变量字典（调用方负责基于 os.environ 构造）。
        tail_lines: 保留的尾部行数。
        stdin_text: 非交互场景下的标准输入内容（如 sdkmanager 许可确认）。
        quiet: 为 True 时不回显输出（仅用于探测类命令）。
    """
    argv = build_command(cmd)
    displayed = argv if isinstance(argv, str) else _display(cmd)
    try:
        proc = subprocess.Popen(
            argv,
            cwd=str(cwd) if cwd else None,
            env=env,
            stdin=subprocess.PIPE if stdin_text is not None else None,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
    except FileNotFoundError:
        msg = "找不到可执行文件: {}".format(cmd[0])
        if logger:
            logger.error(msg)
        return RunResult(code=127, lines=[msg], command=displayed)
    except OSError as exc:
        msg = "无法启动进程: {} ({})".format(cmd[0], exc)
        if logger:
            logger.error(msg)
        return RunResult(code=126, lines=[msg], command=displayed)

    if stdin_text is not None and proc.stdin is not None:
        try:
            proc.stdin.write(stdin_text.encode("utf-8", "replace"))
            proc.stdin.close()
        except OSError:
            pass

    collected: List[str] = []
    assert proc.stdout is not None
    try:
        for raw in proc.stdout:
            line = _decode(raw).rstrip("\r\n")
            collected.append(line)
            if len(collected) > tail_lines * 4:
                del collected[: len(collected) - tail_lines * 2]
            if logger and not quiet:
                logger.raw(line)
    except KeyboardInterrupt:
        _terminate(proc)
        raise

    code = proc.wait()
    return RunResult(code=code, lines=collected[-tail_lines:], command=displayed)


def capture(cmd: Sequence[str], cwd: Path | str | None = None, env: dict | None = None, timeout: int = 30) -> RunResult:
    """静默执行并捕获输出（用于版本探测）。"""
    argv = build_command(cmd)
    try:
        proc = subprocess.run(
            argv,
            cwd=str(cwd) if cwd else None,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=timeout,
        )
    except (FileNotFoundError, OSError, subprocess.TimeoutExpired):
        return RunResult(code=127, lines=[], command=_display(cmd))

    text = _decode(proc.stdout or b"")
    return RunResult(code=proc.returncode, lines=text.splitlines(), command=_display(cmd))


def clean_env(extra: dict | None = None, unset: Iterable[str] = ()) -> dict:
    """构造子进程环境变量：继承当前环境，去除干扰项后覆盖。"""
    env = dict(os.environ)
    for key in unset:
        env.pop(key, None)
    if extra:
        for key, value in extra.items():
            if value is None:
                env.pop(key, None)
            else:
                env[str(key)] = str(value)
    return env


def prepend_path(env: dict, *entries: Path | str) -> dict:
    """把目录插入 PATH 头部（去重、忽略不存在的项）。"""
    existing = [p for p in env.get("PATH", "").split(os.pathsep) if p]
    fresh = [str(e) for e in entries if e and Path(str(e)).exists()]
    merged = fresh + [p for p in existing if p not in fresh]
    env["PATH"] = os.pathsep.join(merged)
    return env
