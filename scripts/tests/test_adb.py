"""adb 安装失败后是否卸载重装的判定。"""

import unittest

import support  # noqa: F401  (把 scripts/ 加入 sys.path)
from udp2cal import adb


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


if __name__ == "__main__":
    unittest.main()
