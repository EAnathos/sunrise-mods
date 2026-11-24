use std::{collections::BTreeSet, fs, path::Path};

use semver::Version;
use sunrise_mod_package::{
    Package,
    index::{MAX_FILE, sha256_file},
    page,
};
use time::OffsetDateTime;

use crate::{
    dependency,
    game_build::Builds,
    github::{self, Attestation, GitHub, Observation, Repo},
    registry::{self, ProjectEntry, Registry},
    release_payload::{self, Payload},
    schema::{
        self, Asset, HttpsUrl, PackagePath, ProjectId, PublishedAt, Release, RepositoryId,
        RepositoryName, Sha256, SignerWorkflowRef, Text,
    },
};

struct Candidate<'a> {
    release: &'a github::Release,
    version: Version,
    published: OffsetDateTime,
}

fn tag_version(project: &schema::Project, tag: &str) -> Option<Version> {
    let tail = match project.path {
        PackagePath::Root(_) => tag,
        PackagePath::Folder(_) => tag
            .strip_prefix(project.mod_id.as_str())?
            .strip_prefix('/')?,
    };
    Version::parse(tail.strip_prefix('v')?).ok()
}

struct Approved<'a> {
    row: &'a Release,
    published: OffsetDateTime,
}

/// The approval order comes from the committed rows alone, so a release the author
/// later takes down still orders the ones after it.
fn latest_approved(entry: &ProjectEntry) -> Option<Approved<'_>> {
    entry
        .releases
        .values()
        .map(|row| Approved {
            row,
            published: row.published.time(),
        })
        .max_by(|a, b| (a.published, &a.row.version).cmp(&(b.published, &b.row.version)))
}

