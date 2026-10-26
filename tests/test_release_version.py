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
        self.root = Path(self.directory.name) / "checkout"
        self.root.mkdir()
        for name in [*FILES, "scripts/release-version.ts"]:
            destination = self.root / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, destination)

        self.git("init")
        self.git("config", "user.name", "Release Test")
        self.git("config", "user.email", "release@example.test")
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "tag.gpgsign", "false")
        self.git("config", "core.hooksPath", str(self.root / "empty-hooks"))
        self.git("add", "--all")
        self.git("commit", "-m", "Initial fixture")
        self.remote = Path(self.directory.name) / "remote.git"
        subprocess.run(["git", "init", "--bare", str(self.remote)], check=True, capture_output=True)
        self.git("remote", "add", "origin", str(self.remote))

    def git(self, *args):
        result = subprocess.run(["git", *args], cwd=self.root, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout.strip()

    def run_command(self, *args):
        return subprocess.run([shutil.which("bun"), *args], cwd=self.root,
                              capture_output=True, text=True, timeout=20)

    def snapshot(self):
        return {name: (self.root / name).read_bytes() for name in FILES}

    def test_release_commits_all_changes_and_pushes_branch_and_tag(self):
        (self.root / "new-file.txt").write_text("Include untracked changes")
        for version in ["0.2.3", "v0.2.4"]:
            result = self.run_command("run", "release", version)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            tag = "v" + version.removeprefix("v")
            result = self.run_command("scripts/release-version.ts", "check", "--tag", tag, "--ref", tag)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(self.git("cat-file", "-t", tag), "tag")
            head = self.git("rev-parse", "HEAD")
            self.assertTrue(self.git("ls-remote", "origin", f"refs/tags/{tag}^{{}}").startswith(head))
            branch = self.git("branch", "--show-current")
            self.assertTrue(self.git("ls-remote", "origin", f"refs/heads/{branch}").startswith(head))
            self.assertEqual(self.git("status", "--porcelain"), "")
        self.assertEqual(self.git("show", "HEAD:new-file.txt"), "Include untracked changes")
        self.assertEqual(json.loads((self.root / "package.json").read_text())["version"], "0.2.4")

    def test_existing_local_or_remote_tag_fails_before_changes(self):
        before = self.snapshot()
        head = self.git("rev-parse", "HEAD")
        self.git("tag", "v0.2.3")
        result = self.run_command("run", "release", "0.2.3")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already exists", result.stderr)
        self.git("push", "origin", "HEAD:refs/tags/v0.2.4")
        result = self.run_command("run", "release", "0.2.4")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already exists on origin", result.stderr)
        self.assertEqual(before, self.snapshot())
        self.assertEqual(head, self.git("rev-parse", "HEAD"))

    def test_replace_tag_repairs_wrong_version_on_remote(self):
        old_head = self.git("rev-parse", "HEAD")
        self.git("tag", "v0.2.3")
        self.git("push", "origin", "HEAD", "refs/tags/v0.2.3")
        result = self.run_command("run", "release", "0.2.3", "--replace-tag")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        head = self.git("rev-parse", "HEAD")
        self.assertNotEqual(head, old_head)
        result = self.run_command("scripts/release-version.ts", "check", "--tag", "v0.2.3", "--ref", "v0.2.3")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(self.git("ls-remote", "origin", "refs/tags/v0.2.3^{}").startswith(head))
        branch = self.git("branch", "--show-current")
        self.assertTrue(self.git("ls-remote", "origin", f"refs/heads/{branch}").startswith(head))

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
