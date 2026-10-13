"""Exercise production assembly with synthetic archives; never contact npm."""
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]
VERSION = '2.9.0'
PLATFORMS = [
    ('linux-x64', 'x86_64-unknown-linux-gnu', 'linux', 'x64', False),
    ('darwin-x64', 'x86_64-apple-darwin', 'darwin', 'x64', False),
    ('darwin-arm64', 'aarch64-apple-darwin', 'darwin', 'arm64', False),
    ('win32-x64', 'x86_64-pc-windows-msvc', 'win32', 'x64', True),
]


class AssemblyTest(unittest.TestCase):
    def test_all_platforms_wrappers_and_missing_asset(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'dist').mkdir()
            for wrapper in ['kimetsu', 'kimetsu-remote']:
                shutil.copytree(ROOT / 'npm' / wrapper, root / 'npm' / wrapper)
            expected = []
            for remote in [False, True]:
                for key, target, system, cpu, windows in PLATFORMS:
                    if remote and key == 'darwin-x64':
                        continue
                    binary = ('kimetsu-remote' if remote else 'kimetsu') + ('.exe' if windows else '')
                    stem = f'kimetsu-remote-{VERSION}-{target}' if remote else f'kimetsu-{VERSION}-{target}-lean'
                    payload = f'fixture:{stem}'.encode()
                    if windows:
                        # Match the backslash entries from PowerShell Compress-Archive.
                        with zipfile.ZipFile(root / 'dist' / f'{stem}.zip', 'w') as archive:
                            archive.writestr(f'{stem}\\{binary}', payload)
                    else:
                        with tarfile.open(root / 'dist' / f'{stem}.tar.gz', 'w:gz') as archive:
                            info = tarfile.TarInfo(f'{stem}/{binary}')
                            info.size = len(payload)
                            archive.addfile(info, io.BytesIO(payload))
                    expected.append((remote, key, system, cpu, binary, payload))
            command = [os.environ.get('KIMETSU_TEST_BASH', 'bash'),
                       (ROOT / 'scripts/assemble-npm.sh').as_posix(), VERSION]
            subprocess.run(command, cwd=root, check=True)
            for remote, key, system, cpu, binary, payload in expected:
                directory = root / ('stage-remote' if remote else 'stage') / key
                manifest = json.loads((directory / 'package.json').read_text())
                self.assertEqual(manifest['name'], '@kimetsu-ai/' + ('remote-' if remote else '') + key)
                self.assertEqual(manifest['version'], VERSION)
                self.assertEqual(manifest['os'], [system])
                self.assertEqual(manifest['cpu'], [cpu])
                self.assertEqual((directory / 'bin' / binary).read_bytes(), payload)
                if os.name != 'nt' and system != 'win32':
                    self.assertTrue((directory / 'bin' / binary).stat().st_mode & 0o111)
            for wrapper, count in [('kimetsu', 4), ('kimetsu-remote', 3)]:
                manifest = json.loads((root / 'npm' / wrapper / 'package.json').read_text())
                self.assertEqual(manifest['version'], VERSION)
                self.assertEqual(len(manifest['optionalDependencies']), count)
                self.assertEqual(set(manifest['optionalDependencies'].values()), {VERSION})
            (root / 'dist' / f'kimetsu-{VERSION}-x86_64-unknown-linux-gnu-lean.tar.gz').unlink()
            result = subprocess.run(command, cwd=root, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('missing lean artifact', result.stdout)


if __name__ == '__main__':
    unittest.main()
