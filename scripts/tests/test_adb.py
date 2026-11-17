"""adb 安装失败后是否卸载重装的判定。"""

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


class UninstallPolicyTest(unittest.TestCase):
    def test_signature_conflict_retries_after_uninstall(self):
        output = "adb: failed to install app.apk: Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: signatures do not match]"
        self.assertTrue(adb.should_uninstall(output))

    def test_version_downgrade_retries_after_uninstall(self):
        output = "Failure [INSTALL_FAILED_VERSION_DOWNGRADE: Downgrade detected]"
        self.assertTrue(adb.should_uninstall(output))

    def test_storage_full_does_not_uninstall(self):
        output = "adb: failed to install app.apk: Failure [INSTALL_FAILED_INSUFFICIENT_STORAGE]"
        self.assertFalse(adb.should_uninstall(output))

    def test_offline_device_does_not_uninstall(self):
        output = "error: device offline"
        self.assertFalse(adb.should_uninstall(output))

    def test_empty_output_does_not_uninstall(self):
        self.assertFalse(adb.should_uninstall(""))

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
