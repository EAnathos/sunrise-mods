#!/usr/bin/env python3
"""Where the fit verdicts, the offered-build list, the list of considered Sunrise releases and
the list of original jars Modrinth does not list are kept: the `registry-fit` branch.

`read` puts the branch's `registry/fit/` into the checkout; `write` commits the checkout's
`registry/fit/` to the branch and, in a workflow, sets the step output `changed`. Nothing else in the registry knows where they are kept.
"""
import os
import re
import subprocess
import sys
import tempfile

BRANCH = "registry-fit"
REF = f"refs/remotes/origin/{BRANCH}"
FILE = re.compile(r"registry/fit/((builds|considered|jars)\.json|[0-9A-Za-z]{8}\.toml)", re.ASCII)
BOT = "41898282+github-actions[bot]@users.noreply.github.com"


def git(*args, env=None):
    return subprocess.run(["git", *args], check=True, stdout=subprocess.PIPE, env=env).stdout.decode()


def fetch():
    """The branch's head, or None before the first judging run has made the branch."""
    if not git("ls-remote", "origin", f"refs/heads/{BRANCH}"):
        return None
    git("fetch", "--no-tags", "origin", f"+refs/heads/{BRANCH}:{REF}")
    head = git("rev-parse", REF).strip()
    # The branch holds verdicts and the offered builds and nothing else. The index generator
    # checks each verdict against the release rows on main; the author check only notices a
    # commit that did not come from the judging job.
    for entry in git("ls-tree", "-rz", head).split("\0")[:-1]:
        header, name = entry.split("\t", 1)
        if header.split()[:2] != ["100644", "blob"] or not FILE.fullmatch(name):
            sys.exit(f"{BRANCH} holds {name!r}, which is not a fit file")
    if strangers := set(git("log", "--format=%ae%n%ce", head).split()) - {BOT}:
        sys.exit(f"{BRANCH} has commits by {sorted(strangers)}, not the judging job")
    return head


def read(optional):
    head = fetch()
    if head is None:
        if optional:
            return
        sys.exit(f"{BRANCH} does not exist: the judging job has not run")
    archive = subprocess.run(["git", "archive", head, "registry/fit"], check=True, stdout=subprocess.PIPE).stdout
    subprocess.run(["tar", "-x"], input=archive, check=True)


def write(message):
    parent = fetch()
    with tempfile.TemporaryDirectory() as folder:
        env = os.environ | {"GIT_INDEX_FILE": os.path.join(folder, "index")}
        # registry/fit is ignored on main, where nothing commits it.
        git("add", "--force", "--", "registry/fit", env=env)
        tree = git("write-tree", env=env).strip()
    changed = not parent or git("rev-parse", f"{parent}^{{tree}}").strip() != tree
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a") as file:
            file.write(f"changed={str(changed).lower()}\n")
    if not changed:
        print("The stored verdicts are current.")
        return
    identity = {f"GIT_{role}_{field}": value for role in ("AUTHOR", "COMMITTER")
                for field, value in (("NAME", "github-actions[bot]"), ("EMAIL", BOT))}
    commit = git("commit-tree", tree, *(["-p", parent] if parent else []), "-m", message, env=os.environ | identity).strip()
    # Never forced: a branch that moved since it was read fails the run, and the next run judges again.
    git("-c", "credential.helper=", "-c", "credential.helper=!gh auth git-credential",
        "push", "origin", f"{commit}:refs/heads/{BRANCH}")
    print(f"Stored the verdicts as {commit}.")


if __name__ == "__main__":
    match sys.argv[1:]:
        case ["read"]:
            read(optional=False)
        case ["read", "--optional"]:
            read(optional=True)
        case ["write", message]:
            write(message)
        case _:
            sys.exit("usage: fit_store.py read [--optional] | write <commit message>")
