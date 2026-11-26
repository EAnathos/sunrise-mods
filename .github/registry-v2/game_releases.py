#!/usr/bin/env python3
"""The copy of Sunrise's release list that the registry's site serves to launchers GitHub is
rate-limiting. Reads `gh api --paginate --slurp` of the release list (an array of pages) on
standard input and writes every published release, as GitHub returned it, to FILE as one array.
A launcher never asks this copy for a second page, so it is always the complete list."""
import json
import sys
from pathlib import Path

releases = [release for page in json.load(sys.stdin) for release in page if release["draft"] is not True]
# The site is public: nothing but a release GitHub itself marks as published may be on it, and an
# answer with no release at all is not the list.
if not releases or any(release["draft"] is not False for release in releases):
    sys.exit("GAME_RELEASE_LIST_REFUSED: no release, or one that is not plainly published")
file = Path(sys.argv[1])
file.parent.mkdir(parents=True, exist_ok=True)
file.write_text(json.dumps(releases))
print(f"Sunrise's release list: {len(releases)} releases")
