import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import carry_legacy

SITE = carry_legacy.SITE


class CarryLegacyTest(unittest.TestCase):
    def live(self, files):
        def fetch(name, out):
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_bytes(files[name])
            return files[name]
        return patch.object(carry_legacy, "fetch", side_effect=fetch)

    def test_carries_both_indexes_and_every_image_they_link(self):
        files = {
            "index.json": f'{{"site":"{SITE}","icon":"{SITE}icons/a.png?v=1"}}'.encode(),
            "sources.json": f'["{SITE}screenshots/a/1-a.jpg","{SITE}icons/a.png"]'.encode(),
            "icons/a.png": b"\x89PNG icon",
            "screenshots/a/1-a.jpg": b"jpeg",
        }
        with tempfile.TemporaryDirectory() as directory, self.live(files):
            site = Path(directory)
            carry_legacy.carry(site)
            carried = {str(path.relative_to(site)): path.read_bytes() for path in site.rglob("*") if path.is_file()}
        self.assertEqual(carried, files)

    def test_refuses_a_link_that_is_not_an_image_of_the_site(self):
        for name in ("v2/index.json", "icons/../index.json", "icons/.hidden"):
            files = {"index.json": f'"{SITE}{name}"'.encode(), "sources.json": b"[]"}
            with tempfile.TemporaryDirectory() as directory, self.live(files):
                with self.assertRaisesRegex(ValueError, "not one of its images"):
                    carry_legacy.carry(Path(directory))


if __name__ == "__main__":
    unittest.main()
