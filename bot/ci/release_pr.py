"""Privileged release PR publication/reporting and read-only PR shape checks.

Never checks out a PR or runs code from one. PR inputs go to Git/GitHub as data.
"""
import collections
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
import zipfile
from pathlib import Path

PROJECT = r"[A-Za-z0-9]{8}"
# Rust's semver parser owns the version; here constrain independent Git/path inputs.
VERSION = r"[0-9][0-9A-Za-z.+-]*"
RELEASE = re.compile(rf"release/(?P<project>{PROJECT})/(?P<version>{VERSION})", re.ASCII)
ROW = re.compile(rf"registry/projects/(?P<project>{PROJECT})/releases/(?P<version>{VERSION})\.toml", re.ASCII)
REPOSITORY = re.compile(r"registry/repositories/[1-9][0-9]*\.toml", re.ASCII)
PROJECT_FILE = re.compile(rf"registry/projects/{PROJECT}/project\.toml", re.ASCII)
SHA = re.compile(r"[0-9a-f]{40}", re.ASCII)
PR = re.compile(r"[1-9][0-9]*", re.ASCII)
DIGEST = re.compile(r"(?:sha256:)?([0-9a-f]{64})", re.ASCII)
REQUIRED_JOBS = {"pending", "verify", "build", "scanner"}
BOT = "github-actions[bot]"
REGISTRATION_APP = "sunrise-sunny[bot]"
STATUS = "registry-v2/release-check"
WORKFLOW = "release-check.yml"
NOT_A_RELEASE = "release-not-applicable.yml"
# The status description of a check that could not finish because GitHub, Modrinth or the connection failed. It says
# nothing about the release: the watcher runs such a check once more, and a reader of the status (a reviewer,
# the merger) tells it from a refusal by this text.
UNAVAILABLE = "Not checked: GitHub or Modrinth failed. Not a verdict on the release"
# registry-bot's exit for it (`UNAVAILABLE_EXIT` in bot/src/github.rs).
UNAVAILABLE_EXIT = 75
MAIN = "refs/heads/main"
ROOT = Path(__file__).resolve().parents[2]


class CommandError(ValueError):
    pass


def command(*args, cwd=ROOT, data=None):
    result = subprocess.run(args, cwd=cwd, input=data, capture_output=True)
    if result.returncode:
        name = " ".join(map(str, args[:2]))
        raise CommandError(f"{name}: {result.stderr.decode('utf-8', errors='replace').strip()}")
    return result.stdout


def gh(*args, data=None):
    return command("gh", *args, data=None if data is None else json.dumps(data).encode())


def api(endpoint, *args, data=None):
    return json.loads(gh("api", "--hostname", "github.com", *args, "repos/" + os.environ["GITHUB_REPOSITORY"] + "/" + endpoint,
                         *(('--input', '-') if data is not None else ()), data=data))


def rows(endpoint):
    return [item for page in api(endpoint, "--paginate", "--slurp") for item in page]


def git(*args, cwd=ROOT):
    return command("git", *args, cwd=cwd)


def require(condition, detail):
    if not condition:
        raise ValueError(detail)


def safe(text):
    return str(text).encode("unicode_escape").decode("ascii").replace("::", "%3A%3A").replace("##[", "%23%23%5B")


def own_bot_pr(pr):
    repo = os.environ["GITHUB_REPOSITORY"].lower()
    head_repo = pr["head"]["repo"]
    return head_repo and head_repo["full_name"].lower() == repo and pr["user"]["login"] == BOT


def own_release(pr):
    return own_bot_pr(pr) and RELEASE.fullmatch(pr["head"]["ref"])


def dispatch():
    number, sha = os.environ["PR_NUMBER"], os.environ["PR_HEAD"]
    require(PR.fullmatch(number) and SHA.fullmatch(sha), "dispatch requires a positive PR number and full lowercase head SHA")
    require(os.environ["GITHUB_REF"] == MAIN, "release checks run from main")
    pr = api(f"pulls/{number}")
    branch = own_release(pr)
    require(branch and pr["base"]["ref"] == "main" and pr["state"] == "open" and pr["head"]["sha"] == sha,
            "dispatch does not name the current head of an open registry release bot PR")
    return number, sha, branch


def set_status(sha, state, description):
    target = (f"https://github.com/{os.environ['GITHUB_REPOSITORY']}/actions/runs/"
              f"{os.environ['GITHUB_RUN_ID']}/attempts/{os.environ['GITHUB_RUN_ATTEMPT']}")
    api(f"statuses/{sha}", "-X", "POST", data={"context": STATUS, "state": state,
                                               "description": description, "target_url": target})
    return target


