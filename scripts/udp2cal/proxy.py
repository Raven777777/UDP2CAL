"""系统代理探测。

探测来源按可靠性排序：注册表 → 环境变量 → 常见本地端口探活。
默认策略为 direct（不注入代理），仅在显式要求时把代理下发给 Gradle / Cargo，
避免 TUN 模式下"代理套代理"导致的构建阻塞。
"""

from __future__ import annotations

import os
import socket
from dataclasses import dataclass
from typing import List, Optional, Tuple

from . import config

_REG_PATH = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings"


@dataclass
class ProxyInfo:
    """一个可用的 HTTP 代理。"""

    host: str
    port: int
    source: str

    @property
    def url(self) -> str:
        return "http://{}:{}".format(self.host, self.port)

    def describe(self) -> str:
        return "{} (来源: {})".format(self.url, self.source)


def _parse_server(value: str) -> Optional[Tuple[str, int]]:
    """解析注册表 ProxyServer 字段。

    支持两种格式：
        "127.0.0.1:10808"
        "http=127.0.0.1:10808;https=127.0.0.1:10808"
    """
    if not value:
        return None
    entries = [v.strip() for v in value.split(";") if v.strip()]
    candidate = None
    for entry in entries:
        if "=" in entry:
            scheme, _, addr = entry.partition("=")
            if scheme.strip().lower() in ("http", "https"):
                candidate = addr.strip()
                break
        elif candidate is None:
            candidate = entry
    if not candidate or ":" not in candidate:
        return None
    host, _, port_text = candidate.rpartition(":")
    try:
        return host.strip() or "127.0.0.1", int(port_text)
    except ValueError:
        return None


def from_registry() -> Optional[ProxyInfo]:
    if os.name != "nt":
        return None
    try:
        import winreg

        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, _REG_PATH) as key:
            enabled = winreg.QueryValueEx(key, "ProxyEnable")[0]
            server = winreg.QueryValueEx(key, "ProxyServer")[0] if enabled else ""
    except (OSError, ImportError, FileNotFoundError):
        return None
    parsed = _parse_server(str(server))
    if not parsed:
        return None
    return ProxyInfo(host=parsed[0], port=parsed[1], source="Windows 系统代理")


def pac_url() -> Optional[str]:
    """返回系统自动配置脚本（PAC）地址，仅用于提示。"""
    if os.name != "nt":
        return None
    try:
        import winreg

        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, _REG_PATH) as key:
            value = winreg.QueryValueEx(key, "AutoConfigURL")[0]
            return str(value) if value else None
    except (OSError, ImportError, FileNotFoundError):
        return None


def from_env() -> Optional[ProxyInfo]:
    for key in config.PROXY_ENV_KEYS:
        raw = os.environ.get(key)
        if not raw:
            continue
        text = raw.strip()
        scheme = ""
        if "://" in text:
            scheme, _, text = text.partition("://")
            scheme = scheme.strip().lower()
        if scheme and scheme not in ("http", "https"):
            continue  # 只支持 HTTP(S) 代理，socks 等直接跳过
        text = text.rstrip("/")
        if "@" in text:  # 不支持带认证信息的代理，跳过
            continue
        if ":" not in text:
            continue
        host, _, port_text = text.rpartition(":")
        try:
            return ProxyInfo(host=host or "127.0.0.1", port=int(port_text), source="环境变量 {}".format(key))
        except ValueError:
            continue
    return None


def _is_open(host: str, port: int, timeout: float = 0.2) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


def from_local_ports() -> Optional[ProxyInfo]:
    """系统未声明代理时，探测常见本地代理端口。"""
    for port in config.COMMON_PROXY_PORTS:
        if _is_open("127.0.0.1", port):
            return ProxyInfo(host="127.0.0.1", port=port, source="本地端口探测")
    return None


def detect_all() -> List[ProxyInfo]:
    """按优先级返回所有探测结果（去重）。"""
    found: List[ProxyInfo] = []
    seen = set()
    for item in (from_registry(), from_env(), from_local_ports()):
        if item is None:
            continue
        key = (item.host, item.port)
        if key in seen:
            continue
        seen.add(key)
        found.append(item)
    return found


