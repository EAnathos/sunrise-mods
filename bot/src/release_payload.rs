//! Check the attested package and page without executing their contents.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read},
    path::Path,
};

use semver::Version;
use sunrise_mod_package::{
    Content, Package,
    manifest::{MANIFEST, MAX_MANIFEST, is_unresolved},
    page::{self, MAX_SCREENSHOT_BYTES},
};

use unicode_normalization::UnicodeNormalization;

use crate::schema::{self, Dependency, Kind, Link, ModId, RelativePath, Screenshot, Slug, Text};

pub struct Payload {
    pub package: Package,
    pub page: schema::Page,
    pub images: BTreeMap<RelativePath, Vec<u8>>,
    pub dependencies: Vec<Dependency>,
}

pub fn read(
    smod: &Path,
    archive: &Path,
    project: &schema::Project,
    version: &Version,
    categories: &page::Categories,
) -> Result<Payload, String> {
    let package = Package::read(smod)?;
    let manifest = &package.manifest;
    if manifest.id != project.mod_id.as_str()
        || manifest.version != *version
        || is_unresolved(version)
    {
        return Err(
            "package manifest ID or version does not match the project and release tag".into(),
        );
    }
    if !matches!(project.kind, Kind::Mod | Kind::NativeMod) {
        return Err("registered project is not a mod".into());
    }
    // Validate resources and runtime data through the same readers as the game/tool.
    Content::read(smod, manifest)?;
    package.read_runtime(smod)?;
    let icon = package.icon.as_ref().ok_or("package is missing icon.png")?;
    let mut files = read_page_archive(archive)?;
    let raw = files
        .remove(page::PAGE_FILE)
        .ok_or("page archive is missing page.toml")?;
    let page = page::Page::parse(
        std::str::from_utf8(&raw).map_err(|e| e.to_string())?,
        manifest,
    )?;
    let problems = page.release_problems(Some(categories));
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    let mut selected = BTreeSet::new();
    let (kind, kind_name) = if package.is_native() {
        (page::Kind::NativeMod, "native-mod")
    } else {
        (page::Kind::Mod, "mod")
    };
    for id in &page.categories {
        if !selected.insert(id)
            || !categories
                .category
                .iter()
                .any(|row| row.id == *id && row.kinds.contains(&kind))
        {
            return Err(format!(
                "category {id:?} is repeated or does not apply to {kind_name}"
            ));
        }
    }
    let mut screenshots = Vec::new();
    let mut images = BTreeMap::new();
    let icon_path = RelativePath::parse("icon.png".into())?;
    images.insert(icon_path.clone(), icon.bytes().to_vec());
    for shot in &page.screenshots {
        let bytes = files
            .remove(shot.path())
            .ok_or_else(|| format!("missing or repeated screenshot {}", shot.path()))?;
        shot.check_image(&bytes)?;
        // The pinned shared reader checks signature; the resolved layout additionally requires the matching extension.
        match Path::new(shot.path()).extension().and_then(|s| s.to_str()) {
            Some("png") if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => (),
            Some("jpg" | "jpeg") if bytes.starts_with(b"\xff\xd8\xff") => (),
            _ => {
                return Err(format!(
                    "{}: screenshot extension does not match its image format",
                    shot.path()
                ));
            }
        }
        let relative = shot
            .path()
            .strip_prefix("page/")
            .ok_or("screenshot must be under page/")?;
        let path = RelativePath::parse(relative.into())?;
        if !portable_path(path.as_str()) {
            return Err(format!(
                "{}: screenshot path is not portable",
                path.as_str()
            ));
        }
        if images.insert(path.clone(), bytes).is_some() {
            return Err("screenshot conflicts with the resolved package icon".into());
        }
        screenshots.push(Screenshot {
            path,
            caption: shot.caption.clone().unwrap_or_default(),
        });
    }
    if !files.is_empty() {
        return Err("page bundle contains files not named by page.toml".into());
    }
    let dependencies = manifest
        .dependencies
        .iter()
        .map(|(id, rule)| {
            let details = rule.details();
            Ok(Dependency {
                mod_id: ModId::parse(id.clone())?,
                version: details.version,
                project: None,
                required: !details.optional,
            })
        })
        .collect::<Result<_, &str>>()?;
    let page = schema::Page {
        name: Text::parse(page.name)?,
        summary: Text::parse(page.summary)?,
        description: Text::parse(page.description)?,
        authors: page
            .authors
            .into_iter()
            .map(Text::parse)
            .collect::<Result<_, _>>()?,
        license: Text::parse(manifest.license.clone())?,
        homepage: page.homepage.map(Link::parse).transpose()?,
        issues: page.issues.map(Link::parse).transpose()?,
        source: None,
        icon: icon_path,
        categories: page
            .categories
            .into_iter()
            .map(Slug::parse)
            .collect::<Result<_, _>>()?,
        screenshots,
    };
    Ok(Payload {
        package,
        page,
        images,
        dependencies,
    })
}