def dispatch_runs():
    # This workflow is public; listing its runs does not require actions: read.
    pages = api(f"actions/workflows/{WORKFLOW}/runs?event=workflow_dispatch&branch=main&per_page=100",
                "--paginate", "--slurp")
    return (run for page in pages for run in page["workflow_runs"])


def current_run(number, sha):
    title = f"Registry release PR {number} at {sha}"
    latest = max(((run["id"], run["run_attempt"]) for run in dispatch_runs()
                  if run["display_title"] == title), default=None)
    return latest == (int(os.environ["GITHUB_RUN_ID"]), int(os.environ["GITHUB_RUN_ATTEMPT"]))


def pending():
    number, sha, _ = dispatch()
    require(current_run(number, sha), "this dispatch is not the latest run and attempt for the PR head")
    set_status(sha, "pending", "Checking release")


def bot(*args):
    """Runs registry-bot, and tells the report job when it stopped because GitHub was unavailable."""
    code = subprocess.run([os.environ["REGISTRY_BOT"], *args], cwd=ROOT).returncode
    if code == UNAVAILABLE_EXIT:
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
            print("unavailable=true", file=stream)
    require(code == 0, f"registry-bot {args[0]} failed (exit {code})")


def repository_blob(commit, path):
    entry = git("ls-tree", "-z", commit, "--", path)
    require(entry, "repository row is missing")
    header, name = entry.rstrip(b"\0").split(b"\t", 1)
    mode, kind, blob = header.decode().split()
    require((mode, kind) == ("100644", "blob") and name.decode() == path,
            "repository row must be a regular non-executable file")
    return blob


def check_registration():
    pr = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_bytes())["pull_request"]
    base, head = pr["base"], pr["head"]
    require(pr["user"]["login"] == REGISTRATION_APP and head["ref"].startswith("register/")
            and base["ref"] == "main" and head["repo"]["id"] == base["repo"]["id"],
            "registration must be opened to main by the registry app from its own register/ branch")
    merge = os.environ["GITHUB_SHA"]
    require(all(SHA.fullmatch(sha) for sha in (head["sha"], merge)), "invalid registration head or test merge SHA")
    tested_base, tested_head = git("rev-parse", f"{merge}^1", f"{merge}^2").decode().splitlines()
    require(tested_head == head["sha"], "registration check did not get the exact PR merge commit")
    diff = ("diff", "--name-status", "-z", "--no-renames", "--no-ext-diff", "--no-textconv")
    changes = git(*diff, tested_base, merge, "--").decode().split("\0")[:-1]
    changed = list(zip(changes[::2], changes[1::2], strict=True))
    repositories = [(status, path) for status, path in changed if REPOSITORY.fullmatch(path)]
    if (len(repositories) == 1 and len(changed) == 2 and repositories[0][0] in ("A", "M")
            and all((status, path) == repositories[0] or status == "A" and PROJECT_FILE.fullmatch(path)
                    for status, path in changed)):
        # CUTOVER: delete. The old writer adds one project row and adds or appends to its repository row
        # (#501, #619). This does not exempt it from the ordinary layout, DCO or non-release checks.
        return
    require(len(changed) == 1 and repositories == changed and changed[0][0] == "A",
            "registration must add exactly one registry/repositories/<numeric id>.toml and nothing else")
    _, path = changed[0]
    with tempfile.TemporaryDirectory() as folder:
        row = Path(folder) / Path(path).name
        row.write_bytes(git("cat-file", "blob", repository_blob(merge, path)))
        bot("check-registration", row)


def source_repository(row):
    repository = tomllib.loads(row.read_text(encoding="utf-8"))["repository"]
    require(type(repository) is int and repository > 0, "release row must name a numeric repository ID")
    return f"registry/repositories/{repository}.toml"


