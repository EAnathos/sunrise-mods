use std::{
    collections::{BTreeMap, btree_map::Entry},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
};

use mod_surface::Surface;
use serde::Deserialize;
use sunrise_mod_package::{
    Manifest, Package, build_data,
    index::{MAX_FILE, native::Bundle},
    sdk,
};
use sunrise_release_sign::{SIGNATURE, SUMS, check, sdk_digest, sha256_hex, trusted_keys_for};

use crate::{
    builds::{self, Build, Considered, Offered, Outcome, SUNRISE},
    escape,
    registry::{self, Registry},
    schema::{Asset, BuildName, Commit, Fit, FitRelease, Release, Sha256},
    signed::SMALL,
};

const SURFACE: &str = "surface.json";
/// The version of the rules a verdict was judged by. Raise it with any change to what
/// [`Tool::judge`] answers, the pinned Sunrise crates it reads a package with included: every
/// stored verdict is then judged again.
pub const RULES: u32 = 3;
/// The stored list of considered Sunrise releases, beside the verdicts.
pub const CONSIDERED: &str = "considered.json";
/// The most the build data may unpack to: its files at the mod tool's bound for one
/// (`cargo-sunrise-mod`'s `BUILD_DATA_LIMIT`).
const BUILD_DATA_LIMIT: u64 = 4 * (256 << 20);
/// The computer the judging job runs on, as an SDK names its per-target assets.
const HOST: &str = "x86_64-unknown-linux-gnu";
/// The most a build's mod tool may unpack to.
const TOOL_LIMIT: u64 = 256 << 20;
/// The `native-check` contract this tool speaks.
const CONTRACT: u32 = 2;

trait Judge {
    /// The digest of the build's signed `surface.json`, or `None` when this tool cannot read that
    /// surface and so cannot offer the build.
    fn surface(&mut self, build: &Build) -> Result<Option<Sha256>, String>;
    /// `None` when the build has nothing to judge a native package with: no verdict is stored,
    /// and a launcher on that build checks the package itself.
    fn judge(&mut self, release: &Release, build: &Offered) -> Result<Option<bool>, String>;
}

/// The stored verdict for a release on an offered build, when it was judged from the release's
/// package and the build's surface by the current rules.
pub fn verdict(row: Option<&FitRelease>, release: &Release, build: &Offered) -> Option<bool> {
    let row = row.filter(|row| row.package == release.file.sha256 && row.rules == RULES)?;
    [(&row.fits, true), (&row.does_not_fit, false)]
        .into_iter()
        .find_map(|(cells, verdict)| (cells.get(&build.id)? == &build.surface).then_some(verdict))
}

/// Judges every release on every candidate build that has no current verdict, and rewrites
/// `out`. A candidate whose surface this tool cannot read is not offered. Nothing is written
/// when a cell cannot be judged.
fn write_rows(
    registry: &Registry,
    candidates: &[Build],
    judge: &mut impl Judge,
    out: &Path,
) -> Result<Vec<Offered>, String> {
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
                        Ok(Some(fits)) => fits,
                        Ok(None) => continue,
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
    Ok(builds)
}

/// Brings `registry/fit` in the checkout up to date. Without a list of considered releases
/// (`registry-bot builds`) the offered builds and the stored list stay as they are; with one, the
/// list is stored with what became of each release.
pub fn run(checkout: &Path, considered: Option<&Path>) -> Result<(), String> {
    let out = checkout.join("registry/fit");
    let stored = out.join(CONSIDERED);
    let load = if out.exists() {
        registry::load_generated
    } else {
        registry::load
    };
    let registry = load(checkout).map_err(|errors| errors.join("\n"))?;
    let mut rows: Option<Vec<Considered>> = match considered {
        Some(path) => {
            let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            Some(serde_json::from_slice(&bytes).map_err(|e| e.to_string())?)
        }
        None => None,
    };
    let kept = match &rows {
        None if stored.exists() => Some(fs::read(&stored).map_err(|e| e.to_string())?),
        _ => None,
    };
    // The judging rewrites the folder, and what Modrinth last answered is not its to drop.
    let unlisted = out.join(crate::jar::UNLISTED);
    let jars = match unlisted.exists() {
        true => Some(fs::read(&unlisted).map_err(|e| e.to_string())?),
        false => None,
    };
    let mut candidates: Vec<Build> = match &rows {
        Some(rows) => rows
            .iter()
            .filter_map(|row| match &row.outcome {
                Outcome::Offered(build) => Some(build.clone()),
                Outcome::Declined(_) => None,
            })
            .collect(),
        None => registry
            .builds
            .iter()
            .map(|build| Build {
                id: build.id.clone(),
                commit: build.commit.clone(),
            })
            .collect(),
    };
    builds::validate(candidates.iter().map(|build| (&build.id, &build.commit)))?;
    candidates.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let scratch = temp.path().canonicalize().map_err(|e| e.to_string())?;
    let offered = write_rows(
        &registry,
        &candidates,
        &mut Tool {
            root: &scratch,
            downloads: Https,
            packages: BTreeMap::new(),
            sdks: BTreeMap::new(),
        },
        &out,
    )?;
    if let Some(bytes) = jars {
        fs::write(&unlisted, bytes).map_err(|e| e.to_string())?;
    }
    if let Some(rows) = &mut rows {
        for row in rows.iter_mut() {
            if let Outcome::Offered(build) = &row.outcome
                && !offered.iter().any(|kept| kept.id == build.id)
            {
                row.outcome = Outcome::Declined("this tool cannot read its surface".to_owned());
            }
        }
        return builds::write(&stored, rows);
    }
    match kept {
        Some(bytes) => fs::write(&stored, bytes).map_err(|e| e.to_string()),
        None => Ok(()),
    }
}