def parse_manual(text: str) -> Optional[ProxyInfo]:
    text = (text or "").strip()
    if not text:
        return None
    if text.lower() in ("none", "off", "direct"):
        return None
    if "://" in text:
        scheme, _, text = text.partition("://")
        if scheme.strip().lower() not in ("http", "https"):
            return None  # 仅支持 HTTP(S) 代理
    text = text.rstrip("/")
    if ":" not in text:
        text = text + ":1080"
    host, _, port_text = text.rpartition(":")
    try:
        return ProxyInfo(host=host or "127.0.0.1", port=int(port_text), source="命令行指定")
    except ValueError:
        return None


def resolve(mode: str, logger) -> Optional[ProxyInfo]:
    """按用户策略解析出最终要使用的代理。

    mode:
        direct      不使用代理（默认）
        auto        使用探测到的第一个可用代理
        <host:port> 使用指定代理
    """
    normalized = (mode or "direct").strip().lower()
    if normalized in ("direct", "none", "off"):
        return None

    if normalized == "auto":
        candidates = detect_all()
        if not candidates:
            logger.warn("--proxy auto 已指定，但未探测到可用代理，本次回退直连")
            return None
        chosen = candidates[0]
        logger.info("代理: {}".format(chosen.describe()))
        if not _is_open(chosen.host, chosen.port):
            logger.warn("代理端口当前不可连接，仍按配置下发；如构建失败请改用 --proxy direct")
        return chosen

    manual = parse_manual(mode)
    if manual is None:
        logger.warn("无法解析代理参数 '{}'，回退直连".format(mode))
        return None
    logger.info("代理: {}".format(manual.describe()))
    return manual


def jvm_args(proxy: Optional[ProxyInfo]) -> List[str]:
    """Gradle / JVM 使用的代理参数。

    注意 nonProxyHosts 的值里含 '|'：在 Windows 上这些参数会被写进 GRADLE_OPTS，
    再由 gradlew.bat 在 cmd 里展开，未加引号时 '|' 会被 cmd 当成管道符，
    于是出现 "'127.0.0.1' 不是内部或外部命令"。
    因此整条参数必须带引号。
    """
    if proxy is None:
        return []
    return [
        "-Dhttp.proxyHost={}".format(proxy.host),
        "-Dhttp.proxyPort={}".format(proxy.port),
        "-Dhttps.proxyHost={}".format(proxy.host),
        "-Dhttps.proxyPort={}".format(proxy.port),
        '"-Dhttp.nonProxyHosts=localhost|127.0.0.1|::1"',
    ]


def apply_env(env: dict, proxy: Optional[ProxyInfo]) -> dict:
    """为子进程设置代理环境变量；proxy 为 None 时主动清除，避免继承残留配置。"""
    keys = ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy")
    if proxy is None:
        for key in keys:
            env.pop(key, None)
        env["NO_PROXY"] = "*"
        env["no_proxy"] = "*"
        return env

    for key in keys:
        env[key] = proxy.url
    env["NO_PROXY"] = "localhost,127.0.0.1"
    env["no_proxy"] = "localhost,127.0.0.1"
    return env


def report(logger) -> None:
    """doctor 模式下的代理体检输出。"""
    registry = from_registry()
    env_proxy = from_env()
    local = from_local_ports()
    pac = pac_url()

    if registry:
        logger.ok("系统代理已启用: {}".format(registry.describe()))
    else:
        logger.info("系统代理: 未启用（注册表 ProxyEnable=0 或未配置）")

    if env_proxy:
        logger.info("环境变量代理: {}".format(env_proxy.describe()))
    else:
        logger.detail("环境变量代理: 未设置")

    if local:
        logger.info("本地代理端口: {} 可连接".format(local.url))
    else:
        logger.detail("本地代理端口: 未探测到常见端口")

    if pac:
        logger.detail("PAC 脚本: {}".format(pac))

    if registry or env_proxy or local:
        logger.hint("默认仍走直连+镜像；需要走代理时加 --proxy auto")
