"""build_common 的输出格式与镜像重试行为。"""

import unittest
from pathlib import Path

import support
from udp2cal import build_common, runner
from udp2cal.logger import C


class _CaptureLogger:
    """记录 raw/info/hint/warn/blank，并暴露 path。"""

    def __init__(self, path=None):
        self.path = path
        self.raws = []
        self.infos = []
        self.hints = []
        self.warns = []
        self.blanks = 0

    def raw(self, text):
        self.raws.append(text)

    def info(self, text):
        self.infos.append(text)

    def hint(self, text):
        self.hints.append(text)

    def warn(self, text):
        self.warns.append(text)

    def blank(self):
        self.blanks += 1


class SuccessBoxTest(unittest.TestCase):
    def test_border_is_44_equals_and_lines_are_not_wrapped(self):
        logger = _CaptureLogger()

        build_common.success_box(logger, ["BUILD SUCCESS", "size:   1 KB"])

        border = "=" * 44
        self.assertEqual(logger.raws[0], C.GREEN + border)
        self.assertEqual(
            logger.raws[1:3],
            [C.GREEN + "   BUILD SUCCESS", C.GREEN + "   size:   1 KB"],
        )
        self.assertEqual(logger.raws[-1], C.GREEN + border + C.RESET)
        self.assertEqual(len(logger.raws), 4)
        for text in logger.raws:
            self.assertNotIn("\n", text)

    def test_empty_lines_still_draws_closed_border(self):
        logger = _CaptureLogger()

        build_common.success_box(logger, [])

        self.assertEqual(len(logger.raws), 2)
        self.assertEqual(logger.raws[0], C.GREEN + "=" * 44)
        self.assertEqual(logger.raws[1], C.GREEN + "=" * 44 + C.RESET)


class PrintFailureTailTest(unittest.TestCase):
    def _result(self, lines, code=1):
        return runner.RunResult(code=code, lines=list(lines))

    def test_keyword_matches_are_filtered_and_trimmed(self):
        logger = _CaptureLogger()
        lines = [
            "noise 1",
            "ERROR: something broke",
            "noise 2",
            "  Caused By: dependency  ",
            "noise 3",
            "what went wrong: see above",
        ]

        build_common.print_failure_tail(
            self._result(lines),
            logger,
            ("error", "failed", "caused by", "what went wrong"),
        )

        self.assertEqual(logger.infos, ["关键错误行:"])
        self.assertEqual(
            logger.hints,
            ["ERROR: something broke", "Caused By: dependency", "what went wrong: see above"],
        )

    def test_long_lines_are_truncated_to_200_chars(self):
        logger = _CaptureLogger()
        long_line = "error " + "x" * 300

        build_common.print_failure_tail(self._result([long_line]), logger, ("error",))

        self.assertEqual(len(logger.hints[0]), 200)

    def test_without_matches_falls_back_to_last_15_lines(self):
        logger = _CaptureLogger()
        lines = ["line {}".format(i) for i in range(30)]

        build_common.print_failure_tail(self._result(lines), logger, ("error",))

        self.assertEqual(logger.hints, lines[-15:])

    def test_only_last_15_matches_are_shown(self):
        logger = _CaptureLogger()
        lines = ["error {}".format(i) for i in range(20)]

        build_common.print_failure_tail(self._result(lines), logger, ("error",))

        self.assertEqual(logger.hints, lines[-15:])

    def test_full_log_path_printed_when_available(self):
        logger = _CaptureLogger(path=Path("logs/build.log"))

        build_common.print_failure_tail(self._result(["error x"]), logger, ("error",))

        self.assertIn("完整日志: {}".format(logger.path), logger.infos)

    def test_no_log_path_skips_full_log_line(self):
        logger = _CaptureLogger(path=None)

        build_common.print_failure_tail(self._result(["error x"]), logger, ("error",))

        self.assertNotIn("完整日志", "".join(logger.infos))

    def test_empty_output_prints_nothing(self):
        logger = _CaptureLogger(path=Path("logs/build.log"))

        build_common.print_failure_tail(self._result([]), logger, ("error",))

        self.assertEqual(logger.infos, ["完整日志: {}".format(logger.path)])
        self.assertEqual(logger.hints, [])


class RunWithMirrorTest(unittest.TestCase):
    def _logger(self):
        return _CaptureLogger()

    def test_success_runs_once_without_retry(self):
        calls = []

        def run(cmd, **kwargs):
            calls.append(list(cmd))
            return runner.RunResult(code=0, lines=[])

        logger = self._logger()
        result = build_common.run_with_mirror(
            run, ["build"], ["--mirror"], logger, warn_message="warn", cwd="dir", env={"A": "1"}
        )

        self.assertTrue(result.ok)
        self.assertEqual(calls, [["build"]])
        self.assertEqual(logger.warns, [])

    def test_network_failure_retries_with_mirror_args_and_forwards_kwargs(self):
        calls = []
        seen_kwargs = []

        def run(cmd, **kwargs):
            calls.append(list(cmd))
            seen_kwargs.append(kwargs)
            code = 1 if len(calls) == 1 else 0
            return runner.RunResult(
                code=code,
                lines=["could not resolve host: repo.maven.apache.org"] if code else [],
            )

        logger = self._logger()
        result = build_common.run_with_mirror(
            run,
            ["build"],
            ["--mirror"],
            logger,
            warn_message="switch mirror",
            cwd="dir",
            env={"A": "1"},
            tail_lines=50,
        )

        self.assertTrue(result.ok)
        self.assertEqual(calls, [["build"], ["build", "--mirror"]])
        self.assertEqual(logger.blanks, 1)
        self.assertEqual(logger.warns, ["switch mirror"])
        for kwargs in seen_kwargs:
            self.assertEqual(kwargs, {"logger": logger, "cwd": "dir", "env": {"A": "1"}, "tail_lines": 50})

    def test_non_network_failure_does_not_retry(self):
        calls = []

        def run(cmd, **kwargs):
            calls.append(list(cmd))
            return runner.RunResult(code=1, lines=["syntax error"])

        logger = self._logger()
        result = build_common.run_with_mirror(
            run, ["build"], ["--mirror"], logger, warn_message="warn"
        )

        self.assertFalse(result.ok)
        self.assertEqual(calls, [["build"]])
        self.assertEqual(logger.warns, [])

    def test_lazy_mirror_args_returning_none_skips_retry(self):
        calls = []

        def run(cmd, **kwargs):
            calls.append(list(cmd))
            return runner.RunResult(code=1, lines=["could not resolve host: crate.io"])

        logger = self._logger()
        result = build_common.run_with_mirror(
            run, ["build"], lambda: None, logger, warn_message="switch mirror"
        )

        self.assertFalse(result.ok)
        self.assertEqual(calls, [["build"]])
        self.assertEqual(logger.warns, ["switch mirror"])

    def test_inline_command_list_is_not_mutated(self):
        cmd = ["build"]

        def run(argv, **kwargs):
            return runner.RunResult(code=1, lines=["could not resolve host: crate.io"])

        build_common.run_with_mirror(
            run, cmd, ["--mirror"], self._logger(), warn_message="warn"
        )

        self.assertEqual(cmd, ["build"])


if __name__ == "__main__":
    unittest.main()