fn candidates<'a>(
    entry: &ProjectEntry,
    releases: &'a [github::Release],
    approved: Option<&Approved<'_>>,
) -> Vec<Candidate<'a>> {
    let mut candidates = releases
        .iter()
        .filter_map(|release| {
            let published = release.published()?;
            let version = tag_version(&entry.project, &release.tag_name)?;
            if entry.releases.contains_key(&version)
                || approved.is_some_and(|last| {
                    (published, &version) <= (last.published, &last.row.version)
                })
            {
                return None;
            }
            Some(Candidate {
                release,
                version,
                published,
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| (a.published, &a.version).cmp(&(b.published, &b.version)));
    if approved.is_none() {
        candidates.reverse();
    }
    candidates
}

pub fn run(checkout: &Path, out: &Path, github: &dyn GitHub) -> Result<(), String> {
    let mut registry = load(checkout)?;
    fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let mut summary = String::from("### Registry release watcher\n\n");
    if registry.repositories.is_empty() {
        summary.push_str("No repositories are registered.\n");
        return fs::write(out.join("watch.md"), summary).map_err(|e| e.to_string());
    }
    let branches = github.release_branches()?;
    let mut builds = Builds::new(github)?;
    let categories = categories(&registry)?;
    let workflow = workflow(&registry)?.to_owned();
    for (&repository, repo) in &mut registry.repositories {
        let live: Result<_, String> = (|| {
            let live = github.repository(Repo::Registered(repository))?;
            if live.id != repository || live.private {
                return Err("registered repository is no longer public or has the wrong ID".into());
            }
            Ok(live)
        })();
        let live = match live {
            Ok(live) => live,
            Err(reason) => {
                note(&mut summary, repo.name.as_str(), &reason);
                continue;
            }
        };
        if repo.name != live.name {
            note(
                &mut summary,
                repo.name.as_str(),
                &format!("renamed to {}", live.name.as_str()),
            );
            repo.name = live.name;
            let path = out.join("rename/registry/repositories");
            fs::create_dir_all(&path).map_err(|e| e.to_string())?;
            let text = toml::to_string_pretty(&*repo).map_err(|e| e.to_string())?;
            fs::write(path.join(format!("{}.toml", repository.get())), text)
                .map_err(|e| e.to_string())?;
        }
        let releases = match github.releases(Repo::Registered(repository)) {
            Ok(releases) => releases,
            Err(reason) => {
                note(&mut summary, repo.name.as_str(), &reason);
                continue;
            }
        };
        for package in &repo.package {
            let id = &package.project;
            let entry = registry
                .projects
                .get(id)
                .ok_or("registered project is missing")?;
            if entry.project.paused || has_open_release(&branches.waiting, id) {
                continue;
            }
            let approved = latest_approved(entry);
            let mut candidates = candidates(entry, &releases, approved.as_ref());
            if approved.is_none() {
                // Registration observes only the newest publication. Neither an unsigned or
                // rejected release nor an inconclusive verification opens an older release.
                candidates.truncate(1);
            }
            for candidate in candidates {
                let branch = format!("release/{}/{}", id.as_str(), candidate.version);
                if branches.handled.contains(&branch) {
                    continue;
                }
                let label = format!("{} {}", id.as_str(), candidate.version);
                let work = tempfile::tempdir().map_err(|e| e.to_string())?;
                let request = Request {
                    github,
                    entry,
                    repository_id: repository,
                    repository: &repo.name,
                    workflow: &workflow,
                    release: candidate.release,
                    version: &candidate.version,
                };
                let assets = release_assets(&request, work.path(), None);
                let assets = match assets {
                    Ok(assets) => assets,
                    Err(reason) => {
                        note(&mut summary, &label, &reason.to_string());
                        continue;
                    }
                };
                let result = (|| {
                    let (row, payload) = prepared(
                        &request,
                        candidate.published,
                        assets,
                        work.path(),
                        &categories,
                        &mut builds,
                        None,
                    )?;
                    release_payload::source(
                        &work.path().join("package.smod"),
                        &work.path().join("source"),
                        payload.package.is_native(),
                    )?;
                    let root = out.join("releases").join(id.as_str());
                    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
                    let stage = tempfile::tempdir_in(&root).map_err(|e| e.to_string())?;
                    write_release(
                        &row,
                        &payload,
                        &stage.path().join(project_path(id)),
                        kind_change(checkout, entry, &row)?.as_deref(),
                    )?;
                    fs::rename(stage.path(), root.join(row.version.to_string()))
                        .map_err(|e| e.to_string())
                })();
                match result {
                    Ok(()) => {
                        note(&mut summary, &label, "verified; ready to open a release PR");
                        break;
                    }
                    Err(reason) => {
                        note(&mut summary, &label, &reason);
                    }
                }
            }
        }
    }
    if summary == "### Registry release watcher\n\n" {
        summary.push_str("No new releases to propose.\n");
    }
    fs::write(out.join("watch.md"), summary).map_err(|e| e.to_string())
}

/// The first required dependency no other project on main can supply: the build would only fail
/// on it later, with a compiler log instead of a reason.
fn unsupplied(registry: &Registry, id: &ProjectId, row: &Release) -> Option<String> {
    let revoked = dependency::revocations(&registry.revoked);
    row.dependencies
        .iter()
        .filter(|dependency| dependency.required)
        .find(|dependency| {
            !registry.projects.iter().any(|(provider, entry)| {
                provider != id
                    && entry.releases.values().any(|release| {
                        dependency::satisfied_by(
                            dependency,
                            provider,
                            &entry.project.mod_id,
                            release,
                            &revoked,
                        )
                    })
            })
        })
        .map(|dependency| {
            format!(
                "needs {} {}, which has no such release in the registry: release the library first",
                dependency.mod_id.as_str(),
                dependency.version
            )
        })
}

fn has_open_release(open: &BTreeSet<String>, id: &ProjectId) -> bool {
    let prefix = format!("release/{}/", id.as_str());
    open.range(prefix.clone()..)
        .next()
        .is_some_and(|branch| branch.starts_with(&prefix))
}

pub fn check(
    checkout: &Path,
    id: &ProjectId,
    claim: &Path,
    out: &Path,
    github: &dyn GitHub,
) -> Result<(), String> {
    let registry = load(checkout)?;
    let entry = registry
        .projects
        .get(id)
        .ok_or("project is not registered on main")?;
    let raw = fs::read_to_string(claim).map_err(|e| e.to_string())?;
    let claim = toml::from_str::<Release>(&raw).map_err(|e| e.to_string())?;
    let live = github.repository(Repo::Registered(entry.project.repository))?;
    if live.id != entry.project.repository || live.private {
        return Err("registered repository is no longer public or has the wrong ID".into());
    }
    let releases = github.releases(Repo::Registered(live.id))?;
    let approved = latest_approved(entry);
    let candidate = candidates(entry, &releases, approved.as_ref())
        .into_iter()
        .find(|c| c.version == claim.version && c.release.tag_name == claim.tag.as_str())
        .ok_or(
            "the proposed release does not match a new, published tag of this registered package",
        )?;
    let workflow = workflow(&registry)?;
    let request = Request {
        github,
        entry,
        repository_id: live.id,
        repository: &live.name,
        workflow,
        release: candidate.release,
        version: &candidate.version,
    };
    fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let work = tempfile::tempdir().map_err(|e| e.to_string())?;
    let assets =
        release_assets(&request, work.path(), Some(&claim)).map_err(|error| error.to_string())?;
    let (row, payload) = prepared(
        &request,
        candidate.published,
        assets,
        work.path(),
        &categories(&registry)?,
        &mut Builds::new(github)?,
        Some(&claim),
    )?;
    if row != claim {
        return Err("release row does not match the attested assets and published metadata".into());
    }
    if let Some(missing) = unsupplied(&registry, id, &row) {
        return Err(missing);
    }
    write_release(
        &row,
        &payload,
        &out.join("expected").join(project_path(id)),
        kind_change(checkout, entry, &row)?.as_deref(),
    )?;
    release_payload::source(
        &work.path().join("package.smod"),
        &out.join("source"),
        payload.package.is_native(),
    )?;
    let Some(previous) = approved.map(|approved| approved.row) else {
        return Ok(());
    };
    // The last approved release is where it was signed, which a project that moved since no
    // longer names.
    let moved;
    let (home, releases) = if previous.repository == live.id {
        (&live, &releases)
    } else {
        let home = github.repository(Repo::Registered(previous.repository))?;
        if home.id != previous.repository || home.private {
            return Err(
                "the last approved release's repository is no longer public or has the wrong ID"
                    .into(),
            );
        }
        let releases = github.releases(Repo::Registered(home.id))?;
        moved = (home, releases);
        (&moved.0, &moved.1)
    };
    let Some(release) = downloadable(entry, previous, releases) else {
        // Nothing is left to compare against. The new release is verified in full above,
        // so it still goes to review, and the report says the comparison is missing.
        return fs::write(out.join("previous-unavailable"), "").map_err(|e| e.to_string());
    };
    let request = Request {
        repository_id: home.id,
        repository: &home.name,
        release,
        version: &previous.version,
        ..request
    };
    let previous_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let commit = request.tag_commit().map_err(|error| error.to_string())?;
    let (file, workflow) = asset(
        &request,
        &commit,
        "smod",
        "package.smod",
        previous_dir.path(),
        Some(Claim::Approved(&previous.file)),
    )
    .map_err(|error| error.to_string())?;
    if file != previous.file || workflow != previous.workflow {
        return Err(
            "last approved package no longer matches its registry row and attestation".into(),
        );
    }
    let smod = previous_dir.path().join("package.smod");
    let package = Package::read(&smod)?;
    release_payload::source(&smod, &out.join("previous"), package.is_native())
}

/// The approved release as the author's repository still publishes it: its tag, package
/// and attestation. `None` once the author took any of them down.
fn downloadable<'a>(
    entry: &ProjectEntry,
    approved: &Release,
    releases: &'a [github::Release],
) -> Option<&'a github::Release> {
    let package = format!(
        "{}-{}.smod",
        entry.project.mod_id.as_str(),
        approved.version
    );
    let bundle = format!("{package}.sigstore.json");
    releases.iter().find(|release| {
        release.tag_name == approved.tag.as_str()
            && release.published().is_some()
            && release.asset(&package).is_ok()
            && release.asset(&bundle).is_ok()
    })
}

