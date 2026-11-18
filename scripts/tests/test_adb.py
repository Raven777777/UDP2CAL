"""adb 安装失败后不自动卸载，避免清除应用数据。"""

import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import support  # noqa: F401  (把 scripts/ 加入 sys.path)
from udp2cal import adb, runner

FakeLogger = support.FakeLogger


class _AdbLogger(FakeLogger):
    def banner(self, message):
        self._log("banner", message)

    def step(self, index, total, message):
        self._log("step", message)

    def blank(self):
        self._log("blank", "")


class InstallFailurePolicyTest(unittest.TestCase):
    def _install_with_results(self, results):
        logger = FakeLogger()
        with patch.object(adb.runner, "run", side_effect=results) as run:
            ok = adb._install(Path("adb"), Path("app-debug.apk"), logger)
        return ok, run, logger

    def test_version_downgrade_retries_with_downgrade_flag(self):
        ok, run, logger = self._install_with_results([
            runner.RunResult(code=1, lines=["Failure [INSTALL_FAILED_VERSION_DOWNGRADE: Downgrade detected]"]),
            runner.RunResult(code=0, lines=["Success"]),
        ])
        self.assertTrue(ok)
        commands = [call.args[0] for call in run.call_args_list]
        self.assertEqual(commands, [
            ["adb", "install", "-r", "app-debug.apk"],
            ["adb", "install", "-r", "-d", "app-debug.apk"],
        ])
        self.assertNotIn("uninstall", [arg for command in commands for arg in command])
        self.assertTrue(any("版本降级" in text for text in logger.texts("warn")))

    def test_version_downgrade_retry_failure_keeps_data(self):
        ok, run, logger = self._install_with_results([
            runner.RunResult(code=1, lines=["Failure [INSTALL_FAILED_VERSION_DOWNGRADE]"]),
            runner.RunResult(code=1, lines=["Failure [INSTALL_FAILED_VERSION_DOWNGRADE]"]),
        ])
        self.assertFalse(ok)
        self.assertEqual(run.call_count, 2)
        self.assertNotIn("uninstall", run.call_args.args[0])
        self.assertTrue(any("未卸载" in text for text in logger.texts("error")))
        self.assertTrue(any("手动卸载" in text for text in logger.texts("hint")))

    def test_signature_conflict_does_not_uninstall(self):
        ok, run, logger = self._install_with_results([
            runner.RunResult(code=1, lines=[
                "adb: failed to install app.apk: Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: signatures do not match]",
            ]),
        ])
        self.assertFalse(ok)
        self.assertEqual(run.call_count, 1)
        self.assertEqual(run.call_args.args[0], ["adb", "install", "-r", "app-debug.apk"])
        self.assertTrue(any("未卸载" in text for text in logger.texts("error")))
        self.assertTrue(any("手动卸载" in text and "私有数据" in text for text in logger.texts("hint")))

    def test_failure_reason_line(self):
        output = "Performing Streamed Install\nadb: failed to install app.apk: Failure [INSTALL_FAILED_INSUFFICIENT_STORAGE]"
        self.assertIn("INSTALL_FAILED_INSUFFICIENT_STORAGE", adb._failure_reason(output))
        self.assertEqual(adb._failure_reason("Performing Streamed Install"), "")


class DeviceSelectionTest(unittest.TestCase):
    def test_multiple_devices_is_rejected(self):
        logger = FakeLogger()
        with patch.object(adb, "_list_devices", return_value=["serial-a", "serial-b"]):
            self.assertFalse(adb._ensure_device(Path("adb"), logger))
        self.assertTrue(any("多台设备" in text for text in logger.texts("error")))

    def test_single_device_is_accepted(self):
        logger = FakeLogger()
        with patch.object(adb, "_list_devices", return_value=["serial-a"]):
            self.assertTrue(adb._ensure_device(Path("adb"), logger))


class LogcatResultTest(unittest.TestCase):
    def test_failed_clear_returns_nonzero(self):
        ctx = SimpleNamespace(
            adb_build=False,
            adb_install=False,
            adb_clear=True,
            adb_monitor=False,
            adb_winlog=False,
            adb_all=False,
        )
        logger = _AdbLogger()
        with patch.object(adb, "_resolve_adb", return_value=Path("adb")), \
                patch.object(adb, "_ensure_device", return_value=True), \
                patch.object(adb.runner, "run", return_value=runner.RunResult(code=1, lines=["error: device offline"])):
            code = adb.run_adb(ctx, logger)

        self.assertEqual(code, 1)
        self.assertNotIn("缓冲已清空", logger.texts("ok"))
        self.assertTrue(logger.texts("error"))


if __name__ == "__main__":
    unittest.main()
