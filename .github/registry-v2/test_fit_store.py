#!/usr/bin/env python3
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

STORE = Path(__file__).with_name("fit_store.py")
BOT = "41898282+github-actions[bot]@users.noreply.github.com"


class FitStore(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.root = Path(folder.name)
        self.git("init", "-q", "--bare", "remote.git", cwd=self.root)
        self.git("init", "-q", "checkout", cwd=self.root)
        self.checkout = self.root / "checkout"
        self.git("remote", "add", "origin", self.root / "remote.git")

    def git(self, *args, cwd=None, email=BOT):
        env = os.environ | {f"GIT_{role}_{field}": value for role in ("AUTHOR", "COMMITTER")
                            for field, value in (("NAME", "someone"), ("EMAIL", email))}
        subprocess.run(["git", "-c", "commit.gpgsign=false", *args], cwd=cwd or self.checkout, check=True, env=env,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def store(self, *args):
        return subprocess.run([sys.executable, "-I", STORE, *args], cwd=self.checkout, text=True,
                              env=os.environ | {"GITHUB_OUTPUT": str(self.root / "output")},
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def files(self, **files):
        for name, text in files.items():
            path = self.checkout / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)

    def push(self, email=BOT, **files):
        """A commit made by hand on the stored branch, holding `files`."""
        self.files(**files)
        self.git("add", "--force", "--", *files)
        self.git("commit", "-q", "-m", "by hand", email=email)
        self.git("push", "-q", "origin", "HEAD:refs/heads/registry-fit")

    def test_verdicts_written_by_one_run_are_read_by_the_next(self):
        self.assertNotEqual(self.store("read").returncode, 0)
        self.assertEqual(self.store("read", "--optional").returncode, 0)
        self.files(**{"registry/fit/builds.json": "[]", "registry/fit/considered.json": "[]",
                      "registry/fit/Ab3dE9xQ.toml": "", "registry/other.toml": ""})
        self.assertIn("Stored the verdicts", self.store("write", "first").stdout)
        self.assertIn("are current", self.store("write", "second").stdout)
        # The judging workflow publishes a dispatched run only when its write changed the store.
        self.assertEqual((self.root / "output").read_text(), "changed=true\nchanged=false\n")
        (self.checkout / "registry/fit/Ab3dE9xQ.toml").unlink()
        self.assertEqual(self.store("read").returncode, 0)
        self.assertTrue((self.checkout / "registry/fit/Ab3dE9xQ.toml").exists())
        log = subprocess.run(["git", "log", "--format=%ae %s", "--name-only", "refs/remotes/origin/registry-fit"],
                             cwd=self.checkout, text=True, stdout=subprocess.PIPE).stdout.split()
        self.assertEqual(log, [BOT, "first", "registry/fit/Ab3dE9xQ.toml", "registry/fit/builds.json",
                               "registry/fit/considered.json"])

    def test_a_branch_holding_anything_but_fit_files_is_refused(self):
        self.push(**{"registry/fit/builds.json": "[]", "registry/projects/Ab3dE9xQ/project.toml": ""})
        result = self.store("read")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is not a fit file", result.stderr)
        self.assertFalse((self.checkout / "registry/fit/builds.json").exists() and result.returncode == 0)

    def test_a_commit_by_anyone_but_the_judging_job_is_refused(self):
        self.push(email="someone@example.com", **{"registry/fit/builds.json": "[]"})
        for command in (["read"], ["write", "next"]):
            result = self.store(*command)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not the judging job", result.stderr)


if __name__ == "__main__":
    unittest.main()
