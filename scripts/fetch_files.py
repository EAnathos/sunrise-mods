"""Downloads the files `cargo sunrise-mod registry list` printed into <dir>/<sha256> (README.md, "How it works")."""
import hashlib
import json
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path
from urllib.parse import quote

HEX = re.compile(r"[0-9a-f]{64}")
# One plain name, as the registry tool's `plain_name` reads it: no folder, no hidden file, at most 128 bytes. A stable
# build's asset carries `+<release>`.
PLAIN = re.compile(r"(?!\.)[A-Za-z0-9._+-]{1,128}")
# An author's release asset; `parse` holds the tag to the tool's rule.
ASSET = re.compile(r"https://github\.com/(?P<repo>[A-Za-z0-9-]+/[A-Za-z0-9._-]+)/releases/download/"
                   r"(?P<tag>[^?#\s]+)/(?P<name>[^/?#\s]+)")
# The registry tool's own limits (sunrise-mod-package: index::MAX_FILE, registry::builds::MAX_DESCRIPTOR): nothing
# larger is read, so nothing larger is downloaded. An archive is cut off at the size its verified descriptor gives.
MAX_PACKAGE = 64 << 20
MAX_DESCRIPTOR = 256 << 10


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def asset_url(repo: str, tag: str, name: str) -> str:
    return f"https://github.com/{repo}/releases/download/{quote(tag, safe='/+')}/{quote(name, safe='+')}"


def download(url: str, into: Path, limit: int) -> str | None:
    """Streams `url` into `into`, stopping at `limit` bytes. Why it failed, or None; "" when the asset isn't there."""
    request = urllib.request.Request(url, headers={"User-Agent": "sunrise-mods-registry"})
    try:
        with urllib.request.urlopen(request, timeout=60) as response, into.open("wb") as out:
            size = 0
            while chunk := response.read(1 << 20):
                size += len(chunk)
                if size > limit:
                    out.close()
                    into.unlink()
                    return f"is larger than the {limit} bytes allowed"
                out.write(chunk)
    except urllib.error.HTTPError as error:
        into.unlink(missing_ok=True)
        return "" if error.code == 404 else f"could not be downloaded (HTTP {error.code})"
    except OSError as error:
        into.unlink(missing_ok=True)
        return f"could not be downloaded ({error})"
    return None


def rows(listing: str):
    """Each listed file as (sha256, id, version, file, size): the lines of `registry list`, where `file` is a plain
    name or an author's release asset, or the JSON of `registry list --build-assets`, whose descriptors have no size."""
    text = Path(listing).read_text()
    if text.lstrip().startswith("["):
        for row in json.loads(text):
            yield row["sha256"], row["id"], row["version"], row["url"], row["size"], True
        return
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 4:
            yield *parts, None, False


def parse(digest: str, id_: str, version: str, file: str, build: bool):
    """The registry's release tag, the asset name and the author's (repo, tag) for a listed file, or None."""
    tag = f"{id_}-{version}"
    theirs = ASSET.fullmatch(file)
    name = theirs["name"] if theirs else file
    if not (HEX.fullmatch(digest) and PLAIN.fullmatch(tag) and PLAIN.fullmatch(name) and "." in name):
        return None
    # As the tool reads them: a package's tag is one plain name, a build asset's may have `/`-separated parts.
    if theirs and not (all(PLAIN.fullmatch(part) for part in theirs["tag"].split("/")) if build
                       else PLAIN.fullmatch(theirs["tag"])):
        return None
    return tag, name, (theirs["repo"], theirs["tag"]) if theirs else None


def fetch(repo: str, listing: str, files: Path) -> list[str]:
    errors = []
    for digest, id_, version, file, size, build in rows(listing):
        if not (parsed := parse(digest, id_, version, file, build)):
            errors.append(f"{digest} {id_}-{version} {file}: not a plain sha256, id, version and a file name or "
                          "GitHub release asset")
            continue
        tag, name, theirs = parsed
        target = files / digest
        if target.is_file() and sha256(target) == digest:
            continue
        if build and size is not None and not (isinstance(size, int) and 0 < size <= MAX_PACKAGE):
            errors.append(f"{tag} {name}: its descriptor gives no usable size")
            continue
        limit = size if size is not None else MAX_DESCRIPTOR if build else MAX_PACKAGE
        # The registry's own copy first; then the author's release for a stable row, `staging` for an older one.
        sources = [(asset_url(repo, tag, name), "")]
        if theirs:
            sources.append((asset_url(*theirs, name), f" ({theirs[0]})"))
        else:
            sources.append((asset_url(repo, "staging", f"{digest}.{name.rsplit('.', 1)[1]}"), " (staging)"))
        part = files / f"{digest}.part"
        problem = ""
        for url, source in sources:
            problem = download(url, part, limit)
            if problem != "":
                break
        if problem == "":
            where = f"{theirs[0]}'s release {theirs[1]}" if theirs else f"staging as {digest}.<extension>"
            errors.append(f"{tag} {name}: not in its release, nor in {where}")
        elif problem:
            errors.append(f"{tag} {name}: {problem}")
        elif (size is not None and part.stat().st_size != size) or sha256(part) != digest:
            part.unlink()
            errors.append(f"{tag} {name}: the downloaded file isn't the one its sha256 names")
        else:
            part.replace(target)
            print(f"{digest} {tag}/{name}{source}")
    return errors


def main(repo: str, listing: str, out: str) -> int:
    files = Path(out)
    files.mkdir(parents=True, exist_ok=True)
    errors = fetch(repo, listing, files)
    for e in errors:
        print(f"error: {e}", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    if len(sys.argv) != 4:
        sys.exit("usage: fetch_files.py <owner/repo> <list> <dir>")
    sys.exit(main(*sys.argv[1:]))
