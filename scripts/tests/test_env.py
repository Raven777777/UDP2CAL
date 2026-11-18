"""JDK 选择：超出 Gradle 支持上限的版本必须直接拒绝。"""

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import support
from udp2cal import env as envmod

FakeLogger = support.FakeLogger


def _java(major: int) -> envmod.JavaInfo:
    return envmod.JavaInfo(Path("jdk{}".format(major)), version=str(major), major=major)


class SelectJavaTest(unittest.TestCase):
    def test_jdk_above_max_is_rejected(self):
        for major in (23, 25):
            logger = FakeLogger()
            self.assertIsNone(envmod.select_java(logger, candidates=[_java(major)]))
            self.assertTrue(any("超出" in text for text in logger.texts("error")))

    def test_jdk_below_min_is_rejected(self):
        logger = FakeLogger()
        self.assertIsNone(envmod.select_java(logger, candidates=[_java(16)]))
        self.assertTrue(logger.texts("warn"))

    def test_supported_jdk_is_selected(self):
        self.assertEqual(envmod.select_java(FakeLogger(), candidates=[_java(21)]).major, 21)

    def test_explicit_preferred_wins_over_java_home(self):
        with tempfile.TemporaryDirectory() as tmp:
            preferred = Path(tmp)
            (preferred / "bin").mkdir()
            (preferred / "bin" / "java.exe").write_text("")
            (preferred / "release").write_text('JAVA_VERSION="21.0.1"\n')
            env_java = _java(17)

            with patch.dict(os.environ, {"JAVA_HOME": str(env_java.home)}):
                chosen = envmod.select_java(
                    FakeLogger(), preferred=str(preferred), candidates=[env_java]
                )

        self.assertEqual(chosen.home, preferred)
        self.assertEqual(chosen.major, 21)


if __name__ == "__main__":
    unittest.main()