def verify(out):
    number, sha, branch = dispatch()
    # By commit, not refs/pull/<n>/head: GitHub sometimes never creates that ref for a PR opened in a burst.
    # dispatch() has shown this commit is the open PR's head on a release branch of this repository.
    git("fetch", "--no-tags", "origin", sha)
    base = git("merge-base", "HEAD", sha).decode().strip()
    project = branch["project"]
    root = f"registry/projects/{project}"
    row = f"{root}/releases/{branch['version']}.toml"
    page = f"{root}/page"
    changed = git("diff", "--name-only", "-z", "--no-renames", "--no-ext-diff", "--no-textconv", base, sha, "--").decode().split("\0")[:-1]
    # A release of another kind than the project's changes the project's kind and takes the place of every earlier
    # release: project.toml is then compared with the regenerated one, and the head holds no other release row.
    project_file = f"{root}/project.toml"
    project_changed = project_file in changed
    earlier = {name for name in changed if name.startswith(f"{root}/releases/") and name != row} if project_changed else set()
    require(row in changed and all(name == row or name.startswith(page + "/") or name == project_file or name in earlier
                                   for name in changed),
            "PR may only add its own release row and replace its own page")
    require(not git("ls-tree", "-z", base, "--", row), "PR may not edit an existing release row")
    first = project_changed and not git("ls-tree", "-z", base, "--", root)
    if project_changed:
        rows = [entry.split(b"\t", 1)[1].decode()
                for entry in git("ls-tree", "-rz", sha, "--", f"{root}/releases").split(b"\0")[:-1]]
        require(rows == [row], "a release that changes the project's kind must remove every earlier release" if not first
                else "a first release must add exactly one release row")
    actual = {}
    for entry in git("ls-tree", "-rz", sha, "--", row, page, *([project_file] if project_changed else [])).split(b"\0")[:-1]:
        header, name = entry.split(b"\t", 1)
        mode, kind, blob = header.decode().split()
        require((mode, kind) == ("100644", "blob"), "release and page may only contain regular non-executable files")
        actual[name.decode()] = blob
    require(row in actual, "release row is missing from the PR head")
    if first:
        require(set(changed) == actual.keys() and f"{page}/page.toml" in actual and project_file in actual,
                "first release must add only the project row, release row and resolved page")

    out.mkdir()
    claim = out / "claim.toml"
    claim.write_bytes(git("cat-file", "blob", actual[row]))
    if first:
        bot("verify-first-release", ROOT, os.environ["GITHUB_REPOSITORY"], project, claim, out)
    else:
        bot("verify-release", ROOT, os.environ["GITHUB_REPOSITORY"], project, claim, out)
    regenerated = out / "expected"
    expected = {p.relative_to(regenerated).as_posix(): git("hash-object", "--no-filters", p).decode().strip()
                for p in regenerated.rglob("*") if p.is_file()}
    require(actual == expected, "PR files are not exactly the release tree regenerated from the attested assets")
    # What the judging job will store for this release once it is merged, for the reviewer to see now. Nothing is
    # kept. A build the release cannot be judged on fails this check instead of every publish after the merge.
    subprocess.run([sys.executable, "-I", ROOT / ".github/registry-v2/fit_store.py", "read"], cwd=ROOT, check=True)
    bot("release-fit", ROOT / "registry/fit/builds.json", claim, "--out", out / "fit.json")
    cells = json.loads((out / "fit.json").read_text(encoding="utf-8"))
    fit = "; ".join(f"{label} {', '.join(builds)}" for label, builds in
                    (("fits", cells["fits"]), ("does not fit", cells["does_not_fit"])) if builds) or "none is offered"
    # Each release goes to the later jobs as one archive: an artifact of tens of thousands of loose files is slow
    # to upload and download.
    for release in ("source", "previous"):
        if (out / release).is_dir():
            (out / "upload" / release).mkdir(parents=True)
            subprocess.run(["tar", "-cf", out / "upload" / release / "release.tar", "-C", out / release, "."], check=True)
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        print("previous=" + str((out / "previous").is_dir()).lower(), file=stream)
        print("comparison=" + ("missing" if (out / "previous-unavailable").exists() else "available"), file=stream)
        print("fit=" + fit, file=stream)
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as stream:
        print("Verified the PR file scope, both attestations, hashes, package manifest, pinned build and resolved page.", file=stream)
        print(f"\nOn the offered builds: {fit}.", file=stream)


