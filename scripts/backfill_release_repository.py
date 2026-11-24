#!/usr/bin/env python3
"""Writes `repository` into every release row that has none, from its project's `project.toml`.

Run once from the repository root, when the field was added: no project had moved then, so a
release was signed from the repository its project names. The line goes after `workflow`, where
the bot writes it.
"""
import re
import tomllib
from pathlib import Path

for manifest in sorted(Path("registry/projects").glob("*/project.toml")):
    project = manifest.parent
    repository = tomllib.loads(manifest.read_text())["repository"]
    for row in sorted((project / "releases").glob("*.toml")):
        text = row.read_text()
        if "repository" in tomllib.loads(text):
            continue
        filled, count = re.subn(r"^(workflow = .*\n)", rf"\g<1>repository = {repository}\n", text, count=1, flags=re.M)
        if count != 1 or tomllib.loads(filled) != tomllib.loads(text) | {"repository": repository}:
            raise SystemExit(f"{row}: could not add repository after workflow")
        row.write_text(filled)
