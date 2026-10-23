"""控制台输出与日志留档。

约定：
- 交互模式下输出带 ANSI 颜色；重定向到管道时自动降级为纯文本。
- 所有输出同步写入 logs/<name>-<timestamp>.log，构建失败时可完整回溯。
"""

from __future__ import annotations

import datetime
import os
import sys
from pathlib import Path


class C:
    """ANSI 颜色常量。"""

    RESET = "\033[0m"
    BOLD = "\033[1m"
    DIM = "\033[2m"
    RED = "\033[31m"
    GREEN = "\033[32m"
    YELLOW = "\033[33m"
    BLUE = "\033[34m"
    CYAN = "\033[36m"
    GRAY = "\033[90m"


def _enable_ansi() -> bool:
    """在 Windows 控制台启用虚拟终端序列。"""
    if os.name != "nt":
        return True
    try:
        import ctypes

        kernel32 = ctypes.windll.kernel32
        handle = kernel32.GetStdHandle(-11)  # STD_OUTPUT_HANDLE
        mode = ctypes.c_uint32()
        if not kernel32.GetConsoleMode(handle, ctypes.byref(mode)):
            return False
        kernel32.SetConsoleMode(handle, mode.value | 0x0004)  # ENABLE_VIRTUAL_TERMINAL_PROCESSING
        return True
    except Exception:
        return False


def _configure_stdio() -> None:
    """统一使用 UTF-8 输出，避免中文在 GBK 代码页下乱码。"""
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except Exception:
            pass


def _strip_ansi(text: str) -> str:
    import re

    return re.sub(r"\033\[[0-9;]*m", "", text)


class Logger:
    """带颜色与文件留档的日志器。"""

    def __init__(
        self,
        name: str = "udp2cal",
        log_dir: Path | None = None,
        verbose: bool = False,
        echo: bool = True,
    ) -> None:
        _configure_stdio()
        self.verbose = verbose
        self.echo = echo
        self._color = _enable_ansi() and self._isatty()
        self._fp = None
        self.path: Path | None = None

        if log_dir is not None:
            log_dir.mkdir(parents=True, exist_ok=True)
            stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
            self.path = log_dir / "{}-{}.log".format(name, stamp)
            self._fp = open(self.path, "w", encoding="utf-8", errors="replace")

    # ------------------------------------------------------------ 内部

    @staticmethod
    def _isatty() -> bool:
        try:
            return sys.stdout.isatty()
        except Exception:
            return False

    def _paint(self, text: str, color: str | None) -> str:
        if not self._color or not color:
            return text
        return "{}{}{}".format(color, text, C.RESET)

    def _emit(self, text: str = "", color: str | None = None, err: bool = False) -> None:
        plain = _strip_ansi(text)
        if self.echo:
            stream = sys.stderr if err else sys.stdout
            shown = text if self._color else plain
            try:
                stream.write(self._paint(shown, color) + "\n")
                stream.flush()
            except Exception:
                pass
        if self._fp:
            self._fp.write(plain + "\n")
            self._fp.flush()

    # ------------------------------------------------------------ 公共接口

    def banner(self, title: str, width: int = 44) -> None:
        line = "=" * width
        self._emit("")
        self._emit(self._paint(line, C.CYAN))
        self._emit(self._paint("   " + title, C.CYAN + C.BOLD))
        self._emit(self._paint(line, C.CYAN))

    def section(self, title: str) -> None:
        self._emit("")
        self._emit(self._paint("── {} {}".format(title, "─" * max(4, 40 - len(title))), C.BLUE))

    def step(self, index: int, total: int, text: str) -> None:
        self._emit("")
        self._emit(self._paint("[{}/{}] {}".format(index, total, text), C.BOLD))

    def info(self, text: str) -> None:
        self._emit("  " + text)

    def ok(self, text: str) -> None:
        self._emit("  " + self._paint("✓ " + text, C.GREEN))

    def warn(self, text: str) -> None:
        self._emit("  " + self._paint("! " + text, C.YELLOW))

    def error(self, text: str) -> None:
        self._emit("  " + self._paint("x " + text, C.RED), err=True)

    def detail(self, text: str) -> None:
        """仅在 --verbose 时输出的细节。"""
        if self.verbose:
            self._emit("    " + self._paint(text, C.GRAY))

    def hint(self, text: str) -> None:
        self._emit("    " + self._paint(text, C.GRAY))

    def raw(self, text: str) -> None:
        """子进程原始输出行（不加前缀）。"""
        self._emit(text)

    def blank(self) -> None:
        self._emit("")

    def close(self) -> None:
        if self._fp:
            try:
                self._fp.close()
            finally:
                self._fp = None