def install_tree(generated, work, branch, *, first=False):
    if match := RELEASE.fullmatch(branch):
        root = Path("registry/projects") / match["project"]
        row, page = root / "releases" / (match["version"] + ".toml"), root / "page"
        # Present only for a first release or a change to an existing project's kind.
        project = root / "project.toml"
        found = {p.relative_to(generated).as_posix() for p in generated.rglob("*") if p.is_file()}
        require(row.as_posix() in found and (page / "page.toml").as_posix() in found
                and all(name in (row.as_posix(), project.as_posix()) or name.startswith(page.as_posix() + "/")
                        for name in found),
                "watch artifact is outside its release row and page")
        require(not (work / row).exists(), "cannot alter a release already on main")
        if first:
            require(project.as_posix() in found and not os.path.lexists(work / root),
                    "first release may only add a new project directory")
            (work / root).mkdir()
            shutil.copyfile(generated / project, work / project)
            git("add", "--", project, cwd=work)
        elif project.as_posix() in found:
            require((work / project).is_file(), "cannot change the kind of a project that is not on main")
            shutil.copyfile(generated / project, work / project)
            # Every release of a project has its kind: the earlier ones go with the change.
            earlier = sorted(p.relative_to(work) for p in (work / root / "releases").glob("*.toml"))
            for old in earlier:
                (work / old).unlink()
            git("add", "--", project, *earlier, cwd=work)
        (work / row).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(generated / row, work / row)
        if (work / page).exists():
            shutil.rmtree(work / page)
        shutil.copytree(generated / page, work / page)
        git("add", "--", row, page, cwd=work)
    else:
        rows = sorted(p.relative_to(generated) for p in generated.rglob("*") if p.is_file())
        require(rows and all(REPOSITORY.fullmatch(row.as_posix()) and (work / row).is_file() for row in rows),
                "watch rename artifact is not a set of registered repository files")
        for row in rows:
            shutil.copyfile(generated / row, work / row)
        git("add", "--", *rows, cwd=work)


def open_pr(generated, branch, base, title, body, *, first=False):
    with tempfile.TemporaryDirectory() as folder:
        work = Path(folder) / "registry"
        git("worktree", "add", "--detach", work, base)
        try:
            install_tree(generated, work, branch, first=first)
            if not git("diff", "--cached", "--name-only", cwd=work):
                return
            git("-c", "core.hooksPath=/dev/null", "-c", "user.name=github-actions[bot]",
                "-c", "user.email=41898282+github-actions[bot]@users.noreply.github.com", "commit", "-s", "-m", title, cwd=work)
            # Never overwrite a branch. A run interrupted after push but before opening the
            # PR can reuse it only if its base and complete tree are exactly the ones just generated.
            cred = ("-c", "credential.helper=", "-c", "credential.helper=!gh auth git-credential")
            try:
                git(*cred, "push", "origin", f"HEAD:refs/heads/{branch}", cwd=work)
            except CommandError:
                git("fetch", "--no-tags", "origin", f"refs/heads/{branch}", cwd=work)
                require(git("rev-parse", "FETCH_HEAD^", "FETCH_HEAD^{tree}", cwd=work)
                        == git("rev-parse", "HEAD^", "HEAD^{tree}", cwd=work),
                        "an existing branch does not match the generated release; not replacing it")
            api("pulls", "-X", "POST", data={"base": "main", "head": branch, "title": title, "body": body})
        finally:
            git("worktree", "remove", "--force", work)


