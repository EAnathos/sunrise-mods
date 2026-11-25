use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use serde::de::DeserializeOwned;
use sunrise_mod_package::{
    icon::{Icon, MAX_ICON},
    page::MAX_SCREENSHOT_BYTES,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    File,
    Directory,
}

pub type Entries = BTreeMap<OsString, Entry>;

pub struct Tree<'a> {
    checkout: &'a Path,
    problems: Vec<(PathBuf, String)>,
}

impl<'a> Tree<'a> {
    pub fn new(checkout: &'a Path) -> Self {
        Self {
            checkout,
            problems: Vec::new(),
        }
    }

    pub fn problem(&mut self, path: &Path, reason: impl ToString) {
        self.problems.push((path.to_owned(), reason.to_string()));
    }

    pub fn finish(mut self) -> Vec<String> {
        self.problems.sort();
        self.problems
            .into_iter()
            .map(|(path, reason)| {
                // Keep each diagnostic on one line and escape both GitHub Actions command markers.
                format!(
                    "{}: {}",
                    path.to_string_lossy().escape_debug(),
                    reason
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .escape_debug()
                )
                .replace("::", "%3A%3A")
                .replace("##[", "%23%23%5B")
            })
            .collect()
    }

    pub fn entry(&mut self, path: &Path) -> Option<Entry> {
        match fs::symlink_metadata(self.checkout.join(path)) {
            Ok(meta) if meta.is_dir() => Some(Entry::Directory),
            Ok(meta) if meta.is_file() => Some(Entry::File),
            Ok(_) => {
                self.problem(
                    path,
                    "expected a regular file or directory, not a symlink or special file",
                );
                None
            }
            Err(err) => {
                self.problem(path, err);
                None
            }
        }
    }

    // Call only on a directory already classified by entry() or list(). The
    // checker reads a checkout snapshot, not a concurrently edited directory.
    pub fn list(&mut self, dir: &Path) -> Entries {
        let mut files = Entries::new();
        match fs::read_dir(self.checkout.join(dir)) {
            Ok(iter) => {
                for item in iter {
                    match item {
                        Ok(item) => {
                            let name = item.file_name();
                            if let Some(entry) = self.entry(&dir.join(&name)) {
                                files.insert(name, entry);
                            }
                        }
                        Err(err) => self.problem(dir, err),
                    }
                }
            }
            Err(err) => self.problem(dir, err),
        }
        files
    }

    pub fn toml<T: DeserializeOwned>(&mut self, path: &Path, entry: Option<Entry>) -> Option<T> {
        if entry != Some(Entry::File) {
            self.problem(path, "expected a regular TOML file");
            return None;
        }
        match fs::read_to_string(self.checkout.join(path)) {
            Ok(text) => toml::from_str(&text)
                .map_err(|err| self.problem(path, err))
                .ok(),
            Err(err) => {
                self.problem(path, err);
                None
            }
        }
    }

    pub fn icon(&mut self, path: &Path) {
        if !path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
        {
            self.problem(path, "icon extension must match PNG format (.png)");
            return;
        }
        let mut bytes = Vec::new();
        match fs::File::open(self.checkout.join(path))
            .and_then(|file| file.take(MAX_ICON as u64 + 1).read_to_end(&mut bytes))
        {
            Ok(_) => {
                if let Err(err) = Icon::parse(bytes) {
                    self.problem(path, err);
                }
            }
            Err(err) => self.problem(path, err),
        }
    }

    /// A file's bytes, up to `limit`.
    pub fn bytes(&mut self, path: &Path, limit: u64) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        match fs::File::open(self.checkout.join(path))
            .and_then(|file| file.take(limit).read_to_end(&mut bytes))
        {
            Ok(_) => Some(bytes),
            Err(err) => {
                self.problem(path, err);
                None
            }
        }
    }

    pub fn screenshot(&mut self, path: &Path) {
        let result = (|| {
            let file = fs::File::open(self.checkout.join(path))?;
            if file.metadata()?.len() > MAX_SCREENSHOT_BYTES {
                self.problem(path, "screenshot exceeds 5 MiB");
                return Ok(());
            }
            // Match the author-side page reader's signature check. Pages also needs a
            // matching extension to serve the validated file as an image.
            let mut header = Vec::new();
            file.take(8).read_to_end(&mut header)?;
            let extensions: &[_] = if header.starts_with(b"\x89PNG\r\n\x1a\n") {
                &["png"]
            } else if header.starts_with(b"\xff\xd8\xff") {
                &["jpg", "jpeg"]
            } else {
                self.problem(path, "expected a PNG or JPEG file signature");
                return Ok(());
            };
            if !path.extension().and_then(OsStr::to_str).is_some_and(|ext| {
                extensions
                    .iter()
                    .any(|expected| ext.eq_ignore_ascii_case(expected))
            }) {
                self.problem(
                    path,
                    "screenshot extension must match PNG or JPEG format (.png, .jpg, .jpeg)",
                );
            }
            Ok::<_, std::io::Error>(())
        })();
        if let Err(err) = result {
            self.problem(path, err);
        }
    }

    pub fn unknown(&mut self, dir: &Path, entries: Entries) {
        for name in entries.keys() {
            self.problem(&dir.join(name), "unexpected registry entry");
        }
    }
}
