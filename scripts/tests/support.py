"""测试辅助：把 scripts/ 加入 sys.path，并提供不写日志的 Logger 替身。"""

import sys
from pathlib import Path

SCRIPTS_DIR = Path(__file__).resolve().parents[1]
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))


class FakeLogger:
    """记录调用参数，供断言使用。"""

    def __init__(self):
        self.messages = []

    def _log(self, level, message):
        self.messages.append((level, message))

    def detail(self, message):
        self._log("detail", message)

    def info(self, message):
        self._log("info", message)

    def ok(self, message):
        self._log("ok", message)

    def warn(self, message):
        self._log("warn", message)

    def error(self, message):
        self._log("error", message)

    def hint(self, message):
        self._log("hint", message)

    def texts(self, level=None):
        return [text for name, text in self.messages if level is None or name == level]
