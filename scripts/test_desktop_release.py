import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("release", Path(__file__).with_name("desktop-release.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def test_version_orders_successive_runs_without_mutating_core_versions(self):
        self.assertEqual(release.version("123"), "0.1.123")
        for invalid in ["0", "-1", "1;echo x", "1.2", ""]:
            with self.assertRaises(ValueError):
                release.version(invalid)

    def test_manifest_contains_only_the_private_asset_and_signature(self):
        url = "https://api.github.com/repos/oppenxyz/oppen/releases/assets/42"
        result = release.metadata("0.1.123", url, "signature\n")
        self.assertEqual(result["platforms"]["darwin-aarch64"], {"url": url, "signature": "signature"})
        for invalid in ["https://evil.test/42", url + "?token=secret", url + "/../43"]:
            with self.assertRaises(ValueError):
                release.metadata("0.1.123", invalid, "signature")
        with self.assertRaises(ValueError):
            release.metadata("0.1.123", url, "")
