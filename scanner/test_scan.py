from pathlib import Path
import unittest

from scan import module_in_release, other_sources


class ModuleFileTest(unittest.TestCase):
    files = {Path(name) for name in ("src/lib.rs", "src/terrain.rs", "src/pieces/mod.rs", "src/pieces/rooms.rs",
                                     "src/terrain/tiles.rs", "tools/entry.rs", "tools/helper.rs", "src/x/lib.rs", "src/x/y.rs")}

    def present(self, declaring, text, roots=("src/lib.rs",)):
        return module_in_release(self.files, {Path(root) for root in roots}, Path(declaring), text.splitlines())

    def test_crate_root_and_mod_rs_look_beside_themselves(self):
        self.assertTrue(self.present("src/lib.rs", "pub mod terrain;"))
        self.assertTrue(self.present("src/lib.rs", "mod r#pieces;"))
        self.assertTrue(self.present("src/pieces/mod.rs", "mod rooms;"))
        self.assertTrue(self.present("tools/entry.rs", "mod helper;", roots=["tools/entry.rs"]))

    def test_other_files_look_in_their_own_folder(self):
        self.assertTrue(self.present("src/terrain.rs", "mod tiles;"))
        self.assertFalse(self.present("src/terrain.rs", "mod pieces;"))
        self.assertFalse(self.present("tools/entry.rs", "mod helper;"))
        # A module that is merely named lib.rs looks in src/x/lib/, not at the file beside it.
        self.assertFalse(self.present("src/x/lib.rs", "mod y;"))

    def test_missing_or_unreadable_declarations_stay_flagged(self):
        self.assertFalse(self.present("src/lib.rs", "mod generated;"))
        self.assertFalse(self.present("src/lib.rs", "mod /* note */ terrain;"))
        self.assertFalse(self.present("src/lib.rs", "mod terrain; mod pieces;"))


class OtherSourcesTest(unittest.TestCase):
    def test_only_source_no_rule_reads_is_listed(self):
        files = {Path(name) for name in (
            "sunrise-mod.toml", "uses.json", "icon.png", "Cargo.toml", "Cargo.lock", "README.md", "LICENSE", "build.rs",
            "src/lib.rs", "src/tables.toml", "assets/gb/textures/block/stone.png", "data/gb/recipe/slab.json",
            "runtime/helper.jar", "native/fast.c", "native/fast.h", "src/shim.S", "docs/README.md")}
        self.assertEqual([path.as_posix() for path in other_sources(files)],
                         ["docs/README.md", "native/fast.c", "native/fast.h", "src/shim.S"])

