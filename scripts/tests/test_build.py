"""Doctor exit status must reflect build-blocking environment problems."""

import os
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import support
import build
from udp2cal import config, env as envmod


class _DoctorLogger(support.FakeLogger):
    def __init__(self):
        super().__init__()
        self.path = Path("doctor.log")

    def banner(self, message):
        self._log("banner", message)

    def section(self, message):
        self._log("section", message)

    def blank(self):
        self._log("blank", "")

    def close(self):
        pass


class DoctorExitCodeTest(unittest.TestCase):
    def setUp(self):
        self.jdk = envmod.JavaInfo(Path("jdk"), version="21", major=21)
        self.sdk = envmod.SdkInfo(Path("sdk"), source="test")
        self.rust = envmod.RustInfo(Path("cargo"), Path("rustc"), Path("rustup"), "test")
        self.targets = [*config.WINDOWS_TARGETS.values(), *config.ANDROID_RUST_TARGETS.values()]

    def _run_doctor(
        self,
        *,
        sdks=None,
        rust=None,
        cmake=(Path("cmake"), "3.22.1"),
        msvc=Path("msvc"),
        adb=Path("adb"),
        missing_components=(),
        installed_targets=None,
    ):
        logger = _DoctorLogger()
        with patch.object(build, "make_logger", return_value=logger), \
                patch.object(build.proxymod, "report"), \
                patch.object(build.envmod, "detect_javas", return_value=[self.jdk]), \
                patch.object(build.envmod, "detect_sdks", return_value=[self.sdk] if sdks is None else sdks), \
                patch.object(build.envmod, "detect_rust", return_value=self.rust if rust is None else rust), \
                patch.object(build.envmod, "detect_cmake", return_value=cmake), \
                patch.object(build.envmod, "detect_msvc", return_value=msvc), \
                patch.object(build.envmod, "detect_adb", return_value=adb), \
                patch.object(build.envmod, "missing_components", return_value=list(missing_components)), \
                patch.object(
                    build.envmod,
                    "installed_rust_targets",
                    return_value=self.targets if installed_targets is None else installed_targets,
                ), \
                patch.object(build, "_check_projects", return_value=0):
            code = build.doctor(SimpleNamespace(
                verbose=False, java_home=None, sdk_dir=None, adb=None, fix=False
            ))
        return code, logger

    def test_missing_required_tools_return_nonzero(self):
        code, logger = self._run_doctor(sdks=[], rust=False, cmake=(None, ""), msvc=None)

        self.assertEqual(code, 1)
        self.assertIn("未找到 Android SDK", logger.texts("error"))

    def test_missing_sdk_component_returns_nonzero(self):
        code, _ = self._run_doctor(missing_components=("ndk;27.0.12077973",))

        self.assertEqual(code, 1)

    def test_fix_installs_missing_sdk_components_and_rechecks(self):
        with tempfile.TemporaryDirectory() as tmp:
            jdk_home = Path(tmp)
            (jdk_home / "bin").mkdir()
            jdk = envmod.JavaInfo(jdk_home, version="21", major=21)
            logger = _DoctorLogger()
            missing = ["ndk;27.0.12077973"]
            installed = []

            def install(sdk, target, _logger, env=None):
                installed.append((target, env))
                missing.clear()
                return True

            with patch.object(build, "make_logger", return_value=logger), \
                    patch.object(build.proxymod, "report"), \
                    patch.object(build.envmod, "detect_javas", return_value=[jdk]), \
                    patch.object(build.envmod, "detect_sdks", return_value=[self.sdk]), \
                    patch.object(build.envmod, "detect_rust", return_value=self.rust), \
                    patch.object(build.envmod, "detect_cmake", return_value=(Path("cmake"), "3.22.1")), \
                    patch.object(build.envmod, "detect_msvc", return_value=Path("msvc")), \
                    patch.object(build.envmod, "detect_adb", return_value=Path("adb")), \
                    patch.object(build.envmod, "missing_components", side_effect=lambda *_: list(missing)), \
                    patch.object(build.envmod, "installed_rust_targets", return_value=self.targets), \
                    patch.object(build, "_check_projects", return_value=0), \
                    patch.object(build.deps, "ensure_gradle_wrapper", return_value=True), \
                    patch.object(build.deps, "resolve_sdk", return_value=self.sdk), \
                    patch.object(build.deps, "ensure_local_properties"), \
                    patch.object(build.deps, "ensure_sdk_components", side_effect=install), \
                    patch.object(build.deps, "ensure_rust_target", return_value=True):
                code = build.doctor(SimpleNamespace(
                    verbose=False, java_home=None, sdk_dir=None, adb=None, fix=True
                ))

        self.assertEqual(code, 0)
        self.assertEqual(len(installed), 1)
        self.assertEqual(installed[0][0], "modern")
        self.assertEqual(installed[0][1]["JAVA_HOME"], str(jdk_home))
        self.assertIn(str(jdk_home / "bin"), installed[0][1]["PATH"].split(os.pathsep))

    def test_components_follow_local_properties_sdk(self):
        sdk_b = envmod.SdkInfo(Path("sdk-b"), source="local.properties")
        missing_for = lambda sdk, _key: ["ndk;27.0.12077973"] if sdk is sdk_b else []
        logger = _DoctorLogger()

        with patch.object(build, "make_logger", return_value=logger), \
                patch.object(build.proxymod, "report"), \
                patch.object(build.envmod, "detect_javas", return_value=[self.jdk]), \
                patch.object(build.envmod, "detect_sdks", return_value=[self.sdk]), \
                patch.object(build.envmod, "detect_rust", return_value=self.rust), \
                patch.object(build.envmod, "detect_cmake", return_value=(Path("cmake"), "3.22.1")), \
                patch.object(build.envmod, "detect_msvc", return_value=Path("msvc")), \
                patch.object(build.envmod, "detect_adb", return_value=Path("adb")), \
                patch.object(build.envmod, "missing_components", side_effect=missing_for), \
                patch.object(build.envmod, "installed_rust_targets", return_value=self.targets), \
                patch.object(build, "_check_projects", return_value=0), \
                patch.object(build.deps, "resolve_sdk", return_value=sdk_b):
            code = build.doctor(SimpleNamespace(
                verbose=False, java_home=None, sdk_dir=None, adb=None, fix=False
            ))

        self.assertEqual(code, 1)
        self.assertTrue(any("ndk;27.0.12077973" in text for text in logger.texts("warn")))

    def test_fix_installs_into_local_properties_sdk_with_java_env(self):
        with tempfile.TemporaryDirectory() as tmp:
            jdk_home = Path(tmp)
            (jdk_home / "bin").mkdir()
            jdk = envmod.JavaInfo(jdk_home, version="21", major=21)
            sdk_b = envmod.SdkInfo(Path("sdk-b"), source="local.properties")
            missing = {sdk_b.root: ["ndk;27.0.12077973"]}
            installed = []

            def missing_for(sdk, _key):
                return list(missing.get(sdk.root, []))

            def install(sdk, target, _logger, env=None):
                installed.append((sdk, target, env))
                missing.clear()
                return True

            logger = _DoctorLogger()
            with patch.object(build, "make_logger", return_value=logger), \
                    patch.object(build.proxymod, "report"), \
                    patch.object(build.envmod, "detect_javas", return_value=[jdk]), \
                    patch.object(build.envmod, "detect_sdks", return_value=[self.sdk]), \
                    patch.object(build.envmod, "detect_rust", return_value=self.rust), \
                    patch.object(build.envmod, "detect_cmake", return_value=(Path("cmake"), "3.22.1")), \
                    patch.object(build.envmod, "detect_msvc", return_value=Path("msvc")), \
                    patch.object(build.envmod, "detect_adb", return_value=Path("adb")), \
                    patch.object(build.envmod, "missing_components", side_effect=missing_for), \
                    patch.object(build.envmod, "installed_rust_targets", return_value=self.targets), \
                    patch.object(build, "_check_projects", return_value=0), \
                    patch.object(build.deps, "resolve_sdk", return_value=sdk_b), \
                    patch.object(build.deps, "ensure_gradle_wrapper", return_value=True), \
                    patch.object(build.deps, "ensure_local_properties"), \
                    patch.object(build.deps, "ensure_sdk_components", side_effect=install), \
                    patch.object(build.deps, "ensure_rust_target", return_value=True):
                code = build.doctor(SimpleNamespace(
                    verbose=False, java_home=None, sdk_dir=None, adb=None, fix=True
                ))

        self.assertEqual(code, 0)
        self.assertEqual(len(installed), 1)
        sdk, target, env = installed[0]
        self.assertIs(sdk, sdk_b)
        self.assertEqual(target, "modern")
        self.assertEqual(env["JAVA_HOME"], str(jdk_home))
        self.assertIn(str(jdk_home / "bin"), env["PATH"].split(os.pathsep))

    def test_missing_rust_target_returns_nonzero(self):
        code, _ = self._run_doctor(installed_targets=self.targets[:-1])

        self.assertEqual(code, 1)

    def test_cmake_below_project_minimum_returns_nonzero(self):
        code, _ = self._run_doctor(cmake=(Path("cmake"), "3.21.9"))

        self.assertEqual(code, 1)

    def test_ready_environment_returns_zero(self):
        code, _ = self._run_doctor()

        self.assertEqual(code, 0)


if __name__ == "__main__":
    unittest.main()
