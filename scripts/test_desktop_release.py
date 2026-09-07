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

    def test_draft_assets_use_cli_lookup_and_publish_only_after_manifest_upload(self):
        import json
        import os
        import plistlib
        import tempfile
        from unittest.mock import patch
        from subprocess import CalledProcessError
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / 'target/aarch64-apple-darwin/release/bundle/macos'
            (bundle / 'oppen.app/Contents').mkdir(parents=True)
            with (bundle / 'oppen.app/Contents/Info.plist').open('wb') as output:
                plistlib.dump({'CFBundleShortVersionString': '0.1.114'}, output)
            (bundle / 'oppen.app.tar.gz').write_bytes(b'signed archive fixture')
            (bundle / 'oppen.app.tar.gz.sig').write_text('signature')
            calls = []
            fail_manifest = False
            def github(*args):
                calls.append(args)
                if args[0] == 'api':
                    self.assertNotIn('/tags/', args[1], 'drafts cannot be looked up through the tag API')
                    return '[[]]'
                if args[:2] == ('release', 'view'):
                    return json.dumps({'assets': [{'name': 'oppen.app.tar.gz', 'apiUrl': 'https://api.github.com/repos/oppenxyz/oppen/releases/assets/42'}]})
                if args[:2] == ('release', 'upload') and str(bundle / 'latest.json') in args:
                    self.assertEqual(json.loads((bundle / 'latest.json').read_text())['version'], '0.1.114')
                    if fail_manifest:
                        raise CalledProcessError(1, 'upload')
                return ''
            with patch.object(release, 'ROOT', root), patch.object(release, 'gh', github), patch.object(release.subprocess, 'run'), patch.dict(os.environ, {'GITHUB_SHA': 'a' * 40}):
                release.publish('114')
                self.assertEqual(calls[-1][:2], ('release', 'edit'))
                self.assertIn('--draft=false', calls[-1])
                calls.clear()
                fail_manifest = True
                with self.assertRaises(CalledProcessError):
                    release.publish('114')
                self.assertFalse(any(call[:2] == ('release', 'edit') for call in calls))
