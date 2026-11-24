use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::Command,
};

use serde::{Deserialize, Serialize};
use sunrise_mod_package::index::stable_tag;
use sunrise_release_sign::{Channel, build_title_version, is_build, nightly_date};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    command_output, game_build,
    schema::{BuildName, Commit, RepositoryName, Sha256},
};

pub const SUNRISE: &str = "sunrise-org/sunrise";
const SURFACE: &str = "surface.json";
const SDK: &str = "sdk.json";
// Registry v2's offered-build policy: ten published builds with a surface per rolling channel.
const ROLLING_BUILDS: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub id: BuildName,
    pub commit: Commit,
}

/// A build the registry offers: its verdicts are complete for the surface named here.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Offered {
    pub id: BuildName,
    pub commit: Commit,
    /// SHA-256 of the build's signed `surface.json`.
    pub surface: Sha256,
}

/// A published Sunrise release that carries an SDK descriptor, and what the offered-build policy
/// made of it. The judging job stores these, so the timer can tell a release nobody has looked
/// at from one that was looked at and declined.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Considered {
    pub tag: String,
    /// The release's target as GitHub lists it. A rolling tag that moves is a new release.
    pub target: String,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Offered(Build),
    /// Why the release is not an offered build.
    Declined(String),
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    target_commitish: String,
    name: Option<String>,
    published_at: Option<String>,
    draft: bool,
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    size: u64,
}

trait Releases {
    fn releases(&self) -> Result<Vec<GithubRelease>, String>;
    fn stable_commits(&self) -> Result<BTreeMap<String, Commit>, String>;
    /// The commit named by the game release's own signed `sdk.json`, verified under `build`.
    fn signed_commit(
        &self,
        release: &GithubRelease,
        build: &str,
        channel: Option<Channel>,
    ) -> Result<String, String>;
}

struct Github;

impl Releases for Github {
    fn releases(&self) -> Result<Vec<GithubRelease>, String> {
        let mut gh = Command::new("gh");
        gh.args([
            "api",
            "--paginate",
            "--slurp",
            "repos/sunrise-org/sunrise/releases?per_page=100",
        ]);
        let pages: Vec<Vec<GithubRelease>> = serde_json::from_slice(&command_output(&mut gh)?)
            .map_err(|e| format!("GitHub release list: {e}"))?;
        Ok(pages.into_iter().flatten().collect())
    }

    fn signed_commit(
        &self,
        release: &GithubRelease,
        build: &str,
        channel: Option<Channel>,
    ) -> Result<String, String> {
        let size = |name: &str| {
            let mut rows = release.assets.iter().filter(|row| row.name == name);
            match (rows.next(), rows.next()) {
                (Some(row), None) => Ok(row.size),
                _ => Err(format!("expected exactly one {name} asset")),
            }
        };
        let repository = RepositoryName::parse(SUNRISE.to_owned())?;
        let sdk = game_build::signed_sdk(&release.tag_name, &size, &repository, build, channel)?;
        Ok(sdk.build.commit)
    }

    fn stable_commits(&self) -> Result<BTreeMap<String, Commit>, String> {
        let bytes = command_output(Command::new("git").args([
            "ls-remote",
            "--tags",
            "https://github.com/sunrise-org/sunrise.git",
            "refs/tags/26.3-*",
        ]))?;
        let refs = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
        let mut tags = BTreeMap::new();
        let mut peeled = BTreeMap::new();
        for line in refs.lines() {
            let (commit, reference) = line.split_once('\t').ok_or("unexpected Git ref listing")?;
            let tag = reference
                .strip_prefix("refs/tags/")
                .ok_or("expected a Git tag")?;
            if let Some(tag) = tag.strip_suffix("^{}") {
                peeled.insert(tag.to_owned(), Commit::parse(commit.to_owned())?);
            } else {
                tags.insert(tag.to_owned(), Commit::parse(commit.to_owned())?);
            }
        }
        tags.extend(peeled);
        Ok(tags)
    }
}

