use std::{
    collections::{BTreeMap, btree_map::Entry},
    fs,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
};

use mod_surface::{
    Surface,
    uses::{Fit as SurfaceFit, PLAIN_SCHEMA, USES_FILE, UsesTable},
};
use sunrise_mod_package::{
    Manifest, Package, index::MAX_FILE, manifest::MAX_USES, native_route, sdk,
};
use sunrise_release_sign::{SIGNATURE, SUMS, check, sdk_digest, sha256_hex, trusted_keys_for};

use crate::{
    builds::{self, Build, Offered, SUNRISE},
    escape,
    registry::{self, Registry},
    schema::{Asset, Commit, Fit, FitRelease, Release, Sha256},
    signed::SMALL,
};

const SURFACE: &str = "surface.json";
/// The version of the rules a verdict was judged by. Raise it with any change to what
/// [`Tool::judge`] answers, the pinned Sunrise crates it compares with included: every stored
/// verdict is then judged again.
pub const RULES: u32 = 1;

trait Judge {
    /// The digest of the build's signed `surface.json`, or `None` when this tool cannot read that
    /// surface and so cannot offer the build.
    fn surface(&mut self, build: &Build) -> Result<Option<Sha256>, String>;
    fn judge(&mut self, release: &Release, build: &Offered) -> Result<bool, String>;
}

/// The stored verdict for a release on an offered build, when it was judged from the release's
/// package and the build's surface by the current rules.
pub fn verdict(row: Option<&FitRelease>, release: &Release, build: &Offered) -> Option<bool> {
    let row = row.filter(|row| row.package == release.file.sha256 && row.rules == RULES)?;
    [(&row.fits, true), (&row.does_not_fit, false)]
        .into_iter()
        .find_map(|(cells, verdict)| (cells.get(&build.id)? == &build.surface).then_some(verdict))
}

/// Refuses a registry in which a release has no current verdict on an offered build.
pub fn complete(registry: &Registry) -> Result<(), String> {
    let mut missing = Vec::new();
    for (id, project) in &registry.projects {
        let rows = registry.fits.get(id);
        for release in project.releases.values() {
            let row = rows.and_then(|rows| rows.get(&release.version));
            for build in &registry.builds {
                if verdict(row, release, build).is_none() {
                    missing.push(format!(
                        "{} {} on {}",
                        id.as_str(),
                        release.version,
                        build.id.as_str()
                    ));
                }
            }
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "no current fit verdict for: {}",
        missing.join(", ")
    ))
}