/// What the judging job will store for one release: the offered builds it fits and the rest.
#[derive(Debug, PartialEq, serde::Serialize)]
struct Cells<'a> {
    fits: Vec<&'a BuildName>,
    does_not_fit: Vec<&'a BuildName>,
}

fn cells<'a>(
    release: &Release,
    builds: &'a [Offered],
    judge: &mut impl Judge,
) -> Result<Cells<'a>, String> {
    let mut cells = Cells {
        fits: Vec::new(),
        does_not_fit: Vec::new(),
    };
    let mut failed = Vec::new();
    for build in builds {
        match judge.judge(release, build) {
            Ok(Some(true)) => cells.fits.push(&build.id),
            Ok(Some(false)) => cells.does_not_fit.push(&build.id),
            Ok(None) => {}
            Err(why) => failed.push(format!("{}: {why}", build.id.as_str())),
        }
    }
    if failed.is_empty() {
        Ok(cells)
    } else {
        Err(format!("cannot judge on:\n{}", failed.join("\n")))
    }
}

/// Judges the release row `row`, which is not on `main` yet, on every offered build (`builds`,
/// the stored `builds.json`) and writes the IDs it fits and does not fit to `out`. Nothing is
/// stored: the judging job judges the release again once it is merged. A cell that cannot be
/// judged is an error here, as it would be there.
pub fn release(builds: &Path, row: &Path, out: &Path) -> Result<(), String> {
    let builds = builds::read(builds, |build: &Offered| (&build.id, &build.commit))?;
    let row = fs::read_to_string(row).map_err(|e| format!("{}: {e}", row.display()))?;
    let release = toml::from_str::<Release>(&row).map_err(|e| e.to_string())?;
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let scratch = temp.path().canonicalize().map_err(|e| e.to_string())?;
    let cells = cells(
        &release,
        &builds,
        &mut Tool {
            root: &scratch,
            downloads: Https,
            packages: BTreeMap::new(),
            sdks: BTreeMap::new(),
        },
    )?;
    let bytes = serde_json::to_vec(&cells).map_err(|e| e.to_string())?;
    fs::write(out, bytes).map_err(|e| format!("{}: {e}", out.display()))
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
        crate::github::download(url, temp.path(), limit)?;
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
    index: sdk::Index,
    surface_bytes: Vec<u8>,
    /// Whether this tool reads the surface: a build whose surface it cannot read is not offered.
    surface: Result<(), String>,
    /// The build's own `cargo-sunrise-mod`, when its SDK has one for [`HOST`].
    tool: Option<PathBuf>,
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
    // Checked as the launcher checks it: listed in the signed index, and unpacked only once
    // its bytes match the digest listed there.
    let tool = match index.route_assets("tool", HOST, sdk::Side::Client) {
        Ok(names) => {
            let [name] = names.as_slice() else {
                return Err(format!("expected one mod tool archive, found {names:?}"));
            };
            let size = index.asset(name).map_err(|e| e.to_string())?.size;
            let archive = sdk_file(downloader, mirror, commit, name, size)?;
            let into = archive.with_file_name("tool");
            sdk::unpack(&index, name, &archive, &into, TOOL_LIMIT).map_err(|e| e.to_string())?;
            Some(into.join("cargo-sunrise-mod"))
        }
        Err(sdk::Refusal::Route(_)) => None,
        Err(other) => return Err(other.to_string()),
    };
    Ok(Rc::new(Sdk {
        index,
        surface: Surface::read(&surface_bytes)
            .map(drop)
            .map_err(|e| e.to_string()),
        surface_bytes,
        tool,
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

    fn judge(&mut self, release: &Release, build: &Offered) -> Result<Option<bool>, String> {
        let asset = &release.file;
        let target = self.sdk(&build.commit)?;
        if sha256_hex(&target.surface_bytes) != build.surface.as_str() {
            return Err("the build's surface is not the one it was offered with".into());
        }
        let Tool {
            root,
            downloads,
            packages,
            ..
        } = self;
        let (file, manifest) = match packages.entry(asset.sha256.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(package(downloads, root, asset)?),
        };
        if target.index.content_api != manifest.api {
            return Ok(Some(false));
        }
        if manifest.native.is_none() {
            return Ok(Some(true));
        }
        // A native package is judged by the build's own mod tool, the one the launcher runs: only
        // it knows what that build reads of a package.
        let Some(program) = &target.tool else {
            println!(
                "no verdict for {} on {}: its SDK has no mod tool for {HOST}",
                asset.url.as_str(),
                build.id.as_str()
            );
            return Ok(None);
        };
        // The package's files as the launcher stages them for the tool.
        let staged = root.join("staged").join(asset.sha256.as_str());
        if !staged.exists() {
            let bundle = Bundle::read(file)?;
            for (name, bytes) in &bundle.files {
                let to = staged.join(&manifest.id).join(name);
                fs::create_dir_all(to.parent().expect("a file in the staged folder"))
                    .and_then(|()| fs::write(&to, bytes))
                    .map_err(|e| format!("{}: {e}", to.display()))?;
            }
        }
        let sdk_dir = root
            .join("sdk-releases")
            .join(format!("sdk-{}", build.commit.as_str()));
        let mut plan = serde_json::json!({
            "contract": CONTRACT, "mods": staged, "build": build.commit.as_str(),
        });
        let does_not_fit = |why: &str| {
            println!(
                "{} does not fit {}: {}",
                asset.url.as_str(),
                build.id.as_str(),
                escape(why)
            );
            Ok(Some(false))
        };
        loop {
            // The tool reads a package nobody has reviewed yet: it gets none of this job's
            // environment.
            let mut child = Command::new(program)
                .arg("native-check")
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("{}: {e}", program.display()))?;
            child
                .stdin
                .take()
                .expect("piped")
                .write_all(format!("{plan}\n").as_bytes())
                .map_err(|e| e.to_string())?;
            let output = child.wait_with_output().map_err(|e| e.to_string())?;
            let unread = || {
                format!(
                    "the mod tool of {} gave no answer this tool reads ({})",
                    build.id.as_str(),
                    output.status
                )
            };
            let reply: Reply = serde_json::from_slice(&output.stdout).map_err(|_| unread())?;
            let checked = match (output.status.code(), reply) {
                // The tool could not read the package: neither can a launcher on this build.
                (Some(1), Reply::Error { error }) => return does_not_fit(&error.message),
                (Some(0), Reply::Checked { mods }) => {
                    let [checked] = <[Checked; 1]>::try_from(mods).map_err(|_| unread())?;
                    checked
                }
                _ => return Err(unread()),
            };
            match (checked.verdict, checked.refusal, checked.needs) {
                (Some(verdict), None, None) => {
                    // The verdicts the launcher builds by (`Verdict::usable`).
                    return if matches!(verdict["kind"].as_str(), Some("made-for" | "fits")) {
                        Ok(Some(true))
                    } else {
                        does_not_fit(&verdict.to_string())
                    };
                }
                (None, Some(why), None) => return does_not_fit(&why),
                (None, None, Some(Need::Surface)) if plan.get("surface").is_none() => {
                    plan["surface"] = serde_json::json!(sdk_dir.join(SURFACE));
                }
                (None, None, Some(Need::BuildData)) if plan.get("build_data").is_none() => {
                    let into = sdk_dir.join("build-data");
                    if !into.exists() {
                        let archive = sdk_file(
                            downloads,
                            &root.join("sdk-releases"),
                            &build.commit,
                            build_data::ARCHIVE,
                            target
                                .index
                                .asset(build_data::ARCHIVE)
                                .map_err(|e| e.to_string())?
                                .size,
                        )?;
                        sdk::unpack(
                            &target.index,
                            build_data::ARCHIVE,
                            &archive,
                            &into,
                            BUILD_DATA_LIMIT,
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    plan["build_data"] = serde_json::json!(into);
                }
                _ => return Err(unread()),
            }
        }
    }
}

/// `native-check`'s reply line (the SDK reference's "native-check").
#[derive(Deserialize)]
#[serde(untagged)]
enum Reply {
    Error { error: ReplyError },
    Checked { mods: Vec<Checked> },
}

#[derive(Deserialize)]
struct ReplyError {
    message: String,
}

/// How one mod stands: its verdict, its refusal, or what the tool needs to give one.
#[derive(Deserialize)]
struct Checked {
    verdict: Option<serde_json::Value>,
    refusal: Option<String>,
    needs: Option<Need>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Need {
    Surface,
    BuildData,
}

#[cfg(test)]
mod tests;