fn read_page_archive(path: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut tar = tar::Archive::new(File::open(path).map_err(|e| e.to_string())?);
    let mut files = BTreeMap::new();
    for entry in tar.entries().map_err(|e| e.to_string())?.raw(true) {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.header().as_ustar().is_none() || !entry.header().entry_type().is_file() {
            return Err("expected only regular ustar files in the page archive".into());
        }
        let name = String::from_utf8(entry.path_bytes().into_owned()).map_err(|e| e.to_string())?;
        RelativePath::parse(name.clone())?;
        let limit = if name == page::PAGE_FILE {
            MAX_MANIFEST
        } else if name.starts_with("page/") {
            MAX_SCREENSHOT_BYTES
        } else {
            return Err("page archive may only contain page.toml and files under page/".into());
        };
        if entry.size() > limit {
            return Err(format!("{name}: exceeds its package size limit"));
        }
        let mut bytes = Vec::new();
        entry
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if files.insert(name, bytes).is_some() {
            return Err("duplicate page archive entry".into());
        }
    }
    Ok(files)
}

/// Extract validated regular files into a fresh directory for the isolated scanner/build jobs.
/// The names must also survive case-insensitive and Win32 artifact extraction unchanged.
pub fn source(smod: &Path, dest: &Path, native: bool) -> Result<(), String> {
    let mut zip = zip::ZipArchive::new(File::open(smod).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if native && zip.by_name("Cargo.lock").is_err() {
        return Err("native package is missing Cargo.lock".into());
    }
    fs::create_dir(dest).map_err(|e| e.to_string())?;
    let mut paths = BTreeSet::new();
    let mut dirs = BTreeMap::new();
    for i in 0..zip.len() {
        let file = zip.by_index(i).map_err(|e| e.to_string())?;
        if file.is_dir() {
            continue;
        }
        let name = file.name().map_err(|e| e.to_string())?.into_owned();
        let path = RelativePath::parse(name.clone())?;
        if !matches!(
            file.unix_mode().unwrap_or_default() & 0o170000,
            0 | 0o100000
        ) {
            return Err(format!("{name}: expected a regular file in the .smod"));
        }
        if !native
            && name != MANIFEST
            && name != "icon.png"
            && !name.starts_with("assets/")
            && !name.starts_with("data/")
        {
            return Err(format!("{name}: unexpected file in content-only package"));
        }
        if !portable_path(path.as_str()) || !paths.insert(alias_key(&name)) {
            return Err(format!(
                "{name}: path is not portable or aliases another package entry"
            ));
        }
        let target = dest.join(path.as_str());
        let parent = target
            .parent()
            .ok_or("package file has no parent directory")?;
        for directory in parent.ancestors().take_while(|p| *p != dest) {
            let key = alias_key(&directory.to_string_lossy());
            if let Some(prior) = dirs.insert(key, directory.to_owned())
                && prior != directory
            {
                return Err(format!("{name}: aliases another package directory"));
            }
        }
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let mut out = File::create_new(&target).map_err(|e| e.to_string())?;
        let size = file.size();
        if io::copy(&mut file.take(size.saturating_add(1)), &mut out).map_err(|e| e.to_string())?
            != size
        {
            return Err(format!(
                "{name}: uncompressed size does not match its ZIP entry"
            ));
        }
    }
    if paths
        .iter()
        .any(|name| dirs.contains_key(&alias_key(&dest.join(name).to_string_lossy())))
    {
        return Err("package file aliases a directory".into());
    }
    Ok(())
}

/// One key for every spelling a case-insensitive or normalization-insensitive file system (APFS,
/// NTFS) would open as the same path, so each host builds the tree the scanner read.
fn alias_key(path: &str) -> String {
    // Unicode's canonical caseless form (definition D145): NFD, full case folding, NFD again.
    // Upper- or lowercasing alone keeps some pairs apart, such as U+03F4 and U+03B8.
    let folded = caseless::default_case_fold_str(&path.nfd().collect::<String>());
    folded.nfd().collect()
}

fn portable_path(path: &str) -> bool {
    path.split('/').all(|part| {
        if part.contains(['<', '>', '"', '|', '?', '*'])
            || part.ends_with(['.', ' '])
            || part.eq_ignore_ascii_case(".git")
        {
            return false;
        }
        let stem = part
            .split('.')
            .next()
            .unwrap_or(part)
            .trim_end()
            .to_ascii_uppercase();
        !matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) && !stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|n| {
                matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
    })
}

impl Payload {
    pub fn write_page(&self, dir: &Path) -> Result<(), String> {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        for (path, bytes) in &self.images {
            let target = dir.join(path.as_str());
            let parent = target
                .parent()
                .ok_or("image path has no parent directory")?;
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            fs::write(target, bytes).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string_pretty(&self.page).map_err(|e| e.to_string())?;
        fs::write(dir.join(page::PAGE_FILE), text).map_err(|e| e.to_string())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_or_nonregular_page_entries() {
        let dir = tempfile::tempdir().unwrap();
        for entry_type in [tar::EntryType::Regular, tar::EntryType::Symlink] {
            let path = dir.path().join("page.tar");
            let mut tar = tar::Builder::new(File::create(&path).unwrap());
            for _ in 0..2 {
                let mut header = tar::Header::new_ustar();
                header.set_entry_type(entry_type);
                header.set_size(0);
                tar.append_data(&mut header, "page.toml", io::empty())
                    .unwrap();
            }
            tar.finish().unwrap();
            assert!(read_page_archive(&path).is_err());
        }
    }

    #[test]
    fn source_paths_cannot_resolve_to_windows_devices_or_alternate_names() {
        for name in [
            "src/con.rs",
            "src/COM¹.txt",
            "src/a?.rs",
            "src/dir./x.rs",
            "src/dir /x.rs",
        ] {
            assert!(!portable_path(name), "accepted {name}");
        }
        assert!(portable_path("src/control.rs"));
    }

    fn native_source(names: &[&str]) -> Result<(), String> {
        let dir = tempfile::tempdir().unwrap();
        let smod = dir.path().join("mod.smod");
        let mut zip = zip::ZipWriter::new(File::create(&smod).unwrap());
        for name in ["Cargo.lock"].iter().chain(names) {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
        }
        zip.finish().unwrap();
        source(&smod, &dir.path().join("out"), true)
    }

    #[test]
    fn source_paths_cannot_differ_only_in_unicode_normalization_or_case() {
        // "é" precomposed (U+00E9) against "e" with a combining acute (U+0301): one path on APFS.
        let (composed, decomposed) = ("caf\u{e9}", "cafe\u{301}");
        for names in [
            // Two files.
            [format!("src/{composed}.rs"), format!("src/{decomposed}.rs")],
            // Two directories.
            [format!("{composed}/a.rs"), format!("{decomposed}/b.rs")],
            // A file against a directory.
            [format!("src/{composed}"), format!("src/{decomposed}/a.rs")],
            // The same three shapes still hold for case, with normalization mixed in.
            [
                format!("src/{composed}.rs"),
                "src/CAFE\u{301}.rs".to_string(),
            ],
            ["Lib/a.rs".to_string(), "lib/b.rs".to_string()],
            ["src/mod".to_string(), "src/MOD/a.rs".to_string()],
            // Case pairs that neither uppercasing nor lowercasing alone brings together.
            ["src/\u{3f4}.rs".to_string(), "src/\u{3b8}.rs".to_string()],
            ["stra\u{df}e/a.rs".to_string(), "STRASSE/b.rs".to_string()],
        ] {
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            assert!(native_source(&names).is_err(), "accepted {names:?}");
        }
        assert_eq!(native_source(&["src/caf\u{e9}.rs", "src/cafe.rs"]), Ok(()));
    }

    /// What the pinned Sunrise tools accept as a native mod's source: C beside the Rust, never
    /// a file that is already machine code, whatever it is called.
    #[test]
    fn a_native_package_carries_c_sources_and_no_prebuilt_binary() {
        let manifest = format!(
            "id='wayfinder'\nname='Wayfinder'\nversion='1.2.0'\n[native]\ngame='26.3'\nsunrise='{}'\n",
            "d".repeat(40)
        );
        let read = |name: &str, bytes: &[u8]| {
            let body = Package::write_files(
                &manifest,
                &[
                    ("Cargo.toml", b"[package]\nname='wayfinder'\n".as_slice()),
                    ("build.rs", b"fn main() {}\n"),
                    ("src/lib.rs", b"pub fn read_map() {}\n"),
                    (name, bytes),
                ],
            )?;
            let dir = tempfile::tempdir().unwrap();
            let smod = dir.path().join("wayfinder.smod");
            std::fs::write(&smod, body).unwrap();
            Package::read(&smod).map(|_| ())
        };
        assert_eq!(
            read("vendor/add.c", b"int add(int a, int b) { return a + b; }\n"),
            Ok(())
        );
        let refused = read("vendor/add.c", b"\x7fELF\x02\x01\x01\0").unwrap_err();
        assert!(refused.contains("vendor/add.c"), "{refused}");
    }
}