/// Judges every release on every candidate build that has no current verdict, and rewrites
/// `out`. A candidate whose surface this tool cannot read is not offered. Nothing is written
/// when a cell cannot be judged.
fn write_rows(
    registry: &Registry,
    candidates: &[Build],
    judge: &mut impl Judge,
    out: &Path,
) -> Result<(), String> {
    let mut builds = Vec::new();
    for build in candidates {
        let surface = match registry
            .builds
            .iter()
            .find(|old| old.id == build.id && old.commit == build.commit)
        {
            Some(old) => Some(old.surface.clone()),
            None => judge.surface(build)?,
        };
        if let Some(surface) = surface {
            builds.push(Offered {
                id: build.id.clone(),
                commit: build.commit.clone(),
                surface,
            });
        }
    }
    let mut failed = Vec::new();
    let mut files = Vec::new();
    for (id, project) in &registry.projects {
        if project.releases.is_empty() {
            continue;
        }
        let rows = registry.fits.get(id);
        let mut results = Fit {
            release: Vec::new(),
        };
        for release in project.releases.values() {
            let stored = rows.and_then(|rows| rows.get(&release.version));
            let mut row = FitRelease {
                version: release.version.clone(),
                package: release.file.sha256.clone(),
                rules: RULES,
                fits: BTreeMap::new(),
                does_not_fit: BTreeMap::new(),
            };
            for build in &builds {
                let fits = match verdict(stored, release, build) {
                    Some(fits) => fits,
                    None => match judge.judge(release, build) {
                        Ok(fits) => fits,
                        Err(why) => {
                            failed.push(format!(
                                "{} {} on {}: {}",
                                id.as_str(),
                                release.version,
                                build.id.as_str(),
                                why
                            ));
                            continue;
                        }
                    },
                };
                if fits {
                    &mut row.fits
                } else {
                    &mut row.does_not_fit
                }
                .insert(build.id.clone(), build.surface.clone());
            }
            results.release.push(row);
        }
        files.push((id, results));
    }
    if !failed.is_empty() {
        return Err(format!("cannot judge:\n{}", failed.join("\n")));
    }
    if out.exists() {
        fs::remove_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    builds::write(&out.join("builds.json"), &builds)?;
    for (id, results) in files {
        let text = toml::to_string(&results).map_err(|e| e.to_string())?;
        fs::write(out.join(format!("{}.toml", id.as_str())), text).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Brings `registry/fit` in the checkout up to date. Without a candidate list the offered builds
/// stay as they are.
pub fn run(checkout: &Path, candidates: Option<&Path>) -> Result<(), String> {
    let out = checkout.join("registry/fit");
    let load = if out.exists() {
        registry::load_generated
    } else {
        registry::load
    };
    let registry = load(checkout).map_err(|errors| errors.join("\n"))?;
    let candidates = match candidates {
        Some(path) => builds::read(path, |build: &Build| (&build.id, &build.commit))?,
        None => registry
            .builds
            .iter()
            .map(|build| Build {
                id: build.id.clone(),
                commit: build.commit.clone(),
            })
            .collect(),
    };
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let scratch = temp.path().canonicalize().map_err(|e| e.to_string())?;
    write_rows(
        &registry,
        &candidates,
        &mut Tool {
            root: &scratch,
            downloads: Https,
            packages: BTreeMap::new(),
            sdks: BTreeMap::new(),
        },
        &out,
    )
}

trait Download {
    fn get(&self, url: &str, file: &Path, limit: u64) -> Result<(), String>;
}

struct Https;
impl Download for Https {
    fn get(&self, url: &str, file: &Path, limit: u64) -> Result<(), String> {
        let parent = file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        let response = Command::new("curl")
            .args([
                "--disable",
                "--silent",
                "--fail",
                "--location",
                "--globoff",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--write-out",
                "%{http_code}",
                "--max-filesize",
            ])
            .arg(limit.to_string())
            .arg("--output")
            .arg(temp.path())
            .arg("--")
            .arg(url)
            .output()
            .map_err(|e| format!("cannot start curl: {e}"))?;
        if response.status.code() == Some(22) && response.stdout == b"404" {
            return Err("download not found (HTTP 404)".into());
        }
        if !response.status.success() || response.stdout != b"200" {
            return Err(format!(
                "download failed: curl {}, HTTP {}",
                response.status,
                String::from_utf8_lossy(&response.stdout)
            ));
        }
        if temp.as_file().metadata().map_err(|e| e.to_string())?.len() > limit {
            return Err("download exceeds the size limit".into());
        }
        temp.persist(file).map_err(|e| e.to_string())?;
        Ok(())
    }
}

struct Tool<'a, D> {
    root: &'a Path,
    downloads: D,
    packages: BTreeMap<Sha256, (PathBuf, Manifest)>,
    sdks: BTreeMap<Commit, Result<Rc<Sdk>, String>>,
}

fn package(
    downloads: &impl Download,
    root: &Path,
    asset: &Asset,
) -> Result<(PathBuf, Manifest), String> {
    if asset.size == 0 || asset.size > MAX_FILE {
        return Err("package size is outside the registry's package limit".into());
    }
    let file = root.join(format!("{}.smod", asset.sha256.as_str()));
    downloads.get(asset.url.as_str(), &file, asset.size)?;
    let bytes = fs::read(&file).map_err(|e| e.to_string())?;
    if bytes.len() as u64 != asset.size {
        return Err("package size does not match its release record".into());
    }
    if sha256_hex(&bytes) != asset.sha256.as_str() {
        return Err("package does not match its release record's SHA-256".into());
    }
    let manifest = Package::read(&file)?.manifest;
    Ok((file, manifest))
}

fn sdk_file(
    downloads: &impl Download,
    mirror: &Path,
    commit: &Commit,
    name: &str,
    limit: u64,
) -> Result<PathBuf, String> {
    let tag = format!("sdk-{}", commit.as_str());
    let path = mirror.join(&tag).join(name);
    downloads.get(
        &format!("https://github.com/{SUNRISE}/releases/download/{tag}/{name}"),
        &path,
        limit,
    )?;
    Ok(path)
}

struct Sdk {
    content_api: String,
    game: String,
    surface_bytes: Vec<u8>,
    surface: Result<Surface, String>,
}

fn sdk(downloader: &impl Download, mirror: &Path, commit: &Commit) -> Result<Rc<Sdk>, String> {
    let bytes = |name| {
        fs::read(sdk_file(downloader, mirror, commit, name, SMALL)?).map_err(|e| e.to_string())
    };
    let sums = bytes(SUMS)?;
    let signature = String::from_utf8(bytes(SIGNATURE)?).map_err(|e| e.to_string())?;
    let expected = sdk_digest(
        commit.as_str(),
        sdk::INDEX,
        Some(&sums),
        Some(&signature),
        &trusted_keys_for(SUNRISE),
    )?;
    let index = bytes(sdk::INDEX)?;
    check(sdk::INDEX, &expected, &sha256_hex(&index))?;
    let index = sdk::Index::read_for_fit(&index, commit.as_str()).map_err(|e| e.to_string())?;
    let surface_bytes = bytes(SURFACE)?;
    index
        .verify(SURFACE, surface_bytes.as_slice())
        .map_err(|e| e.to_string())?;
    Ok(Rc::new(Sdk {
        content_api: index.content_api,
        game: index.build.game,
        surface: Surface::read(&surface_bytes).map_err(|e| e.to_string()),
        surface_bytes,
    }))
}

impl<D: Download> Tool<'_, D> {
    fn sdk(&mut self, commit: &Commit) -> Result<Rc<Sdk>, String> {
        let mirror = self.root.join("sdk-releases");
        self.sdks
            .entry(commit.clone())
            .or_insert_with(|| sdk(&self.downloads, &mirror, commit))
            .as_ref()
            .map(Rc::clone)
            .map_err(Clone::clone)
    }
}

impl<D: Download> Judge for Tool<'_, D> {
    fn surface(&mut self, build: &Build) -> Result<Option<Sha256>, String> {
        let sdk = self.sdk(&build.commit)?;
        match &sdk.surface {
            Ok(_) => Ok(Some(Sha256::parse(sha256_hex(&sdk.surface_bytes))?)),
            Err(why) => {
                println!("not offering {}: {}", build.id.as_str(), escape(why));
                Ok(None)
            }
        }
    }

    fn judge(&mut self, release: &Release, build: &Offered) -> Result<bool, String> {
        let asset = &release.file;
        let target = self.sdk(&build.commit)?;
        if sha256_hex(&target.surface_bytes) != build.surface.as_str() {
            return Err("the build's surface is not the one it was offered with".into());
        }
        let (file, manifest) = match self.packages.entry(asset.sha256.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(package(&self.downloads, self.root, asset)?),
        };
        if target.content_api != manifest.api {
            return Ok(false);
        }
        let Some(native) = &manifest.native else {
            return Ok(true);
        };
        if native.game != target.game {
            return Ok(false);
        }
        if native.sunrise == build.commit.as_str() {
            return Ok(true);
        }
        if native_route::manifest_game_code(manifest).is_some() {
            println!(
                "{} does not fit {}: made for {}, has native hooks",
                asset.url.as_str(),
                build.id.as_str(),
                release.sunrise.as_str()
            );
            return Ok(false);
        }
        let bytes = Package::entry(file, USES_FILE, MAX_USES)?.ok_or("package has no uses list")?;
        let mut table = UsesTable::read(&bytes).map_err(|e| format!("{USES_FILE}: {e}"))?;
        if table.made_for != native.sunrise {
            return Err("uses list is not for the manifest's pin".into());
        }
        // The manifest has no game code. Unexpected game entries in the package are
        // not part of the typed API comparison and cannot produce either verdict.
        if table
            .entries
            .iter()
            .any(|e| e.game_code.is_some() || e.id.starts_with("game:"))
        {
            return Err("uses list includes game code outside the API comparison".into());
        }
        let surface = target.surface.as_ref().map_err(Clone::clone)?;
        // FitReport prefers known changes to Unversioned. Complete the hook evidence
        // before comparing so a known change cannot hide an incomplete comparison.
        let missing_hook_versions = |table: &UsesTable| {
            surface.hooks.iter().any(|hook| {
                table
                    .entries
                    .binary_search_by(|entry| entry.id.cmp(&hook.id))
                    .is_ok_and(|i| table.entries[i].version.is_none())
            })
        };
        if missing_hook_versions(&table) {
            if table.schema < PLAIN_SCHEMA {
                let pin = Commit::parse(native.sunrise.clone())?;
                let mirror = self.root.join("sdk-releases");
                let baseline = self
                    .sdks
                    .entry(pin.clone())
                    .or_insert_with(|| sdk(&self.downloads, &mirror, &pin))
                    .as_ref()
                    .map_err(Clone::clone)?;
                table
                    .pin_hook_versions(&baseline.surface_bytes)
                    .map_err(|e| e.to_string())?;
            }
            if missing_hook_versions(&table) {
                return Err("uses list has no version for a used hook".into());
            }
        }
        let report = table.fits(table.entries.iter(), surface);
        if let Some(note) = report.deferred {
            return Err(note);
        }
        match report.verdict {
            SurfaceFit::Fits => Ok(true),
            SurfaceFit::DoesNotFit(_) => Ok(false),
            SurfaceFit::Unversioned(_) => Err("uses list has no version for a used hook".into()),
        }
    }
}

#[cfg(test)]
mod tests;
