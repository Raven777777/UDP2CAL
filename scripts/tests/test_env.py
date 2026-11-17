"""JDK 选择：超出 Gradle 支持上限的版本必须直接拒绝。"""

import unittest
from pathlib import Path

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


if __name__ == "__main__":
    unittest.main()
