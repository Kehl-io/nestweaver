#!/usr/bin/env python3
"""Exercise old/new Cargo layouts and refuse absent or ambiguous build evidence."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('openssl_source', Path(__file__).with_name('release-openssl-source.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class OpenSSLSource(unittest.TestCase):
    def test_layouts_and_current_build_only(self):
        with tempfile.TemporaryDirectory() as directory:
            for layout in ('openssl-sys-oldhash/out', 'openssl-sys/newhash/out'):
                output = Path(directory) / layout
                install = output / 'openssl-build/install'
                for name in ('lib/libssl.a', 'lib/libcrypto.a', 'include/openssl/ssl.h',
                             'include/openssl/opensslconf.h', 'lib/pkgconfig/openssl.pc'):
                    path = install / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.touch()
                event = dict(reason='build-script-executed', package_id='registry+https://example/#openssl-sys@0.9.118', out_dir=str(output))
                irrelevant = event | {'package_id': 'registry+https://example/#other@1.0.0'}
                self.assertEqual(module.source([irrelevant, event, event]), install)
                with self.assertRaisesRegex(ValueError, 'found 0'):
                    module.source([irrelevant])
                with self.assertRaisesRegex(ValueError, 'found 2'):
                    module.source([event, event | {'out_dir': directory + '/stale/out'}])
                (install / 'lib/libssl.a').unlink()
                with self.assertRaisesRegex(ValueError, 'missing built OpenSSL file'):
                    module.source([event])


if __name__ == '__main__':
    unittest.main()