fn select(source: &impl Releases) -> Result<Vec<Considered>, String> {
    let releases = source.releases()?;
    let has_surface: BTreeSet<_> = releases
        .iter()
        .filter(|r| !r.draft)
        .filter(|r| r.assets.iter().any(|asset| asset.name == SURFACE))
        .filter_map(|r| r.tag_name.strip_prefix("sdk-"))
        .collect();
    let mut stable = Vec::new();
    let mut nightly = Vec::new();
    let mut dev = Vec::new();
    let mut tags = None;
    let mut declined = BTreeMap::new();
    for release in &releases {
        let tag = &release.tag_name;
        if release.draft {
            continue;
        }
        if !release.prerelease && tag.starts_with("26.3-") && stable_tag(tag).is_some() {
            if tags.is_none() {
                tags = Some(source.stable_commits()?);
            }
            let commit = tags
                .as_ref()
                .and_then(|tags| tags.get(tag))
                .ok_or_else(|| format!("stable release {tag} has no Git tag"))?;
            stable.push((
                Build {
                    id: BuildName::parse(tag.clone())?,
                    commit: commit.clone(),
                },
                release,
                None,
            ));
            continue;
        }
        let (channel, list) = if tag == "nightly"
            || nightly_date(tag).is_some_and(|date| tag.len() == "nightly-".len() + date.len())
        {
            (Channel::Nightly, &mut nightly)
        } else if is_build(Channel::Dev, tag) {
            (Channel::Dev, &mut dev)
        } else {
            declined.insert(tag.as_str(), "not a game build");
            continue;
        };
        if !release.prerelease {
            declined.insert(tag.as_str(), "a Dev or Nightly build must be a prerelease");
            continue;
        }
        if !has_surface.contains(release.target_commitish.as_str()) {
            declined.insert(
                tag.as_str(),
                "no SDK release with a surface.json is published for its commit",
            );
            continue;
        }
        let commit = Commit::parse(release.target_commitish.clone())?;
        let id = release
            .name
            .as_deref()
            .and_then(|title| build_title_version(channel, title))
            .ok_or_else(|| format!("release {tag} does not name an exact build in its title"))?;
        let short = id
            .rsplit_once('-')
            .map(|(_, short)| short)
            .ok_or("missing build suffix")?;
        if !commit.as_str().starts_with(short)
            || (channel == Channel::Dev && id != *tag)
            || (channel == Channel::Nightly
                && tag != "nightly"
                && !id.starts_with(&format!("{tag}-")))
        {
            return Err(format!(
                "release {tag} and its commit do not match the title's build"
            ));
        }
        let date = release
            .published_at
            .as_deref()
            .ok_or("missing release publication time")?;
        let date = OffsetDateTime::parse(date, &Rfc3339).map_err(|e| e.to_string())?;
        list.push((
            date,
            (
                Build {
                    id: BuildName::parse(id)?,
                    commit,
                },
                release,
                Some(channel),
            ),
        ));
    }
    for list in [&mut nightly, &mut dev] {
        list.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.0.id.cmp(&b.1.0.id)));
        let mut seen = BTreeSet::new();
        for (_, row) in list.drain(..) {
            let tag = row.1.tag_name.as_str();
            if !seen.insert(row.0.id.clone()) {
                declined.insert(tag, "a newer release is the same build");
            } else if seen.len() > ROLLING_BUILDS {
                declined.insert(tag, "older than the ten newest builds of its channel");
            } else {
                stable.push(row);
            }
        }
    }
    // A tag, target commit or title is only where to look. What ties a build ID to its commit is
    // the game release's own signature over the `sdk.json` that names the commit.
    let mut offered = BTreeMap::new();
    for (build, release, channel) in stable {
        let id = build.id.as_str();
        let signed = source
            .signed_commit(release, id, channel)
            .map_err(|e| format!("build {id}: {e}"))?;
        if signed != build.commit.as_str() {
            return Err(format!(
                "build {id}: the release's signed SDK descriptor names another commit"
            ));
        }
        offered.insert(release.tag_name.as_str(), build);
    }
    validate(offered.values().map(|build| (&build.id, &build.commit)))?;
    let mut considered: Vec<_> = releases
        .iter()
        .filter(|r| !r.draft)
        .filter_map(|release| {
            let tag = release.tag_name.as_str();
            let outcome = match offered.remove(tag) {
                Some(build) => Outcome::Offered(build),
                // The timer asks about exactly the releases that carry an SDK descriptor.
                None if release.assets.iter().any(|asset| asset.name == SDK) => {
                    Outcome::Declined(declined[tag].to_owned())
                }
                None => return None,
            };
            Some(Considered {
                tag: tag.to_owned(),
                target: release.target_commitish.clone(),
                outcome,
            })
        })
        .collect();
    considered.sort_unstable_by(|a, b| (&a.tag, &a.target).cmp(&(&b.tag, &b.target)));
    Ok(considered)
}

/// Writes every release the policy considered, offered or declined, to `out`.
pub fn discover(out: &Path) -> Result<(), String> {
    let considered = select(&Github)?;
    for row in &considered {
        match &row.outcome {
            Outcome::Offered(build) => println!("{}: offering {}", row.tag, build.id.as_str()),
            Outcome::Declined(why) => println!("{}: {why}", row.tag),
        }
    }
    write(out, &considered)
}

pub fn write(path: &Path, builds: &[impl Serialize]) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(builds).map_err(|e| e.to_string())?;
    fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Reads a list of candidate builds ([`Build`]) or of offered ones ([`Offered`]).
pub fn read<T: serde::de::DeserializeOwned>(
    path: &Path,
    name: impl Fn(&T) -> (&BuildName, &Commit),
) -> Result<Vec<T>, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let builds: Vec<T> = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    validate(builds.iter().map(name))?;
    Ok(builds)
}

pub fn validate<'a>(
    builds: impl Iterator<Item = (&'a BuildName, &'a Commit)>,
) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    for (id, commit) in builds {
        let id = id.as_str();
        if Channel::of(id).is_some()
            && id
                .rsplit_once('-')
                .is_none_or(|(_, short)| !commit.as_str().starts_with(short))
        {
            return Err(format!("build {id} does not match its commit"));
        }
        if !ids.insert(id) {
            return Err(format!("duplicate offered build {id}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
