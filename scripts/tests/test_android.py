"""Gradle 调用路径与 --no-install-deps 行为。"""

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import support
from udp2cal import android, env as envmod, runner

FakeLogger = support.FakeLogger


class _Logger(FakeLogger):
    def banner(self, message):
        self._log("banner", message)

    def step(self, index, total, message):
        self._log("step", message)

    def blank(self):
        self._log("blank", "")


class FindApkTest(unittest.TestCase):
    def test_missing_build_type_returns_none(self):
        # 只有 Release APK 时，找 Debug 不应回退到任意最新 APK
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            release = project / "app" / "build" / "outputs" / "apk" / "release"
            release.mkdir(parents=True)
            (release / "app-release.apk").write_bytes(b"apk")
            self.assertIsNone(android.find_apk(project, "Debug"))

    def test_matching_build_type_is_found(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            debug = project / "app" / "build" / "outputs" / "apk" / "debug"
            debug.mkdir(parents=True)
            apk = debug / "app-debug.apk"
            apk.write_bytes(b"apk")
            self.assertEqual(android.find_apk(project, "Debug"), apk)


class AndroidBuildTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.project = self.root / "android"
        self.project.mkdir()
        (self.project / "gradlew.bat").write_text("@echo off\n", encoding="ascii")
        self.apk = self.root / "app-debug.apk"
        self.apk.write_bytes(b"apk")
        self.java = envmod.JavaInfo(self.root / "jdk", version="21", major=21)
        self.sdk = envmod.SdkInfo(self.root / "sdk", source="test")
        self.target = {"dir": self.project, "prefix": "udp2cal", "label": "test"}
        self.logger = _Logger()

    def tearDown(self):
        self._tmp.cleanup()

    def _build(self, install_deps):
        calls = []
        rust_calls = []

        def fake_run(cmd, **kwargs):
            calls.append(list(cmd))
            return runner.RunResult(code=0, lines=[])

        def fake_target(rust, triple, logger, env=None):
            rust_calls.append((triple, env))
            return True

        ctx = SimpleNamespace(
            target="modern",
            abi="arm64-v8a",
            build_type="Debug",
            install_deps=install_deps,
            java_home=None,
            sdk_dir=None,
            proxy_mode="direct",
            mirror_mode="off",
        )
        with patch.dict(android.config.ANDROID_TARGETS, {"modern": self.target}), \
                patch.object(android.config, "ROOT", self.root), \
                patch.object(android, "preflight", return_value={"java": self.java, "sdk": self.sdk}), \
                patch.object(android.deps, "resolve_sdk", return_value=self.sdk), \
                patch.object(android.proxymod, "resolve", return_value=None), \
                patch.object(android.deps, "ensure_gradle_wrapper", return_value=True), \
                patch.object(android.deps, "ensure_local_properties", return_value=True), \
                patch.object(android.deps, "ensure_sdk_components", return_value=True), \
                patch.object(android.deps, "ensure_rust_target", side_effect=fake_target), \
                patch.object(
                    android.envmod,
                    "detect_rust",
                    return_value=envmod.RustInfo(self.root / "cargo", self.root / "rustc"),
                ), \
                patch.object(android.deps, "heal_daemon_registry"), \
                patch.object(android, "_gradle_env", return_value={}), \
                patch.object(android, "_success_box"), \
                patch.object(android, "find_apk", return_value=self.apk), \
                patch.object(android.runner, "run", side_effect=fake_run):
            code = android.build(ctx, self.logger)
        return code, calls, rust_calls

    def test_gradlew_is_invoked_by_absolute_path(self):
        code, calls, _ = self._build(install_deps=True)

        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 1)
        gradlew = Path(calls[0][0])
        self.assertTrue(gradlew.is_absolute())
        self.assertEqual(gradlew, self.project / "gradlew.bat")

    def test_no_install_deps_skips_rust_target(self):
        code, _, rust_calls = self._build(install_deps=False)

        self.assertEqual(code, 0)
        self.assertEqual(rust_calls, [])

    def test_rust_target_install_receives_env(self):
        code, _, rust_calls = self._build(install_deps=True)

        self.assertEqual(code, 0)
        self.assertEqual(len(rust_calls), 1)
        self.assertIsNotNone(rust_calls[0][1])


class WrapperCheckTest(unittest.TestCase):
    def test_missing_gradlew_bat_is_incomplete(self):
        from udp2cal import deps

        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            wrapper = project / "gradle" / "wrapper"
            wrapper.mkdir(parents=True)
            (wrapper / "gradle-wrapper.jar").write_bytes(b"jar")
            (wrapper / "gradle-wrapper.properties").write_text("distributionUrl=x", encoding="ascii")
            self.assertFalse(deps.wrapper_ready(project))
            (project / "gradlew.bat").write_text("@echo off\n", encoding="ascii")
            self.assertTrue(deps.wrapper_ready(project))


if __name__ == "__main__":
    unittest.main()
