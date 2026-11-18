"""手动代理不接受认证信息，且失败日志不得回显原始参数。"""

import unittest
from unittest import mock

import support
from udp2cal import proxy

FakeLogger = support.FakeLogger


class ManualProxyTest(unittest.TestCase):
    def test_credentials_are_rejected(self):
        for text in ("user:secret@127.0.0.1:8080", "http://user:secret@127.0.0.1:8080"):
            self.assertIsNone(proxy.parse_manual(text), text)

    def test_plain_host_port_still_parses(self):
        info = proxy.parse_manual("127.0.0.1:8080")
        self.assertEqual((info.host, info.port), ("127.0.0.1", 8080))

    def test_registry_proxy_credentials_are_rejected(self):
        self.assertIsNone(proxy._parse_server("http=user:secret@127.0.0.1:8080"))

    def test_pac_report_redacts_credentials(self):
        logger = FakeLogger()
        with mock.patch.object(proxy, "from_registry", return_value=None), \
                mock.patch.object(proxy, "from_env", return_value=None), \
                mock.patch.object(proxy, "from_local_ports", return_value=None), \
                mock.patch.object(proxy, "pac_url", return_value="http://user:secret@example.test/proxy.pac"):
            proxy.report(logger)
        self.assertNotIn("secret", repr(logger.messages))

    def test_resolve_does_not_log_credentials(self):
        logger = FakeLogger()
        self.assertIsNone(proxy.resolve("user:secret@127.0.0.1:8080", logger))
        self.assertNotIn("secret", repr(logger.messages))
        self.assertTrue(any("认证信息" in text for text in logger.texts("warn")))


if __name__ == "__main__":
    unittest.main()
