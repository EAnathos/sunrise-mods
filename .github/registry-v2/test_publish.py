"""Offline fixtures for the signed Pages publish and recovery decisions."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import live

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
PORTS = REPO / "bot/tests/fixtures/two-ports"
FIXTURES = HERE / "fixtures"
KEY = FIXTURES / "index-key.pub"
OLDER = (PORTS / "index.json").read_bytes()
NEWER = OLDER.replace(b'"serial": 41,', b'"serial": 42,', 1)
BODIES = {"older": OLDER, "newer": NEWER, "equal-different": OLDER + b"\n"}


class Publish(unittest.TestCase):
    def setUp(self):
        output = Path(os.environ["CARGO_TARGET_DIR"])
        scratch = tempfile.TemporaryDirectory(dir=output)
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.bot = Path(os.environ["REGISTRY_BOT"])
        self.verifier = Path(os.environ["REGISTRY_VERIFIER"])
        self.site = self.root / "site"
        self.site.mkdir()

    def snapshot(self, root, variant):
        root.mkdir(exist_ok=True)
        (root / "index.json").write_bytes(BODIES[variant])
        shutil.copyfile(FIXTURES / f"{variant}.minisig", root / "index.json.minisig")
        (root / live.BUNDLE).write_bytes(live.bundle(root))
        for project in json.loads(OLDER)["projects"]:
            source = PORTS / "registry/projects" / project["id"] / "page/icon.png"
            # The golden fixture's screenshots are byte-identical to each project's icon.
            file = root / project["icon"]
            file.parent.mkdir(parents=True)
            shutil.copyfile(source, file)
        return root

    def live_from(self, root):
        def download(name, file, _limit):
            source = root / name
            if not source.is_file():
                return False
            file.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, file)
            return True
        return patch.object(live, "download", side_effect=download)

    def assert_site(self, variant=None):
        # The catalogue is the whole site.
        self.assertEqual([path.name for path in self.site.iterdir()], [] if variant is None else ["v2"])
        if variant is not None:
            published = self.site / "v2"
            index = live.verify(published, KEY, self.verifier)
            live.check_images(published, index)
            self.assertEqual(index.body, BODIES[variant])
            signature = (published / "index.json.minisig").read_bytes()
            self.assertEqual((published / live.BUNDLE).read_bytes(), signature + BODIES[variant])

    def test_deploy_is_confirmed_only_once_the_site_serves_its_one_signed_file(self):
        deployed = self.snapshot(self.root / "deployed", "newer")
        stale = self.snapshot(self.root / "stale", "older")
        # The mix a reader of two files could get: the new index beside the old signature.
        mixed = self.root / "mixed"
        mixed.mkdir()
        (mixed / live.BUNDLE).write_bytes((stale / "index.json.minisig").read_bytes() + BODIES["newer"])
        served = iter([stale, mixed, deployed])
        current = [next(served)]
        waits = []

        def pause(seconds):
            waits.append(seconds)
            current[0] = next(served, current[0])

        def download(name, file, _limit):
            shutil.copyfile(current[0] / name, file)
            return True

        with patch.object(live, "download", side_effect=download):
            live.confirm_live(deployed, KEY, self.verifier, pause)
            self.assertEqual(len(waits), 2)
            current[0] = mixed
            with self.assertRaisesRegex(ValueError, "LIVE_V2_NOT_DEPLOYED"):
                live.confirm_live(deployed, KEY, self.verifier, lambda _seconds: None)

    def test_a_commit_publishes_unless_it_left_main_or_is_older_than_the_deployed_one(self):
        workflow = (REPO / ".github/workflows/publish.yml").read_text()
        for job in ("sign", "deploy"):
            self.assertIn('live.py --publishable "$GITHUB_SHA"', workflow.split(f"\n  {job}:\n", 1)[1].split("\n\n", 1)[0])
        env, api = self.deployed_api()
        deployed, merged, tip = "c" * 40, "a" * 40, "b" * 40

        def publishable(sha, to_main, from_deployed):
            compare = {f"compare/{sha}...main": {"status": to_main},
                       f"compare/{deployed}...{sha}": {"status": from_deployed}}
            with patch.dict(os.environ, env), patch.object(live, "gh_api", side_effect={**api, **compare}.__getitem__):
                live.require_publishable(sha)

        # Another pull request merged while this run worked: main moved on, and this commit still publishes.
        publishable(merged, "ahead", "ahead")
        publishable(tip, "identical", "ahead")
        publishable(deployed, "ahead", "identical")
        with self.assertRaisesRegex(ValueError, "older than the deployed one"):
            publishable(merged, "ahead", "behind")
        with self.assertRaisesRegex(ValueError, "not on main"):
            publishable(merged, "diverged", "ahead")

    def test_deploy_retry_refuses_an_older_signed_candidate(self):
        candidate = self.snapshot(self.root / "candidate", "older")
        published = self.snapshot(self.root / "published", "newer")
        with self.live_from(published), patch.object(live, "read_deployed", return_value=None):
            with self.assertRaisesRegex(ValueError, "old or different equal serial"):
                live.guard_deploy(candidate, KEY, self.verifier, carried=False)

    def test_deploy_retry_uses_last_deployment_when_public_pages_lags(self):
        candidate = self.snapshot(self.root / "candidate", "older")
        published = self.snapshot(self.root / "published", "newer")
        verified = live.verify(published, KEY, self.verifier)
        with self.live_from(candidate), patch.object(live, "read_deployed", return_value=verified):
            with self.assertRaisesRegex(ValueError, "old or different equal serial"):
                live.guard_deploy(candidate, KEY, self.verifier, carried=True)

    def test_carry_only_reuses_equal_serial_for_identical_index_bytes(self):
        published = self.snapshot(self.root / "published", "older")
        equal = self.snapshot(self.root / "different", "equal-different")
        with self.live_from(published), patch.object(live, "read_deployed", return_value=None):
            live.guard_deploy(published, KEY, self.verifier, carried=True)
            with self.assertRaisesRegex(ValueError, "old or different equal serial"):
                live.guard_deploy(equal, KEY, self.verifier, carried=True)
            with self.assertRaisesRegex(ValueError, "old or different equal serial"):
                live.guard_deploy(published, KEY, self.verifier, carried=False)

    def test_publish_carries_live_snapshot_after_generation_failure(self):
        self.assert_failed_candidate_carries_live("generation")

    def test_publish_carries_live_snapshot_after_signing_failure(self):
        self.assert_failed_candidate_carries_live("signing")

    def test_publish_carries_live_snapshot_after_verification_failure(self):
        self.assert_failed_candidate_carries_live("verification")

    def assert_failed_candidate_carries_live(self, phase):
        candidate = self.root / "candidate"
        pages = self.root / "v2-pages.tar"
        published = self.snapshot(self.root / "published", "older")
        if phase in ("signing", "verification"):
            candidate.mkdir()
            (candidate / "index.json").write_bytes(NEWER)
            subprocess.run(["tar", "-C", published, "-cf", pages, "pages"], check=True)
        if phase == "verification":
            shutil.copyfile(FIXTURES / "older.minisig", candidate / "index.json.minisig")
        with self.live_from(published), patch.object(live, "read_deployed", return_value=None):
            self.assertTrue(live.assemble(self.site, candidate, pages, KEY, self.verifier))
        self.assert_site("older")

    def test_failed_candidate_carries_the_deployed_snapshot_when_public_pages_lags(self):
        # Pages still serves serial 41 while serial 42 is what the last deployment published.
        stale = self.snapshot(self.root / "stale", "older")
        deployed = self.snapshot(self.root / "deployed", "newer")

        def read_deployed(root, key, bot):
            shutil.copytree(deployed, root)
            index = live.verify(root, key, bot)
            live.check_images(root, index)
            return index

        with self.live_from(stale), patch.object(live, "read_deployed", side_effect=read_deployed):
            self.assertTrue(live.assemble(self.site, self.root / "none", self.root / "none.tar", KEY, self.verifier))
            live.guard_deploy(self.site / "v2", KEY, self.verifier, carried=True)
        self.assert_site("newer")

    def deployed_api(self, *, keyed=True):
        env = {"GITHUB_SERVER_URL": "https://github.com", "GITHUB_REPOSITORY": "example/mods"}
        sha = "c" * 40
        log_url = f"{env['GITHUB_SERVER_URL']}/{env['GITHUB_REPOSITORY']}/actions/runs/70/job/80"
        api = {
            "deployments?environment=github-pages&per_page=100&page=1": [{"id": 10}, {"id": 9, "ref": "main", "sha": sha}],
            "deployments/10/statuses?per_page=100": [{"state": "in_progress"}],
            "deployments/9/statuses?per_page=100": [{"state": "success", "log_url": log_url}],
            "actions/runs/70": {"head_branch": "main", "path": ".github/workflows/publish.yml", "head_sha": sha,
                                "conclusion": "failure"},  # Pages succeeded before v2 was reported red.
            "actions/jobs/80": {"run_id": 70, "name": "deploy", "run_attempt": 2, "head_sha": sha},
            f"git/commits/{sha}": {"tree": {"sha": "d" * 40}},
            f"git/trees/{'d' * 40}": {"truncated": False, "tree": [{"path": "registry", "sha": "e" * 40}]},
            f"git/trees/{'e' * 40}": {"truncated": False, "tree": (
                [{"path": "index-key.pub", "sha": "f" * 40}] if keyed else []
            )},
        }
        return env, api

    def test_live_read_failure_uses_last_successful_deploy_run_artifact(self):
        published = self.snapshot(self.root / "previous", "older")
        # Attempt 2 deployed, so its snapshot is the deployed one.
        env, api = self.deployed_api()
        run = subprocess.run
        downloads = []

        def run_with_artifact(command, **kwargs):
            if command[:3] == ["gh", "run", "download"]:
                self.assertEqual(command[3:-1], ["70", "--repo", "example/mods", "--name", "v2-snapshot-2", "--dir"])
                shutil.copytree(published, command[-1])
                downloads.append(command[3])
                return subprocess.CompletedProcess(command, 0)
            return run(command, **kwargs)

        with (patch.dict(os.environ, env), patch.object(live, "gh_api", side_effect=api.__getitem__),
              patch.object(live, "download", side_effect=ValueError("live Pages returned HTTP 503")),
              patch.object(subprocess, "run", side_effect=run_with_artifact)):
            self.assertTrue(live.assemble(self.site, self.root / "none", self.root / "none.tar", KEY, self.verifier))
            live.guard_deploy(self.site / "v2", KEY, self.verifier, carried=True)
        self.assertEqual(downloads, ["70", "70"])
        self.assert_site("older")

    def test_deploy_retry_refuses_stale_pages_when_the_deployed_snapshot_is_unavailable(self):
        stale = self.snapshot(self.root / "stale", "older")
        env, api = self.deployed_api()
        run = subprocess.run
        downloads = []

        def run_with_missing_snapshot(command, **kwargs):
            if command[:3] == ["gh", "run", "download"]:
                name = command[command.index("--name") + 1]
                downloads.append(name)
                if name == "v2-snapshot-1":
                    shutil.copytree(stale, command[-1])  # A stale artifact is still retained.
                    return subprocess.CompletedProcess(command, 0)
                # The successful deployment used attempt 2's newer signed catalogue.
                raise subprocess.CalledProcessError(1, command)
            return run(command, **kwargs)

        with (patch.dict(os.environ, env), patch.object(live, "gh_api", side_effect=api.__getitem__),
              self.live_from(stale), patch.object(subprocess, "run", side_effect=run_with_missing_snapshot)):
            with self.assertRaisesRegex(ValueError, "V2_DEPLOYED_SERIAL_UNKNOWN.*both unavailable"):
                live.guard_deploy(stale, KEY, self.verifier, carried=True)
            # The deployment recorded the newer catalogue, so stale Pages still cannot stand in.
            api.update(self.record("newer"))
            with self.assertRaisesRegex(ValueError, "V2_DEPLOYED_SERIAL_UNKNOWN.*does not serve"):
                live.guard_deploy(stale, KEY, self.verifier, carried=True)
        self.assertEqual(downloads, ["v2-snapshot-2", "v2-snapshot-2"])

    def record(self, variant, **changes):
        body = BODIES[variant]
        row = {"context": "registry-v2/deployed", "state": "success", "creator": {"login": "github-actions[bot]"},
               "target_url": "https://github.com/example/mods/actions/runs/70/attempts/2",
               "description": f"serial={json.loads(body)['serial']} sha256={hashlib.sha256(body).hexdigest()}"}
        unrelated = {**row, "context": "registry-v2/release-check"}
        return {f"commits/{'c' * 40}/statuses?per_page=100&page=1": [unrelated, {**row, **changes}],
                f"commits/{'c' * 40}/statuses?per_page=100&page=2": []}

    def test_matching_live_site_stands_in_for_an_unavailable_deployment_snapshot(self):
        published = self.snapshot(self.root / "published", "older")
        newer = self.snapshot(self.root / "newer", "newer")
        env, api = self.deployed_api()
        run = subprocess.run

        def run_without_artifacts(command, **kwargs):
            if command[:3] == ["gh", "run", "download"]:
                raise subprocess.CalledProcessError(1, command)  # Expired, or the Actions API failed.
            return run(command, **kwargs)

        def guard(candidate, carried, **changes):
            api.update(self.record("older", **changes))
            with (patch.dict(os.environ, env), patch.object(live, "gh_api", side_effect=api.__getitem__),
                  self.live_from(published), patch.object(subprocess, "run", side_effect=run_without_artifacts)):
                return live.guard_deploy(candidate, KEY, self.verifier, carried=carried)

        self.assertEqual(guard(newer, False).serial, 42)
        self.assertEqual(guard(published, True).serial, 41)  # A carried snapshot is the same index.
        with self.assertRaisesRegex(ValueError, "old or different equal serial"):
            guard(published, False)
        for changes in ({"target_url": "https://github.com/example/mods/actions/runs/70/attempts/1"},
                        {"state": "pending"}, {"creator": {"login": "someone"}},
                        {"description": "serial=041 sha256=" + "0" * 64}):
            with self.subTest(changes=changes), self.assertRaisesRegex(ValueError, "V2_DEPLOYED_SERIAL_UNKNOWN"):
                guard(newer, False, **changes)

    def test_first_keyed_deploy_can_follow_a_deploy_without_a_key(self):
        candidate = self.snapshot(self.root / "candidate", "older")
        env, api = self.deployed_api(keyed=False)
        with (patch.dict(os.environ, env), patch.object(live, "gh_api", side_effect=api.__getitem__) as request,
              self.live_from(self.root / "not-published")):
            live.guard_deploy(candidate, KEY, self.verifier, carried=False)
        self.assertNotIn("actions/runs/70/jobs?filter=all&per_page=100&page=1",
                         [call.args[0] for call in request.call_args_list])

    def test_no_key_never_reads_v2_and_stages_nothing(self):
        with patch.object(live, "download") as download, patch.object(live, "gh_api") as api:
            no_key = self.root / "no-key"
            self.assertFalse(live.assemble(self.site, self.root / "none", self.root / "no.tar", no_key, self.verifier))
            live.guard_deploy(self.root / "none", no_key, self.verifier, carried=False)
            download.assert_not_called()
            api.assert_not_called()
        self.assert_site()

    def test_missing_authenticated_snapshot_has_a_named_failure(self):
        with (patch.object(live, "read_live", side_effect=ValueError("live read failed")),
              patch.object(live, "read_deployed", return_value=None)):
            with self.assertRaisesRegex(ValueError, "NO_AUTHENTICATED_V2_SNAPSHOT"):
                live.assemble(self.site, self.root / "none", self.root / "no.tar", KEY, self.verifier)
        self.assert_site()

    def test_author_dot_github_screenshot_survives_the_pages_archive_rule(self):
        source = PORTS / "registry/projects/Zz91kLmN/page/.github/map #1 %.png"
        digest = hashlib.sha256(source.read_bytes()).hexdigest()
        relative = f"pages/Zz91kLmN/{digest}.png"
        candidate = self.root / "candidate"
        subprocess.run([self.bot, "index", PORTS, "--serial", "41", "--out", candidate], check=True)
        generated = json.loads((candidate / "index.json").read_bytes())
        self.assertEqual(generated["projects"][1]["screenshots"][0]["path"], relative)
        pages = self.root / "v2-pages.tar"
        subprocess.run(["tar", "-C", candidate, "-cf", pages, "pages"], check=True)
        shutil.rmtree(candidate / "pages")
        (candidate / "index.json").write_bytes(OLDER)
        shutil.copyfile(FIXTURES / "older.minisig", candidate / "index.json.minisig")
        self.assertFalse(live.assemble(self.site, candidate, pages, KEY, self.verifier))
        self.assert_site("older")
        # Also prove the guard would reject an otherwise staged image that Pages strips.
        stripped = "pages/Zz91kLmN/.github/shot.png"
        file = self.site / "v2" / stripped
        file.parent.mkdir()
        file.write_bytes(source.read_bytes())
        with self.assertRaisesRegex(ValueError, "Pages archive would omit signed v2 files"):
            live.check_archive(self.site, live.Index(41, OLDER, {stripped: (live.SCREENSHOT_LIMIT, digest)}))


if __name__ == "__main__":
    unittest.main()
