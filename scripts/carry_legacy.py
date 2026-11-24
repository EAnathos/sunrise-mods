"""Copies the old-format registry site from the live Pages site, byte for byte, so the next deploy carries it.

The old format is no longer built. Launchers up to 1.2.0 still read it: `index.json` and `sources.json`
(sunrise-launcher-addresses `MOD_INDEX` and `MOD_SOURCES`) and the images those two files link under the site
(`MOD_IMAGES`). Those are the whole old-format site, so they are what this copies.

    python3 -I scripts/carry_legacy.py <dir>     # the files, for the Pages deploy
    python3 -I scripts/carry_legacy.py --hashes  # `sha256  URL` per file, to show the site did not change

Any failed download fails the script, so the workflow stops before it deploys and the live site stays as it is.
"""
import hashlib
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SITE = "https://sunrise-org.github.io/sunrise-mods/"
ROOTS = ("index.json", "sources.json")
LINK = re.compile(re.escape(SITE) + r'([^"?#\s\\]*)')
# The site's own image folders: `cargo sunrise-mod registry build` linked `icons/` and `screenshots/`.
IMAGE = re.compile(r"(?:icons|screenshots)(?:/[A-Za-z0-9_-][A-Za-z0-9._-]*)+")


def fetch(name: str, out: Path) -> bytes:
    out.parent.mkdir(parents=True, exist_ok=True)
    # Past Pages' edge cache, as .github/registry-v2/live.py reads the v2 files.
    subprocess.run(
        ["curl", "--disable", "--silent", "--show-error", "--fail", "--globoff",
         "--proto", "=https", "--header", "Cache-Control: no-cache",
         "--output", str(out), "--", f"{SITE}{name}?carry={time.time_ns()}"],
        check=True,
    )
    return out.read_bytes()


def links(body: bytes) -> set[str]:
    found = set()
    for name in LINK.findall(body.decode("utf-8")):
        if name == "":
            continue  # The site's own address, not a file.
        if not IMAGE.fullmatch(name):
            raise ValueError(f"the old-format site links {name!r}, which is not one of its images")
        found.add(name)
    return found


def carry(site: Path) -> list[str]:
    names = list(ROOTS)
    linked = set()
    for name in ROOTS:
        linked |= links(fetch(name, site / name))
    for name in sorted(linked):
        fetch(name, site / name)
    return names + sorted(linked)


def main(args: list[str]) -> int:
    if args == ["--hashes"]:
        with tempfile.TemporaryDirectory() as directory:
            for name in carry(Path(directory)):
                print(f"{hashlib.sha256((Path(directory) / name).read_bytes()).hexdigest()}  {SITE}{name}")
        return 0
    if len(args) != 1 or args[0].startswith("-"):
        sys.exit("usage: carry_legacy.py <dir> | --hashes")
    site = Path(args[0])
    if site.exists() and any(site.iterdir()):
        sys.exit(f"{site} is not empty")
    carry(site)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