def publish(generated):
    base = git("rev-parse", "HEAD").decode().strip()
    live = api("git/ref/heads/main")["object"]["sha"]
    repo = os.environ["GITHUB_REPOSITORY"]
    pulls = "pulls?per_page=100&state="
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as summary:
        # Main may have moved while the watcher observed. An output is still published when none of
        # the files it was generated from changed; its branch is based on the observed commit.
        moved, tool = [], None
        if live != base:
            require(SHA.fullmatch(live), "GitHub returned an invalid commit for main")
            git("fetch", "--no-tags", "--depth=1", "origin", live)
            moved = git("diff", "--name-only", "-z", "--no-renames", "--no-ext-diff", "--no-textconv",
                        base, live, "--").decode().split("\0")[:-1]
            # The watcher itself, and what it reads for every release.
            tool = next((path for path in moved if path.startswith(("bot/", ".github/"))
                         or path in ("registry/categories.toml", "registry/release-workflow.toml")), None)
        if tool is None:
            seen = [pr for pr in rows(pulls + "all") if own_bot_pr(pr)]
            kept = {ref["ref"].removeprefix("refs/heads/") for ref in rows("git/matching-refs/heads/release/")}
            # A closed, unmerged PR whose branch was deleted was withdrawn, not refused.
            handled = {pr["head"]["ref"] for pr in seen
                       if own_release(pr) and (pr["state"] == "open" or pr["merged_at"] or pr["head"]["ref"] in kept)}
            open_branches = {pr["head"]["ref"] for pr in seen if pr["state"] == "open"}
            # An open release PR holds its project's later releases back until it is decided, unless its
            # release check failed: the author's fix for that is a later release.
            waiting = {pr["head"]["ref"] for pr in seen if pr["state"] == "open" and own_release(pr) and not any(
                status["context"] == STATUS and status["state"] == "failure"
                for status in api(f"commits/{pr['head']['sha']}/status")["statuses"])}
            rename = "registry/refresh-repository-names"
            names = {path.relative_to(generated / "rename").as_posix() for path in (generated / "rename").rglob("*.toml")}
            stale = next((path for path in moved if path in names), None)
            if stale:
                print(f"- Skipped {rename}: main changed {safe(stale)} during observation.", file=summary)
            elif names and rename not in open_branches:
                try:
                    open_pr(generated / "rename", rename, base, "Refresh watched repository names",
                            "Updates the display names returned by GitHub for registered numeric repository IDs. No package or release identity changes.")
                except (OSError, ValueError) as error:
                    print(f"- Could not publish repository names: {safe(error)}", file=summary)
            proposals = [(path, first) for leaf, first in (("releases", False), ("first-releases", True))
                         for path in (generated / leaf).glob("*/*")]
            for path, first in sorted(proposals):
                branch = f"release/{path.parent.name}/{path.name}"
                try:
                    match = RELEASE.fullmatch(branch)
                    require(match, "watch artifact has an invalid release path")
                    prefix = f"release/{match['project']}/"
                    if branch in handled or any(other.startswith(prefix) for other in waiting):
                        continue
                    # The generated release always carries the stable ID of its signing repository.
                    # New claims additionally rely on the global ID/slug/path/namespace inventory.
                    # Comparing paths just withholds stale observations; Rust owns admission/collisions.
                    if moved:
                        project_root = f"registry/projects/{match['project']}/"
                        release = path / project_root / "releases" / (match["version"] + ".toml")
                        repository = source_repository(release)
                        stale = next((p for p in moved if p == repository or p.startswith(project_root)
                                      or first and (p.startswith("registry/collections/")
                                                    or p.startswith("registry/projects/") and p.endswith("/project.toml"))), None)
                    else:
                        stale = None
                    if stale:
                        print(f"- Skipped {safe(branch)}: main changed {safe(stale)} during observation.", file=summary)
                        continue
                    title = f"Publish {match['project']} {match['version']}"
                    open_pr(path, branch, base, title,
                            "## Verified release\n\nBoth GitHub attestations match the registered repository ID, tag and commit, and the shared signer workflow. "
                            "The asset sizes and SHA-256 hashes, package manifest, exact Sunrise build and resolved page were checked. "
                            "The release check will reverify those inputs, build the package and run the scanner before review.", first=first)
                    waiting.add(branch)
                except (OSError, ValueError) as error:
                    print(f"- Could not publish {safe(branch)}: {safe(error)}", file=summary)
        else:
            print(f"Main changed {safe(tool)} during observation; no generated files were published.", file=summary)

        # A dispatch retry after a failed request, without redispatching a queued or completed run.
        dispatched = None
        for pr in rows(pulls + "open&base=main"):
            if not own_bot_pr(pr) or pr["base"]["ref"] != "main":
                continue
            number, sha = pr["number"], pr["head"]["sha"]
            if not own_release(pr):
                # GitHub starts no pull_request workflow for a PR this token opened, so the
                # classifier that marks non-release PRs is asked to look at it. It decides by
                # the PR's changed paths, never by its branch name.
                try:
                    require(PR.fullmatch(str(number)) and SHA.fullmatch(sha), "invalid GitHub PR identity")
                    if not any(status["context"] == STATUS for status in api(f"commits/{sha}/statuses?per_page=100")):
                        gh("workflow", "run", NOT_A_RELEASE, "--repo", repo, "--ref", "main", "-f", f"pr={number}")
                except (OSError, ValueError) as error:
                    print(f"- Could not dispatch the non-release check for #{safe(number)}: {safe(error)}", file=summary)
                continue
            try:
                require(PR.fullmatch(str(number)) and SHA.fullmatch(sha), "invalid GitHub PR identity")
                title = f"Registry release PR {number} at {sha}"
                if dispatched is None:
                    # Listed once for every open release PR, not once per PR.
                    dispatched = collections.Counter(run["display_title"] for run in dispatch_runs())
                # A head is checked once, and once more when that check stopped because GitHub was unavailable.
                if dispatched[title] == 0 or dispatched[title] == 1 and any(
                        status["context"] == STATUS and status["description"] == UNAVAILABLE
                        for status in api(f"commits/{sha}/status")["statuses"]):
                    gh("workflow", "run", WORKFLOW, "--repo", repo, "--ref", "main", "-f", f"pr={number}", "-f", f"head={sha}")
            except (OSError, ValueError) as error:
                print(f"- Could not dispatch release check for #{safe(number)}: {safe(error)}", file=summary)


