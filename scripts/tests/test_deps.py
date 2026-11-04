"""local.properties 读写与「唯一生效 SDK」的解析。"""

import tempfile
import unittest
from pathlib import Path

import support
from udp2cal import deps, env as envmod

FakeLogger = support.FakeLogger


def _make_sdk(root: Path, *components: str) -> Path:
    root.mkdir(parents=True, exist_ok=True)
    for name in components or ("platforms",):
        (root / name).mkdir(exist_ok=True)
    return root


class PropertyEscapeTest(unittest.TestCase):
    def test_escape_is_ascii_and_roundtrips(self):
        cases = [
            r"C:\Users\张三\AppData\Local\Android\Sdk",
            "/home/😀/Android/Sdk",
            r"D:\a:b\back\\slash\test",
            r"C:\Program Files\Android",
            "tab\tand\nnewline",
            "plain-ascii/path",
        ]
        for value in cases:
            escaped = deps._escape_property(value)
            escaped.encode("ascii")  # Java 按 ISO-8859-1 读，必须全 ASCII
            self.assertNotIn("\n", escaped)
            self.assertEqual(deps._unescape_property(escaped), value)

    def test_surrogate_pair_is_written_as_two_units(self):
        escaped = deps._escape_property("/sdk/😀")
        self.assertIn(r"\uD83D\uDE00", escaped)

    def test_unescape_java_escapes(self):
        self.assertEqual(deps._unescape_property(r"\u5F20\:a\\b\tc"), "张:a\\b\tc")

    def test_read_sdk_dir_decodes_java_escapes(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            (project / "local.properties").write_text(
                "sdk.dir=C\\:\\\\Users\\\\\\u5F20\\u4E09\\\\Sdk\n", encoding="utf-8"
            )
            self.assertEqual(deps.read_sdk_dir(project), r"C:\Users\张三\Sdk")


class SdkResolutionTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.project = self.root / "android"
        self.project.mkdir()
        self.detected = envmod.SdkInfo(root=_make_sdk(self.root / "detected"), source="test")
        self.preferred = envmod.SdkInfo(root=_make_sdk(self.root / "preferred"), source="test")
        self.logger = FakeLogger()

    def tearDown(self):
        self._tmp.cleanup()

    def _write_sdk_dir(self, value: str) -> None:
        (self.project / "local.properties").write_text(
            "sdk.dir={}\n".format(deps._escape_property(value)), encoding="utf-8"
        )

    def test_prefers_local_properties_sdk(self):
        self._write_sdk_dir(str(self.preferred.root))
        sdk = deps.resolve_sdk(self.project, self.detected, self.logger)
        self.assertEqual(sdk.root, self.preferred.root)
        self.assertTrue(any("按 Gradle 的优先级" in text for text in self.logger.texts("warn")))

    def test_missing_sdk_dir_falls_back_to_detected(self):
        self._write_sdk_dir(str(self.root / "missing"))
        sdk = deps.resolve_sdk(self.project, self.detected, self.logger)
        self.assertEqual(sdk.root, self.detected.root)

    def test_non_sdk_dir_is_replaced_in_properties(self):
        empty = self.root / "empty"
        empty.mkdir()
        self._write_sdk_dir(str(empty))
        sdk = deps.resolve_sdk(self.project, self.detected, self.logger)
        self.assertTrue(deps.ensure_local_properties(self.project, sdk, self.logger))
        self.assertEqual(deps.read_sdk_dir(self.project), str(self.detected.root))
        self.assertTrue(list(self.project.glob("local.properties.*.bak")))

    def test_correct_sdk_dir_is_not_rewritten(self):
        self._write_sdk_dir(str(self.detected.root))
        self.assertTrue(deps.ensure_local_properties(self.project, self.detected, self.logger))
        self.assertFalse(list(self.project.glob("local.properties.*.bak")))
        self.assertIn("sdk.dir 正确", " ".join(self.logger.texts("detail")))

    def test_chinese_path_roundtrip_through_file(self):
        sdk_root = _make_sdk(self.root / "中文 路径 & sdk")
        self._write_sdk_dir(str(sdk_root))
        sdk = deps.resolve_sdk(self.project, self.detected, self.logger)
        self.assertEqual(sdk.root, sdk_root)
        self.assertTrue(deps.ensure_local_properties(self.project, sdk, self.logger))
        self.assertEqual(deps.read_sdk_dir(self.project), str(sdk_root))


if __name__ == "__main__":
    unittest.main()