fn project_path(id: &ProjectId) -> std::path::PathBuf {
    Path::new("registry/projects").join(id.as_str())
}

fn load(checkout: &Path) -> Result<Registry, String> {
    registry::load(checkout).map_err(|errors| errors.join("; "))
}

fn workflow(registry: &Registry) -> Result<&str, String> {
    registry
        .release_workflow
        .as_str()
        .split_once('@')
        .map(|(path, _)| path)
        .ok_or_else(|| "missing shared release workflow".into())
}

fn categories(registry: &Registry) -> Result<page::Categories, String> {
    let mut values = toml::Table::new();
    values.insert(
        "category".into(),
        toml::Value::try_from(&registry.categories).map_err(|e| e.to_string())?,
    );
    page::Categories::parse(&toml::to_string(&values).map_err(|e| e.to_string())?)
}

/// The project's `project.toml` with `row`'s kind, when `row` is the first release of another kind
/// than the project's. Every release of a project has its kind, so the pull request that takes
/// this file also removes the project's earlier releases.
fn kind_change(
    checkout: &Path,
    entry: &ProjectEntry,
    row: &Release,
) -> Result<Option<String>, String> {
    let project = &entry.project;
    if row.kind == project.kind {
        return Ok(None);
    }
    let name = |kind| {
        toml::Value::try_from(kind)
            .map(|value: toml::Value| value.to_string())
            .map_err(|e| e.to_string())
    };
    let path = checkout
        .join(project_path(&project.id))
        .join("project.toml");
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let old = format!("kind = {}\n", name(project.kind)?);
    if text.matches(&old).count() != 1 {
        return Err("project.toml does not state its kind on one line of its own".into());
    }
    Ok(Some(
        text.replace(&old, &format!("kind = {}\n", name(row.kind)?)),
    ))
}

fn write_release(
    row: &Release,
    payload: &Payload,
    project: &Path,
    kind_change: Option<&str>,
) -> Result<(), String> {
    let releases = project.join("releases");
    fs::create_dir_all(&releases).map_err(|e| e.to_string())?;
    if let Some(text) = kind_change {
        fs::write(project.join("project.toml"), text).map_err(|e| e.to_string())?;
    }
    let text = toml::to_string_pretty(row).map_err(|e| e.to_string())?;
    fs::write(releases.join(format!("{}.toml", row.version)), text).map_err(|e| e.to_string())?;
    payload.write_page(&project.join("page"))
}

struct Request<'a> {
    github: &'a dyn GitHub,
    entry: &'a ProjectEntry,
    /// The repository the release is signed from: the project's for a proposed release, the
    /// row's own for an approved one, which a project that moved since no longer names.
    repository_id: RepositoryId,
    repository: &'a RepositoryName,
    workflow: &'a str,
    release: &'a github::Release,
    version: &'a Version,
}

impl Request<'_> {
    fn tag_commit(&self) -> Result<String, Observation> {
        self.github
            .tag_commit(Repo::Registered(self.repository_id), &self.release.tag_name)
            .map_err(Observation::Unknown)
    }
}

struct Assets {
    package: Asset,
    page: Asset,
    workflow: SignerWorkflowRef,
}