def scan_summary(folder, outputs):
    digest = DIGEST.fullmatch(outputs["summary-digest"])
    require(digest, "invalid digest for the scanner summary")
    artifact, = folder.iterdir()
    with artifact.open("rb") as stream:
        require(hashlib.file_digest(stream, "sha256").hexdigest() == digest[1],
                "scanner report does not match the digest from the trusted scanner")
        stream.seek(0)
        with zipfile.ZipFile(stream) as archive:
            require(archive.namelist() == ["summary.md"], "unexpected scanner report artifact shape")
            with archive.open("summary.md") as summary:
                # GitHub caps the full comment at 65,536 characters. Keep whole lines.
                text = summary.read(60_000)
    truncated = len(text) == 60_000
    if truncated:
        text = text.rsplit(b"\n", 1)[0]
    body = text.decode("utf-8", errors="replace").replace("::", "%3A%3A").replace("##[", "%23%23%5B")
    if truncated:
        body += "\n\nReport excerpt; see the full scanner artifact linked above.\n"
    return body


def report(folder):
    number, sha, _ = dispatch()
    if not current_run(number, sha):
        return
    jobs = json.loads(os.environ["JOB_RESULTS"])
    attempt = os.environ["GITHUB_RUN_ATTEMPT"]
    scan = jobs.get("scanner", {}).get("outputs", {})
    current = (jobs.get("verify", {}).get("outputs", {}).get("attempt") == attempt
               and scan.get("attempt") == attempt and scan.get("summary-id") and scan.get("summary-digest"))
    marker = "<!-- registry-v2-release-check -->"
    target = (f"https://github.com/{os.environ['GITHUB_REPOSITORY']}/actions/runs/"
              f"{os.environ['GITHUB_RUN_ID']}/attempts/{attempt}")
    body = f"{marker}\n## Registry release check\n\n[Check run]({target}).\n"
    body += "\n" + "\n".join(f"- {safe(name)}: {safe(job['result'])}" for name, job in jobs.items()) + "\n"
    success = False
    description = "Release checks failed or did not complete"
    try:
        verified = jobs.get("verify", {}).get("outputs", {})
        if verified.get("attempt") == attempt and verified.get("unavailable") == "true":
            description = UNAVAILABLE
            body += ("\n**Not checked**: GitHub or Modrinth answered with a server error or the connection failed, so this "
                     "says nothing about the release. The check runs once more by itself; after that, use "
                     "**Rerun all jobs**.\n")
        elif not current or os.environ["REPORT_DOWNLOADED"] != "true":
            description = "Rerun all jobs"
            body += "\n**Rerun all jobs**: no scanner report was available from this attempt.\n"
        else:
            if jobs["verify"]["outputs"].get("comparison") == "missing":
                body += ("\n**No comparison with the last approved release**: its tag, package or attestation is no "
                         "longer published in the author's repository, so the scan below treats every file and "
                         "dependency as new. Review this release in full.\n")
            body += f"\n**On the offered builds**: {safe(jobs['verify']['outputs']['fit'])}.\n"
            body += "\n" + scan_summary(folder, scan)
            success = jobs.keys() == REQUIRED_JOBS and all(job["result"] == "success" for job in jobs.values())
            if success:
                description = "All release checks passed"
        comments = rows(f"issues/{number}/comments?per_page=100")
        existing = next((c for c in comments if c["user"]["login"] == BOT and c["body"].startswith(marker)), None)
        if existing:
            api(f"issues/comments/{existing['id']}", "-X", "PATCH", data={"body": body})
        else:
            api(f"issues/{number}/comments", "-X", "POST", data={"body": body})
    finally:
        set_status(sha, "success" if success else "failure", description)


if __name__ == "__main__":
    try:
        match sys.argv[1:]:
            case ["publish", folder]:
                publish(Path(folder))
            case ["verify", folder]:
                verify(Path(folder))
            case ["check-registration"]:
                check_registration()
            case ["pending"]:
                pending()
            case ["report", folder]:
                report(Path(folder))
            case _:
                raise ValueError("usage: release_pr.py pending|check-registration, or publish|verify|report <folder>")
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError, zipfile.BadZipFile) as error:
        sys.exit("registry release: " + safe(error))
