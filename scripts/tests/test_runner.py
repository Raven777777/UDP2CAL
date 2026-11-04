"""runner.build_command 的 cmd.exe 引用规则。"""

import os
import shutil
import tempfile
import unittest
from pathlib import Path

import support  # noqa: F401  (把 scripts/ 加入 sys.path)
from udp2cal import runner


class BuildCommandQuotingTest(unittest.TestCase):
    def test_non_batch_command_stays_list(self):
        cmd = ["python", "-c", "print('a b')"]
        self.assertEqual(runner.build_command(cmd), cmd)

    def test_batch_path_with_space_and_ampersand_is_escaped(self):
        cmd = runner.build_command([r"C:\repo & tools\build.bat", "--abi", "arm64-v8a"])
        self.assertIsInstance(cmd, str)
        self.assertIn('"C:\\repo & tools\\build.bat"', cmd)

    def test_meta_arguments_are_quoted_and_escaped(self):
        result = runner.build_command(["build.bat", "a b", "x&y", "p|q", "a^b", "50%"])
        self.assertIn('"a b"', result)
        self.assertIn('"x^&y"', result)
        self.assertIn('"p^|q"', result)
        self.assertIn('"a^^b"', result)
        self.assertIn('"50^%"', result)

    def test_chinese_argument_passes_through(self):
        result = runner.build_command(["build.bat", r"C:\用户\项目"])
        self.assertIn(r'"C:\用户\项目"', result)


@unittest.skipUnless(os.name == "nt", "cmd.exe 仅在 Windows 上可用")
class BatchExecutionTest(unittest.TestCase):
    """真实跑一遍 .bat，确认参数没有被空格/元字符拆开。"""

    def setUp(self):
        # 目录名同时包含空格与 &，覆盖最容易出错的两种路径
        self.tmp = Path(tempfile.mkdtemp(prefix="udp2cal test & dir "))
        self.bat = self.tmp / "echo args.bat"
        self.bat.write_text(
            "@echo off\r\n"
            "chcp 65001 >nul\r\n"
            ":loop\r\n"
            'if "%~1"=="" goto end\r\n'
            "echo ARG=[%~1]\r\n"
            "shift\r\n"
            "goto loop\r\n"
            ":end\r\n",
            encoding="ascii",
        )

    def tearDown(self):
        shutil.rmtree(str(self.tmp), ignore_errors=True)

    def _args(self, *args):
        result = runner.run([str(self.bat), *args], tail_lines=50)
        self.assertEqual(result.code, 0, result.tail)
        return [line[5:-1] for line in result.lines if line.startswith("ARG=[")]

    def test_spaces_and_ampersand(self):
        args = ["a b", "x&y", "C:\\Program Files\\Sdk"]
        self.assertEqual(self._args(*args), args)

    def test_percent_is_literal(self):
        self.assertEqual(self._args("100%PATH%"), ["100%PATH%"])

    def test_parentheses_and_pipes(self):
        args = ["(x)", "a|b", "c<d", "e>f"]
        self.assertEqual(self._args(*args), args)

    def test_chinese_and_emoji(self):
        self.assertEqual(self._args("中文路径", "emoji😀"), ["中文路径", "emoji😀"])


if __name__ == "__main__":
    unittest.main()
