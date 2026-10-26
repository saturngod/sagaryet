import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
FILES = ["package.json", "src-tauri/tauri.conf.json", "src-tauri/Cargo.toml", "src-tauri/Cargo.lock"]


@unittest.skipUnless(shutil.which("bun"), "Bun is required")
class ReleaseVersionTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        for name in [*FILES, "scripts/release-version.ts"]:
            destination = self.root / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, destination)

    def run_command(self, *args):
        return subprocess.run([shutil.which("bun"), *args], cwd=self.root,
                              capture_output=True, text=True, timeout=20)

    def snapshot(self):
        return {name: (self.root / name).read_bytes() for name in FILES}

    def test_release_updates_all_files_and_is_repeatable(self):
        for version in ["0.2.3", "v0.2.3"]:
            result = self.run_command("run", "release", version)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            result = self.run_command("scripts/release-version.ts", "check", "--tag", "v0.2.3")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(json.loads((self.root / "package.json").read_text())["version"], "0.2.3")
        self.assertFalse((self.root / ".git").exists())

    def test_invalid_arguments_do_not_change_files(self):
        before = self.snapshot()
        for args in [[], ["invalid"], ["0.2.3", "extra"], ["0.2.3", "--push"]]:
            result = self.run_command("run", "release", *args)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(before, self.snapshot())

    def test_missing_lockfile_fails_before_changes(self):
        before = self.snapshot()
        (self.root / "src-tauri/Cargo.lock").unlink()
        result = self.run_command("run", "release", "0.2.3")
        self.assertNotEqual(result.returncode, 0)
        for name in FILES[:-1]:
            self.assertEqual(before[name], (self.root / name).read_bytes())
