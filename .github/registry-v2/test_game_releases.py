#!/usr/bin/env python3
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("game_releases.py")


class GameReleases(unittest.TestCase):
    def write(self, pages):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        file = Path(folder.name) / "github/repos/sunrise-org/sunrise/releases"
        run = subprocess.run([sys.executable, "-I", SCRIPT, file], input=json.dumps(pages), text=True,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        return run, file

    def test_every_page_is_one_list_without_the_draft(self):
        stable = {"tag_name": "26.3-0.1.0", "draft": False, "prerelease": False, "assets": [{"name": "sdk.json"}]}
        draft = {"tag_name": "dev-next", "draft": True, "prerelease": True, "assets": []}
        dev = {"tag_name": "dev-7", "draft": False, "prerelease": True, "assets": []}
        run, file = self.write([[stable, draft], [dev]])
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(json.loads(file.read_text()), [stable, dev])

    def test_a_release_that_is_not_plainly_published_stops_the_write(self):
        run, file = self.write([[{"tag_name": "dev-8", "draft": None}]])
        self.assertIn("GAME_RELEASE_LIST_HAS_A_DRAFT", run.stderr)
        self.assertFalse(file.exists())


if __name__ == "__main__":
    unittest.main()
