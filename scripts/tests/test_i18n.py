"""多语言资源一致性：中英文 key、占位符与语言索引。"""

import re
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RES = ROOT / "android" / "app" / "src" / "main" / "res"
PLACEHOLDER = re.compile(r"%(?:\d+\$)?[a-zA-Z]")


def _read(path: Path):
    root = ET.parse(path).getroot()
    strings = {e.get("name"): (e.text or "") for e in root.findall("string")}
    plurals = {
        e.get("name"): [item.text or "" for item in e.findall("item")]
        for e in root.findall("plurals")
    }
    return strings, plurals


def _placeholders(text: str):
    return sorted(PLACEHOLDER.findall(text))


class AndroidLocaleTest(unittest.TestCase):
    def setUp(self):
        self.en_strings, self.en_plurals = _read(RES / "values" / "strings.xml")
        self.zh_strings, self.zh_plurals = _read(RES / "values-zh" / "strings.xml")

    def test_locales_have_same_keys(self):
        self.assertEqual(set(self.en_strings), set(self.zh_strings))
        self.assertEqual(set(self.en_plurals), set(self.zh_plurals))

    def test_placeholders_match(self):
        for name in self.en_strings:
            self.assertEqual(
                _placeholders(self.en_strings[name]),
                _placeholders(self.zh_strings[name]),
                "占位符不一致: {}".format(name),
            )
        for name in self.en_plurals:
            self.assertEqual(
                {p for text in self.en_plurals[name] for p in _placeholders(text)},
                {p for text in self.zh_plurals[name] for p in _placeholders(text)},
                "复数占位符不一致: {}".format(name),
            )

    def test_language_tags_have_resources(self):
        root = ET.parse(RES / "values" / "arrays.xml").getroot()
        tags = [item.text for item in root.find("string-array").findall("item")]
        self.assertIn("en", tags)
        self.assertIn("zh", tags)
        for tag in tags:
            folder = RES / "values" if tag == "en" else RES / "values-{}".format(tag)
            self.assertTrue(folder.is_dir(), "缺少资源目录: {}".format(folder))
            strings, _ = _read(folder / "strings.xml")
            self.assertIn("language_name", strings, "{} 缺少 language_name".format(tag))

    def test_language_name_is_native(self):
        self.assertEqual(self.en_strings["language_name"], "English")
        self.assertEqual(self.zh_strings["language_name"], "简体中文")


if __name__ == "__main__":
    unittest.main()