#[derive(Clone, Copy)]
enum Claim<'a> {
    Proposed(&'a Asset),
    Approved(&'a Asset),
}

fn release_assets(
    request: &Request<'_>,
    dir: &Path,
    claim: Option<&Release>,
) -> Result<Assets, Observation> {
    // Both artifacts must come from the same snapshot even if the tag moves mid-check.
    let commit = request.tag_commit()?;
    let (package, package_ref) = asset(
        request,
        &commit,
        "smod",
        "package.smod",
        dir,
        claim.map(|r| Claim::Proposed(&r.file)),
    )?;
    let (page, page_ref) = asset(
        request,
        &commit,
        "page.tar",
        "page.tar",
        dir,
        claim.map(|r| Claim::Proposed(&r.page)),
    )?;
    if package_ref != page_ref {
        return Err(Observation::Ineligible(
            "package and page attestations name different signer workflow refs".into(),
        ));
    }
    Ok(Assets {
        package,
        page,
        workflow: package_ref,
    })
}

fn download_asset(
    github: &dyn GitHub,
    url: &str,
    path: &Path,
    live: &github::Asset,
) -> Result<String, Observation> {
    // The existing registry package download cap (sunrise_mod_package::index::MAX_FILE).
    github
        .download_release(url, path, live.size.min(MAX_FILE))
        .map_err(Observation::Unknown)?;
    if fs::metadata(path)
        .map_err(|e| Observation::Unknown(e.to_string()))?
        .len()
        != live.size
    {
        return Err(Observation::Ineligible(
            "download size differs from the GitHub release asset".into(),
        ));
    }
    let hash = sha256_file(path).map_err(Observation::Unknown)?;
    if live
        .digest
        .as_ref()
        .is_some_and(|digest| digest != &format!("sha256:{hash}"))
    {
        return Err(Observation::Ineligible(
            "download hash differs from the GitHub release asset".into(),
        ));
    }
    Ok(hash)
}

fn asset(
    request: &Request<'_>,
    commit: &str,
    suffix: &str,
    local: &str,
    dir: &Path,
    claim: Option<Claim<'_>>,
) -> Result<(Asset, SignerWorkflowRef), Observation> {
    let tag = &request.release.tag_name;
    let repo = request.repository_id;
    let name = format!(
        "{}-{}.{suffix}",
        request.entry.project.mod_id.as_str(),
        request.version
    );
    let bundle_name = format!("{name}.sigstore.json");
    let file = request
        .release
        .asset(&name)
        .map_err(Observation::Ineligible)?;
    let bundle = request
        .release
        .asset(&bundle_name)
        .map_err(Observation::Ineligible)?;
    let url = file.browser_download_url.as_str();
    let bundle_url = bundle.browser_download_url.as_str();
    for (url, expected) in [(url, &name), (bundle_url, &bundle_name)] {
        if !registry::release_download_repository(url, tag, expected)
            .is_some_and(|owner| owner.eq_ignore_ascii_case(request.repository.as_str()))
        {
            return Err(Observation::Ineligible("GitHub release asset or bundle URL does not name the registered repository and release".into()));
        }
    }
    if let Some(Claim::Proposed(claim)) = claim
        && (claim.url.as_str() != url || claim.attestation.as_str() != bundle_url)
    {
        return Err(Observation::Ineligible("proposed asset or bundle URL does not match the live release of the registered repository".into()));
    }
    let path = dir.join(local);
    let attestation = dir.join(format!("{local}.sigstore.json"));
    let sha256 = download_asset(request.github, url, &path, file)?;
    let bundle_sha256 = download_asset(request.github, bundle_url, &attestation, bundle)?;
    let mut old_name = None;
    let (stored_url, stored_bundle) = match claim {
        Some(Claim::Proposed(claim) | Claim::Approved(claim)) => {
            if claim.size != file.size || claim.sha256.as_str() != sha256 {
                return Err(Observation::Ineligible(
                    "download size or hash differs from the approved/proposed release row".into(),
                ));
            }
            for (old, current, expected, live, hash) in [
                (claim.url.as_str(), url, &name, file, &sha256),
                (
                    claim.attestation.as_str(),
                    bundle_url,
                    &bundle_name,
                    bundle,
                    &bundle_sha256,
                ),
            ] {
                if old == current {
                    continue;
                }
                let old_repo = registry::release_download_repository(old, tag, expected)
                    .ok_or_else(|| {
                        Observation::Ineligible(
                            "approved URL is not a GitHub download for this tag and asset".into(),
                        )
                    })?;
                let old_repo = RepositoryName::parse(old_repo.into())
                    .map_err(|reason| Observation::Ineligible(reason.into()))?;
                let resolved = request
                    .github
                    .repository(Repo::Named(&old_repo))
                    .map_err(Observation::Unknown)?;
                if resolved.id != repo || resolved.private {
                    return Err(Observation::Ineligible(
                        "approved URL no longer resolves to the registered repository".into(),
                    ));
                }
                // Confirm the actual browser URL still works after the rename. A matching
                // repository and tag alone do not prove GitHub still serves that URL.
                if download_asset(request.github, old, &dir.join("approved-alias"), live)? != *hash
                {
                    return Err(Observation::Ineligible(
                        "approved URL does not serve the same release asset".into(),
                    ));
                }
                if expected == &name {
                    old_name = Some(old_repo);
                }
            }
            (claim.url.as_str(), claim.attestation.as_str())
        }
        None => (url, bundle_url),
    };
    let verify = |repository_name| {
        request.github.verify_attestation(
            Attestation {
                repository_id: repo,
                repository_name,
                workflow_path: request.workflow,
                tag,
                commit,
            },
            &path,
            &attestation,
        )
    };
    let workflow = verify(request.repository).or_else(|error| match &old_name {
        Some(old) => verify(old),
        None => Err(error),
    })?;
    Ok((
        Asset {
            url: HttpsUrl::parse(stored_url.into())
                .map_err(|reason| Observation::Ineligible(reason.into()))?,
            size: file.size,
            sha256: Sha256::parse(sha256)
                .map_err(|reason| Observation::Ineligible(reason.into()))?,
            attestation: HttpsUrl::parse(stored_bundle.into())
                .map_err(|reason| Observation::Ineligible(reason.into()))?,
        },
        workflow,
    ))
}

fn prepared(
    request: &Request<'_>,
    published: OffsetDateTime,
    assets: Assets,
    dir: &Path,
    categories: &page::Categories,
    builds: &mut Builds<'_>,
    claim: Option<&Release>,
) -> Result<(Release, Payload), String> {
    let payload = release_payload::read(
        &dir.join("package.smod"),
        &dir.join("page.tar"),
        &request.entry.project,
        request.version,
        categories,
    )?;
    let sunrise = builds.resolve(
        payload.package.manifest.native.as_ref(),
        claim.map(|r| &r.sunrise),
    )?;
    let row = Release {
        version: request.version.clone(),
        kind: if payload.package.is_native() {
            schema::Kind::NativeMod
        } else {
            schema::Kind::Mod
        },
        tag: Text::parse(request.release.tag_name.clone())?,
        published: PublishedAt::of(published)?,
        sunrise,
        workflow: assets.workflow,
        // Both attestations were verified against this ID (`asset`).
        repository: request.repository_id,
        file: assets.package,
        page: assets.page,
        dependencies: payload.dependencies.clone(),
        jars: payload.package.manifest.jar.clone(),
    };
    Ok((row, payload))
}

pub fn diagnostic(text: &str) -> String {
    text.escape_default()
        .to_string()
        .replace("::", "%3A%3A")
        .replace("##[", "%23%23%5B")
}

fn note(summary: &mut String, label: &str, reason: &str) {
    let literal = |s: &str| {
        diagnostic(s)
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('@', "&#64;")
            .replace('[', "&#91;")
            .replace(']', "&#93;")
            .replace('`', "&#96;")
    };
    summary.push_str(&format!(
        "- <code>{}</code>: <code>{}</code>\n",
        literal(label),
        literal(reason)
    ));
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
    };

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/two-ports");
    const RELEASES: &str = include_str!("../tests/fixtures/watcher/releases.json");

    #[test]
    fn registration_prioritizes_newest_publication_with_the_exact_tag_shape() {
        let mut registry = load(Path::new(FIXTURE)).unwrap();
        let releases: Vec<github::Release> = serde_json::from_str(RELEASES).unwrap();
        let root = ProjectId::parse("Ab3dE9xQ".into()).unwrap();
        let folder = ProjectId::parse("Zz91kLmN".into()).unwrap();
        let root = registry.projects.get_mut(&root).unwrap();
        root.releases.clear();
        let initial = candidates(root, &releases, None);
        assert_eq!(initial.first().unwrap().release.tag_name, "v1.1.0");
        assert_eq!(initial.len(), 3);
        let folder = registry.projects.get_mut(&folder).unwrap();
        folder.releases.clear();
        let initial = candidates(folder, &releases, None);
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].release.tag_name, "wayfinder/v2.0.0-beta.1");
    }

    #[test]
    fn a_required_dependency_needs_another_projects_unrevoked_release_in_range() {
        let mut registry = load(Path::new(FIXTURE)).unwrap();
        let dependent = ProjectId::parse("Zz91kLmN".into()).unwrap();
        let library = ProjectId::parse("Ab3dE9xQ".into()).unwrap();
        let mut row = registry
            .projects
            .get_mut(&dependent)
            .unwrap()
            .releases
            .pop_first()
            .unwrap()
            .1;
        // Optional: nothing has to supply it.
        row.dependencies[0].version = "=9.9.9".parse().unwrap();
        assert_eq!(unsupplied(&registry, &dependent, &row), None);
        row.dependencies[0].required = true;
        let refusal = "needs wayfinder =9.9.9, which has no such release in the registry: release the library first";
        assert_eq!(
            unsupplied(&registry, &dependent, &row).as_deref(),
            Some(refusal)
        );
        row.dependencies[0].version = "^1.2".parse().unwrap();
        assert_eq!(unsupplied(&registry, &dependent, &row), None);
        // The dependent shares the in-game ID but cannot supply itself.
        assert!(unsupplied(&registry, &library, &row).is_some());
        // A pin to another provider, or a takedown of the only matching release, leaves nothing.
        row.dependencies[0].project = Some(dependent.clone());
        assert!(unsupplied(&registry, &dependent, &row).is_some());
        row.dependencies[0].project = None;
        let hash = registry.projects[&library]
            .releases
            .values()
            .next()
            .unwrap()
            .file
            .sha256
            .clone();
        registry.revoked[1].sha256 = Some(hash);
        assert!(unsupplied(&registry, &dependent, &row).is_some());
    }

    #[test]
    fn latest_approval_is_the_watermark_and_open_pr_blocks_its_project() {
        let registry = load(Path::new(FIXTURE)).unwrap();
        let releases = serde_json::from_str::<Vec<github::Release>>(RELEASES).unwrap();
        let id = ProjectId::parse("Ab3dE9xQ".into()).unwrap();
        let entry = &registry.projects[&id];
        let approved = latest_approved(entry);
        let remaining = candidates(entry, &releases, approved.as_ref());
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].release.tag_name, "v1.1.0");
        assert!(has_open_release(
            &BTreeSet::from(["release/Ab3dE9xQ/9.9.9".into()]),
            &id
        ));
        assert!(!has_open_release(
            &BTreeSet::from(["release/Zz91kLmN/9.9.9".into()]),
            &id
        ));
    }

    #[test]
    fn approval_order_follows_the_rows_publication_times_even_after_a_takedown() {
        let id = ProjectId::parse("Ab3dE9xQ".into()).unwrap();
        let mut registry = load(Path::new(FIXTURE)).unwrap();
        let entry = registry.projects.get_mut(&id).unwrap();
        // The fixture approved 1.2.0, published at 12:00. 1.1.0 came an hour later.
        let mut later: Release =
            toml::from_str(&toml::to_string(entry.releases.values().next().unwrap()).unwrap())
                .unwrap();
        later.version = Version::new(1, 1, 0);
        later.tag = Text::parse("v1.1.0".into()).unwrap();
        later.published = PublishedAt::parse("2026-11-23T13:00:00Z".into()).unwrap();
        entry.releases.insert(later.version.clone(), later);
        // The author has since deleted both approved releases from GitHub.
        let releases: Vec<github::Release> = serde_json::from_value(serde_json::json!([
            {"tag_name":"v1.1.5","published_at":"2026-11-23T12:30:00Z","draft":false,"assets":[]},
            {"tag_name":"v1.0.0","published_at":"2026-11-23T13:30:00Z","draft":false,"assets":[]}
        ]))
        .unwrap();
        let previous = latest_approved(entry).unwrap();
        assert_eq!(previous.row.tag.as_str(), "v1.1.0");
        let eligible = candidates(entry, &releases, Some(&previous));
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].release.tag_name, "v1.0.0");
    }

    #[test]
    fn the_previous_package_is_compared_only_while_its_tag_package_and_attestation_are_up() {
        let registry = load(Path::new(FIXTURE)).unwrap();
        let entry = &registry.projects[&ProjectId::parse("Ab3dE9xQ".into()).unwrap()];
        let approved = entry.releases.values().next().unwrap();
        let up = || live_release("1.2.0", "2026-11-23T12:00:00Z", None);
        assert!(downloadable(entry, approved, &[up()]).is_some());
        assert!(downloadable(entry, approved, &[]).is_none());
        let other = live_release("1.3.0", "2026-11-24T12:00:00Z", None);
        assert!(downloadable(entry, approved, &[other]).is_none());
        let mut draft = up();
        draft.draft = true;
        assert!(downloadable(entry, approved, &[draft]).is_none());
        for gone in ["wayfinder-1.2.0.smod", "wayfinder-1.2.0.smod.sigstore.json"] {
            let mut release = up();
            release.assets.retain(|asset| asset.name != gone);
            assert!(
                downloadable(entry, approved, &[release]).is_none(),
                "{gone}"
            );
        }
    }

    const CURRENT: &str = "Example/Wayfinder-Renamed";
    const OLD: &str = "example/wayfinder";
    const BODY: &[u8] = b"release bytes";
    const BUNDLE: &[u8] = b"attestation bytes";

    #[derive(Default)]
    struct Recorded {
        downloads: RefCell<Vec<String>>,
        tag_lookups: Cell<usize>,
        verified_commits: RefCell<Vec<String>>,
        inconclusive: bool,
        alias_reassigned: bool,
        alias_broken: bool,
    }

    impl GitHub for Recorded {
        fn repository(&self, repo: Repo<'_>) -> Result<github::Repository, String> {
            let (id, name) = match repo {
                Repo::Registered(id) if id.get() == 123456789 => (id.get(), CURRENT),
                Repo::Registered(id) if id.get() == 987654321 => {
                    (id.get(), "other/port-collection")
                }
                Repo::Named(name)
                    if name.as_str().eq_ignore_ascii_case(OLD) && !self.alias_reassigned =>
                {
                    (123456789, CURRENT)
                }
                Repo::Named(name) if name.as_str().eq_ignore_ascii_case(OLD) => (555, OLD),
                _ => return Err("unexpected repository lookup".into()),
            };
            serde_json::from_value(serde_json::json!({
                "id": id, "full_name": name, "private": false,
                "owner": {"id": 1, "login": name.split_once('/').unwrap().0},
            }))
            .map_err(|e| e.to_string())
        }

        fn releases(&self, repo: Repo<'_>) -> Result<Vec<github::Release>, String> {
            match repo {
                Repo::Registered(id) if id.get() == 123456789 && self.inconclusive => Ok(vec![
                    live_release("1.2.0", "2026-11-23T13:00:00Z", None),
                    live_release("1.1.0", "2026-11-23T12:00:00Z", None),
                ]),
                Repo::Registered(_) => Ok(Vec::new()),
                Repo::Named(_) => Err("registered releases must be fetched by numeric ID".into()),
            }
        }

        fn release_branches(&self) -> Result<github::ReleaseBranches, String> {
            Ok(github::ReleaseBranches::default())
        }

        fn tag_commit(&self, repo: Repo<'_>, tag: &str) -> Result<String, String> {
            assert!(matches!(repo, Repo::Registered(id) if id.get() == 123456789));
            assert_eq!(tag, "v1.2.0");
            let count = self.tag_lookups.get();
            self.tag_lookups.set(count + 1);
            Ok(if count == 0 { "a" } else { "b" }.repeat(40))
        }

        fn download_release(&self, url: &str, dest: &Path, _: u64) -> Result<(), String> {
            self.downloads.borrow_mut().push(url.into());
            if self.inconclusive || (self.alias_broken && url.contains(OLD)) {
                return Err("asset temporarily unavailable".into());
            }
            let bytes = if url.ends_with(".sigstore.json") {
                BUNDLE
            } else {
                BODY
            };
            fs::write(dest, bytes).map_err(|e| e.to_string())
        }

        fn verify_attestation(
            &self,
            check: Attestation<'_>,
            file: &Path,
            bundle: &Path,
        ) -> Result<SignerWorkflowRef, Observation> {
            assert_eq!(check.repository_id.get(), 123456789);
            assert_eq!(fs::read(file).unwrap().as_slice(), BODY);
            assert_eq!(fs::read(bundle).unwrap().as_slice(), BUNDLE);
            self.verified_commits.borrow_mut().push(check.commit.into());
            SignerWorkflowRef::parse(format!("{}@refs/tags/release-v1", schema::RELEASE_WORKFLOW))
                .map_err(|reason| Observation::Ineligible(reason.into()))
        }
    }

    fn live_release(version: &str, published: &str, digest: Option<String>) -> github::Release {
        let tag = format!("v{version}");
        let assets = [("smod", digest), ("page.tar", None)]
            .into_iter()
            .flat_map(|(suffix, digest)| {
                let name = format!("wayfinder-{version}.{suffix}");
                let url = format!("https://github.com/{CURRENT}/releases/download/{tag}/{name}");
                let bundle_name = format!("{name}.sigstore.json");
                let bundle_url = format!("{url}.sigstore.json");
                [
                    github::Asset {
                        name,
                        size: BODY.len() as u64,
                        digest,
                        browser_download_url: url,
                    },
                    github::Asset {
                        name: bundle_name,
                        size: BUNDLE.len() as u64,
                        digest: None,
                        browser_download_url: bundle_url,
                    },
                ]
            })
            .collect();
        github::Release {
            tag_name: tag,
            name: None,
            published_at: Some(published.into()),
            draft: false,
            assets,
        }
    }

    #[test]
    fn both_attestations_use_the_same_tag_commit_even_if_a_second_lookup_would_change_it() {
        let registry = load(Path::new(FIXTURE)).unwrap();
        let entry = &registry.projects[&ProjectId::parse("Ab3dE9xQ".into()).unwrap()];
        let github = Recorded::default();
        let work = tempfile::tempdir().unwrap();
        let release = live_release("1.2.0", "2026-11-23T13:00:00Z", None);
        let request = Request {
            github: &github,
            entry,
            repository_id: entry.project.repository,
            repository: &RepositoryName::parse(CURRENT.into()).unwrap(),
            workflow: schema::RELEASE_WORKFLOW,
            release: &release,
            version: &Version::new(1, 2, 0),
        };
        release_assets(&request, work.path(), None).unwrap();
        assert_eq!(github.tag_lookups.get(), 1);
        assert_eq!(
            github.verified_commits.borrow().as_slice(),
            ["a".repeat(40), "a".repeat(40)]
        );
    }

    #[test]
    fn a_release_is_verified_against_the_repository_it_was_signed_from_after_the_project_moved() {
        let mut registry = load(Path::new(FIXTURE)).unwrap();
        let entry = registry
            .projects
            .get_mut(&ProjectId::parse("Ab3dE9xQ".into()).unwrap())
            .unwrap();
        let signed_from = entry.project.repository;
        // The fake answers, and verifies attestations, only for the repository that signed.
        entry.project.repository = serde_json::from_str("555").unwrap();
        let work = tempfile::tempdir().unwrap();
        let release = live_release("1.2.0", "2026-11-23T13:00:00Z", None);
        let request = Request {
            github: &Recorded::default(),
            entry,
            repository_id: signed_from,
            repository: &RepositoryName::parse(CURRENT.into()).unwrap(),
            workflow: schema::RELEASE_WORKFLOW,
            release: &release,
            version: &Version::new(1, 2, 0),
        };
        release_assets(&request, work.path(), None).unwrap();
    }

    #[test]
    fn foreign_claim_urls_are_refused_with_or_without_github_digests() {
        let registry = load(Path::new(FIXTURE)).unwrap();
        let entry = &registry.projects[&ProjectId::parse("Ab3dE9xQ".into()).unwrap()];
        let github = Recorded::default();
        let work = tempfile::tempdir().unwrap();
        fs::write(work.path().join("original"), BODY).unwrap();
        let hash = sha256_file(&work.path().join("original")).unwrap();
        let repo = RepositoryName::parse(CURRENT.into()).unwrap();
        let version = Version::new(1, 2, 0);
        for digest in [None, Some(format!("sha256:{hash}"))] {
            let release = live_release("1.2.0", "2026-11-23T13:00:00Z", digest);
            let request = Request {
                github: &github,
                entry,
                repository_id: entry.project.repository,
                repository: &repo,
                workflow: schema::RELEASE_WORKFLOW,
                release: &release,
                version: &version,
            };
            let commit = request.tag_commit().unwrap();
            for index in [0, 1] {
                let url = |i: usize| {
                    HttpsUrl::parse(if index == i {
                        release.assets[i]
                            .browser_download_url
                            .replace(CURRENT, "unrelated/mirror")
                    } else {
                        release.assets[i].browser_download_url.clone()
                    })
                    .unwrap()
                };
                let claim = Asset {
                    url: url(0),
                    attestation: url(1),
                    size: BODY.len() as u64,
                    sha256: Sha256::parse(hash.clone()).unwrap(),
                };
                assert!(
                    matches!(asset(&request, &commit, "smod", "test.smod", work.path(), Some(Claim::Proposed(&claim))), Err(Observation::Ineligible(reason)) if reason.contains("does not match the live release"))
                );
            }
            let claim = Asset {
                url: HttpsUrl::parse(release.assets[0].browser_download_url.clone()).unwrap(),
                attestation: HttpsUrl::parse(release.assets[1].browser_download_url.clone())
                    .unwrap(),
                size: BODY.len() as u64,
                sha256: Sha256::parse("a".repeat(64)).unwrap(),
            };
            assert!(
                matches!(asset(&request, &commit, "smod", "test.smod", work.path(), Some(Claim::Proposed(&claim))), Err(Observation::Ineligible(reason)) if reason.contains("hash differs from the approved/proposed release row"))
            );
        }
        assert!(
            github
                .downloads
                .borrow()
                .iter()
                .all(|url| !url.contains("unrelated"))
        );
    }

    #[test]
    fn renamed_previous_asset_keeps_the_stored_hash_only_for_a_working_alias_of_the_same_repository()
     {
        let registry = load(Path::new(FIXTURE)).unwrap();
        let entry = &registry.projects[&ProjectId::parse("Ab3dE9xQ".into()).unwrap()];
        let work = tempfile::tempdir().unwrap();
        fs::write(work.path().join("original"), BODY).unwrap();
        let hash = sha256_file(&work.path().join("original")).unwrap();
        let release = live_release("1.2.0", "2026-11-23T13:00:00Z", None);
        let repo = RepositoryName::parse(CURRENT.into()).unwrap();
        let version = Version::new(1, 2, 0);
        let claim = Asset {
            url: HttpsUrl::parse(release.assets[0].browser_download_url.replace(CURRENT, OLD))
                .unwrap(),
            attestation: HttpsUrl::parse(
                release.assets[1].browser_download_url.replace(CURRENT, OLD),
            )
            .unwrap(),
            size: BODY.len() as u64,
            sha256: Sha256::parse(hash).unwrap(),
        };
        for (alias_reassigned, alias_broken) in [(false, false), (true, false), (false, true)] {
            let github = Recorded {
                alias_reassigned,
                alias_broken,
                ..Recorded::default()
            };
            let request = Request {
                github: &github,
                entry,
                repository_id: entry.project.repository,
                repository: &repo,
                workflow: schema::RELEASE_WORKFLOW,
                release: &release,
                version: &version,
            };
            let commit = request.tag_commit().unwrap();
            let result = asset(
                &request,
                &commit,
                "smod",
                "test.smod",
                work.path(),
                Some(Claim::Approved(&claim)),
            );
            if alias_reassigned || alias_broken {
                assert!(result.is_err());
            } else {
                let (file, _) = result.unwrap();
                assert!(file == claim);
                assert_eq!(github.downloads.borrow().len(), 4);
            }
        }
    }

    #[test]
    fn registration_logs_an_inconclusive_first_observation_without_downloading_older_assets() {
        let root = tempfile::tempdir().unwrap();
        let ids = ["Ab3dE9xQ", "Zz91kLmN"];
        let base = Path::new(FIXTURE).join("registry");
        let fixture = root.path().join("registry");
        for id in ids {
            let project = Path::new("projects").join(id);
            fs::create_dir_all(fixture.join(&project)).unwrap();
            fs::copy(
                base.join(&project).join("project.toml"),
                fixture.join(&project).join("project.toml"),
            )
            .unwrap();
        }
        fs::create_dir(fixture.join("repositories")).unwrap();
        for file in [
            "categories.toml",
            "repositories/123456789.toml",
            "repositories/987654321.toml",
            "revoked.toml",
            "release-workflow.toml",
        ] {
            fs::copy(base.join(file), fixture.join(file)).unwrap();
        }
        let out = tempfile::tempdir().unwrap();
        let github = Recorded {
            inconclusive: true,
            ..Recorded::default()
        };
        run(root.path(), out.path(), &github).unwrap();
        let summary = fs::read_to_string(out.path().join("watch.md")).unwrap();
        assert!(summary.contains("release verification is unknown"));
        assert!(!out.path().join("releases").exists());
        let downloads = github.downloads.borrow();
        assert_eq!(downloads.len(), 1);
        assert!(downloads[0].contains("/v1.2.0/"));
    }

    #[test]
    fn a_paused_project_gets_no_release_proposed() {
        let root = tempfile::tempdir().unwrap();
        let base = Path::new(FIXTURE).join("registry");
        let fixture = root.path().join("registry");
        for id in ["Ab3dE9xQ", "Zz91kLmN"] {
            let project = Path::new("projects").join(id);
            fs::create_dir_all(fixture.join(&project)).unwrap();
            let text = fs::read_to_string(base.join(&project).join("project.toml")).unwrap();
            fs::write(
                fixture.join(&project).join("project.toml"),
                format!("{text}paused = true\n"),
            )
            .unwrap();
        }
        fs::create_dir(fixture.join("repositories")).unwrap();
        for file in [
            "categories.toml",
            "repositories/123456789.toml",
            "repositories/987654321.toml",
            "revoked.toml",
            "release-workflow.toml",
        ] {
            fs::copy(base.join(file), fixture.join(file)).unwrap();
        }
        let out = tempfile::tempdir().unwrap();
        let github = Recorded::default();
        run(root.path(), out.path(), &github).unwrap();
        assert!(github.downloads.borrow().is_empty());
        assert!(!out.path().join("releases").exists());
    }

    #[test]
    fn renames_use_numeric_identity_and_rewrite_only_that_repository_file() {
        let out = tempfile::tempdir().unwrap();
        run(Path::new(FIXTURE), out.path(), &Recorded::default()).unwrap();
        let renamed = out.path().join("rename/registry/repositories");
        let text = fs::read_to_string(renamed.join("123456789.toml")).unwrap();
        let row: schema::Repository = toml::from_str(&text).unwrap();
        assert_eq!(row.name.as_str(), "Example/Wayfinder-Renamed");
        assert_eq!(row.package.len(), 1);
        assert_eq!(fs::read_dir(renamed).unwrap().count(), 1);
        assert!(!out.path().join("releases").exists());
    }

    #[test]
    fn writes_only_the_release_row_and_resolved_page() {
        let id = ProjectId::parse("Ab3dE9xQ".into()).unwrap();
        let fixture = Path::new(FIXTURE).join(project_path(&id));
        let mut row: Release =
            toml::from_str(&fs::read_to_string(fixture.join("releases/1.2.0.toml")).unwrap())
                .unwrap();
        assert!(
            !toml::to_string_pretty(&row).unwrap().contains("jars"),
            "a release without original jars writes no key"
        );
        row.jars =
            vec![schema::JarRef::new("original".into(), "3.1.2".into(), "ab".repeat(64)).unwrap()];
        let page: schema::Page =
            toml::from_str(&fs::read_to_string(fixture.join("page/page.toml")).unwrap()).unwrap();
        let icon = fs::read(fixture.join("page").join(page.icon.as_str())).unwrap();
        let images = BTreeMap::from([(page.icon.clone(), icon.clone())]);
        let payload = Payload {
            package: Package {
                manifest: sunrise_mod_package::Manifest::parse(
                    "id='wayfinder'\nname='Wayfinder'\nversion='1.2.0'",
                )
                .unwrap(),
                icon: None,
            },
            page,
            images,
            dependencies: Vec::new(),
        };
        let out = tempfile::tempdir().unwrap();
        write_release(&row, &payload, out.path(), None).unwrap();
        let written = fs::read_to_string(out.path().join("releases/1.2.0.toml")).unwrap();
        assert!(toml::from_str::<Release>(&written).unwrap() == row);
        assert_eq!(fs::read(out.path().join("page/icon.png")).unwrap(), icon);
        assert!(out.path().join("page/page.toml").is_file());
        assert!(!out.path().join("project.toml").exists());
    }

    #[test]
    fn the_first_release_of_another_kind_changes_the_projects_kind() {
        let registry = load(Path::new(FIXTURE)).unwrap();
        let id = ProjectId::parse("Ab3dE9xQ".into()).unwrap();
        let entry = &registry.projects[&id];
        let fixture = Path::new(FIXTURE).join(project_path(&id));
        let mut row: Release =
            toml::from_str(&fs::read_to_string(fixture.join("releases/1.2.0.toml")).unwrap())
                .unwrap();
        assert!(
            kind_change(Path::new(FIXTURE), entry, &row)
                .unwrap()
                .is_none(),
            "a release of the project's own kind changes nothing"
        );
        row.kind = schema::Kind::Mod;
        let changed = kind_change(Path::new(FIXTURE), entry, &row)
            .unwrap()
            .unwrap();
        let before = fs::read_to_string(fixture.join("project.toml")).unwrap();
        assert_eq!(
            changed,
            before.replace("kind = \"native-mod\"", "kind = \"mod\"")
        );
        assert_ne!(changed, before);
        assert_eq!(
            toml::from_str::<schema::Project>(&changed).unwrap().kind,
            schema::Kind::Mod
        );
    }

    #[test]
    fn summary_escapes_actions_commands_and_line_breaks() {
        let mut out = String::new();
        note(&mut out, "bad\n::error", "##[warning]<script>");
        assert_eq!(out.lines().count(), 1);
        for bad in ["::", "##[", "<script>"] {
            assert!(!out.contains(bad));
        }
    }
}
