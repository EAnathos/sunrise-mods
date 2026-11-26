//! First-release discovery and regeneration. Tags and names are hints; only a verified caller
//! at the tagged commit can claim a package path.
use std::collections::BTreeMap;

use serde::Deserialize;

use super::*;
use crate::schema::{CalendarDate, Login, Project, SignerWorkflowRef};

struct FirstCandidate<'a> {
    namespace: Option<ModId>,
    candidate: Candidate<'a>,
}

fn first_candidates<'a>(
    registry: &Registry,
    repository: RepositoryId,
    releases: &'a [github::Release],
) -> Vec<FirstCandidate<'a>> {
    let claimed = registry
        .projects
        .values()
        .filter(|entry| entry.project.repository == repository)
        .map(|entry| match entry.project.path {
            PackagePath::Root(_) => None,
            PackagePath::Folder(_) => Some(entry.project.mod_id.clone()),
        })
        .collect::<BTreeSet<_>>();
    let mut newest = releases
        .iter()
        .filter_map(|release| {
            let published = release.published()?;
            let (namespace, version) = tag_parts(&release.tag_name)?;
            let namespace = namespace
                .map(|id| ModId::parse(id.to_owned()))
                .transpose()
                .ok()?;
            // Tag names only narrow discovery. A newer tag that cannot authenticate its path
            // must not suppress an earlier tag, even in the same namespace.
            if claimed.contains(&namespace)
                || !release
                    .assets
                    .iter()
                    .any(|asset| asset.name.ends_with(".smod.sigstore.json"))
            {
                return None;
            }
            Some(FirstCandidate {
                namespace,
                candidate: Candidate {
                    release,
                    published,
                    version,
                },
            })
        })
        .collect::<Vec<_>>();
    newest.sort_by(|a, b| {
        (b.candidate.published, &b.candidate.version, &b.namespace).cmp(&(
            a.candidate.published,
            &a.candidate.version,
            &a.namespace,
        ))
    });
    newest
}

fn asset_mod_id(first: &FirstCandidate<'_>) -> Result<ModId, Observation> {
    let release = first.candidate.release;
    let suffix = format!("-{}.smod", first.candidate.version);
    let id = if let Some(id) = &first.namespace {
        id.clone()
    } else {
        let mut packages = release
            .assets
            .iter()
            .filter_map(|asset| asset.name.strip_suffix(&suffix));
        match (packages.next(), packages.next()) {
            (Some(id), None) => {
                ModId::parse(id.to_owned()).map_err(|e| Observation::Ineligible(e.into()))?
            }
            _ => {
                return Err(Observation::Ineligible(
                    "expected exactly one <mod>-<version>.smod asset for the root tag".into(),
                ));
            }
        }
    };
    let name = format!("{}{suffix}", id.as_str());
    release.asset(&name).map_err(Observation::Ineligible)?;
    release
        .asset(&format!("{name}.sigstore.json"))
        .map_err(Observation::Ineligible)?;
    Ok(id)
}

fn caller_path(
    bytes: &[u8],
    signer: &SignerWorkflowRef,
    namespace: Option<&ModId>,
) -> Result<PackagePath, Observation> {
    #[derive(Deserialize)]
    struct Workflow {
        jobs: BTreeMap<String, Job>,
    }
    #[derive(Deserialize)]
    struct Job {
        uses: Option<String>,
        #[serde(rename = "with", default)]
        inputs: BTreeMap<String, serde_json::Value>,
    }
    let refuse = |message: &str| Observation::Ineligible(message.to_owned());
    // Actions supports YAML anchors, not YAML merge keys. Keep the parser's default
    // duplicate-key rejection and expansion budget.
    let options = serde_saphyr::options! { merge_keys: serde_saphyr::MergeKeyPolicy::Error };
    let workflow: Workflow = serde_saphyr::from_slice_with_options(bytes, options)
        .map_err(|e| Observation::Ineligible(format!("invalid attested caller YAML: {e}")))?;
    let (workflow_ref, tag) = signer
        .as_str()
        .split_once("@refs/tags/")
        .ok_or_else(|| refuse("expected a verified shared signer tag"))?;
    let short = format!("{workflow_ref}@{tag}");
    let mut jobs = workflow.jobs.values().filter(|job| {
        job.uses
            .as_deref()
            .is_some_and(|uses| uses == short || uses == signer.as_str())
    });
    let (Some(job), None) = (jobs.next(), jobs.next()) else {
        return Err(refuse(
            "expected one caller job using the exact verified shared signer ref",
        ));
    };
    let literal = job
        .inputs
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.contains("${{"))
        .ok_or_else(|| refuse("caller with.path is not a literal string"))?;
    let path = PackagePath::parse(literal.to_owned()).map_err(refuse)?;
    if matches!(path, PackagePath::Root(_)) != namespace.is_none() {
        return Err(refuse(
            "caller with.path does not match the release tag namespace",
        ));
    }
    Ok(path)
}

/// What first-release discovery has settled so far in one pass. A tag stream is a namespace
/// (or the root); package paths are deduplicated across streams independently of it.
#[derive(Default)]
pub(super) struct Streams {
    settled: BTreeSet<Option<ModId>>,
    paths: BTreeSet<PackagePath>,
}

impl Streams {
    pub(super) fn settled(&self, namespace: Option<&ModId>) -> bool {
        self.settled.contains(&namespace.cloned())
    }
}

/// Called newest first within each stream. A stream stops at its newest candidate whose package
/// authenticates and whose caller proves a path, even when another stream already reserved that
/// path, and does so before the page, collisions, PR or payload are checked, so a later refusal
/// never admits an older release. A candidate that fails to authenticate or prove a path settles
/// nothing. Older candidates of a settled stream are not read. `Ok(None)` means nothing is left
/// to propose from this candidate.
pub(super) fn newest_path(
    streams: &mut Streams,
    request: &Request<'_>,
    namespace: Option<&ModId>,
    dir: &Path,
    claim: Option<&Release>,
) -> Result<Option<(PackagePath, SignedPackage)>, Observation> {
    if streams.settled(namespace) {
        return Ok(None);
    }
    let package = signed_package(request, dir, claim)?;
    let (caller, _) = package.attestation.caller.as_ref().ok_or_else(|| {
        Observation::Ineligible("first release must attest the caller workflow".into())
    })?;
    let bytes = request
        .github
        .caller_workflow(request.repository_id, caller)?;
    let path = caller_path(&bytes, &package.attestation.signer, namespace)?;
    streams.settled.insert(namespace.cloned());
    Ok(streams
        .paths
        .insert(path.clone())
        .then_some((path, package)))
}

fn first_request<'a>(
    live: &'a github::Repository,
    candidate: &'a Candidate<'_>,
    mod_id: &'a ModId,
    workflow: &'a str,
    github: &'a dyn GitHub,
) -> Request<'a> {
    Request {
        github,
        mod_id,
        registered_kind: None,
        repository_id: live.id,
        repository: &live.name,
        workflow,
        release: candidate.release,
        version: &candidate.version,
    }
}

pub(super) struct Watch<'a, 'gh> {
    pub registry: &'a Registry,
    pub branches: &'a github::ReleaseBranches,
    pub out: &'a Path,
    pub github: &'gh dyn GitHub,
    pub workflow: &'a str,
    pub categories: &'a page::Categories,
    pub builds: &'a mut Builds<'gh>,
}

impl Watch<'_, '_> {
    pub(super) fn observe(
        self,
        live: &github::Repository,
        releases: &[github::Release],
        summary: &mut String,
    ) {
        let mut streams = Streams::default();
        for first in first_candidates(self.registry, live.id, releases) {
            if streams.settled(first.namespace.as_ref()) {
                continue;
            }
            let candidate = &first.candidate;
            let label = &candidate.release.tag_name;
            let result = (|| {
                let mod_id = asset_mod_id(&first).map_err(|e| e.to_string())?;
                let request = first_request(live, candidate, &mod_id, self.workflow, self.github);
                let work = tempfile::tempdir().map_err(|e| e.to_string())?;
                let Some((path, package)) = newest_path(
                    &mut streams,
                    &request,
                    first.namespace.as_ref(),
                    work.path(),
                    None,
                )
                .map_err(|e| e.to_string())?
                else {
                    return Ok(false);
                };
                if self.registry.has_package_path(live.id, &path) {
                    return Ok(false);
                }
                let id = registry::first_project_id(live.id, &path)?;
                let branch = format!("release/{}/{}", id.as_str(), candidate.version);
                if has_open_release(&self.branches.waiting, &id)
                    || self.branches.handled.contains(&branch)
                {
                    return Ok(false);
                }
                let assets =
                    signed_page(&request, work.path(), None, package).map_err(|e| e.to_string())?;
                let (row, payload) = prepared(
                    &request,
                    candidate.published,
                    assets,
                    work.path(),
                    self.categories,
                    self.builds,
                    None,
                )?;
                if let Some(missing) = unsupplied(self.registry, &id, &row) {
                    return Err(missing);
                }
                let project = first_project(self.registry, &request, id, path, &row)?;
                release_payload::source(
                    &work.path().join("package.smod"),
                    &work.path().join("source"),
                    payload.package.is_native(),
                )?;
                let root = self.out.join("first-releases").join(project.id.as_str());
                fs::create_dir_all(&root).map_err(|e| e.to_string())?;
                let stage = tempfile::tempdir_in(&root).map_err(|e| e.to_string())?;
                write_first(&project, &row, &payload, stage.path())?;
                fs::rename(stage.path(), root.join(row.version.to_string()))
                    .map_err(|e| e.to_string())?;
                Ok(true)
            })();
            match result {
                Ok(true) => note(summary, label, "verified; ready to open a first-release PR"),
                Ok(false) => (),
                Err(reason) => note(summary, label, &reason),
            }
        }
    }
}

fn first_project(
    registry: &Registry,
    request: &Request<'_>,
    id: ProjectId,
    path: PackagePath,
    row: &Release,
) -> Result<Project, String> {
    let project = Project {
        slug: registry.first_slug(&id, request.mod_id)?,
        id,
        mod_id: request.mod_id.clone(),
        kind: row.kind,
        owners: owners(registry, request.repository_id, request.github)
            .map_err(|e| e.to_string())?,
        repository: request.repository_id,
        path,
        workflow: Text::parse(request.workflow.to_owned())?,
        registered: CalendarDate::parse(row.published.date().to_owned())?,
        paused: false,
    };
    registry.check_new_project(&project)?;
    Ok(project)
}

fn owners(
    registry: &Registry,
    repository: RepositoryId,
    github: &dyn GitHub,
) -> Result<Vec<Login>, Observation> {
    let Some(repo) = registry.repositories.get(&repository) else {
        return Err(Observation::Ineligible(
            "first-release repository is not registered".into(),
        ));
    };
    if repo.registrant.is_empty() {
        return Err(Observation::Ineligible(
            "first-release repository has no registrants".into(),
        ));
    }
    let mut logins = BTreeMap::new();
    for registrant in &repo.registrant {
        let account = github.account(registrant.id)?;
        if account.id != registrant.id {
            return Err(Observation::Ineligible(
                "GitHub returned the wrong account ID for a registrant".into(),
            ));
        }
        logins.insert(account.id, account.login);
    }
    Ok(logins.into_values().collect())
}

fn same_registration_authority(
    registry: &Registry,
    branch_row: &Path,
    id: RepositoryId,
) -> Result<(), String> {
    let current = registry
        .repositories
        .get(&id)
        .ok_or("first-release repository is not registered on main")?;
    if branch_row
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(RepositoryId::of_file)
        != Some(id)
    {
        return Err("first-release base row does not name the verified repository ID".into());
    }
    let observed: schema::Repository = toml::from_str(
        &fs::read_to_string(branch_row)
            .map_err(|e| format!("first-release base repository row: {e}"))?,
    )
    .map_err(|e| format!("first-release base repository row: {e}"))?;
    let ids = |row: &schema::Repository| {
        row.registrant
            .iter()
            .map(|account| account.id)
            .collect::<BTreeSet<_>>()
    };
    if current.registrant.is_empty() || ids(&observed) != ids(current) {
        return Err("repository registrant authority is missing or changed".into());
    }
    Ok(())
}

/// Only for a previously verified exact-head claim. Does not install it over main.
pub fn check_claim(
    main: &Path,
    branch_row: &Path,
    claim: &Path,
    github: &dyn GitHub,
) -> Result<(), String> {
    let registry = load(main)?;
    let project: Project = toml::from_str(&fs::read_to_string(claim).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    registry.check_first_project(&project)?;
    same_registration_authority(&registry, branch_row, project.repository)?;
    if project.owners != owners(&registry, project.repository, github).map_err(|e| e.to_string())? {
        return Err(
            "first-release owners no longer match the repository registrants' GitHub logins".into(),
        );
    }
    Ok(())
}

pub fn verify(
    main: &Path,
    branch_row: &Path,
    id: &ProjectId,
    claim: &Path,
    out: &Path,
    github: &dyn GitHub,
) -> Result<(), String> {
    let registry = load(main)?;
    let claim: Release = toml::from_str(&fs::read_to_string(claim).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    same_registration_authority(&registry, branch_row, claim.repository)?;
    let workflow = workflow(&registry)?;
    let live = github.repository(Repo::Registered(claim.repository))?;
    if live.id != claim.repository || live.private {
        return Err("registered repository is no longer public or has the wrong ID".into());
    }
    let releases = github.releases(Repo::Registered(live.id))?;
    let mut streams = Streams::default();
    let target_root = !claim.tag.as_str().contains('/');
    for first in first_candidates(&registry, live.id, &releases) {
        // Root and subfolder namespaces cannot share paths.
        if target_root != first.namespace.is_none() {
            continue;
        }
        let candidate = &first.candidate;
        let target =
            candidate.release.tag_name == claim.tag.as_str() && candidate.version == claim.version;
        let mod_id = match asset_mod_id(&first) {
            Ok(id) => id,
            Err(error) if target => return Err(error.to_string()),
            Err(_) => continue, // An unverifiable tag cannot reserve a package path.
        };
        let request = first_request(&live, candidate, &mod_id, workflow, github);
        let work = tempfile::tempdir().map_err(|e| e.to_string())?;
        let observed = newest_path(
            &mut streams,
            &request,
            first.namespace.as_ref(),
            work.path(),
            target.then_some(&claim),
        );
        if !target {
            continue;
        }
        let (path, package) = observed
            .map_err(|e| e.to_string())?
            .ok_or("release is not the newest publication for this package path")?;
        let key = registry::first_project_id(live.id, &path)?;
        if &key != id {
            return Err("first-release project ID does not match the release branch".into());
        }
        let assets =
            signed_page(&request, work.path(), Some(&claim), package).map_err(|e| e.to_string())?;
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
            return Err(
                "release row does not match the attested assets and published metadata".into(),
            );
        }
        let project = first_project(&registry, &request, key, path, &row)?;
        check_payload(&registry, id, &row, &payload, work.path())?;
        write_first(&project, &row, &payload, &out.join("expected"))?;
        return release_payload::source(
            &work.path().join("package.smod"),
            &out.join("source"),
            payload.package.is_native(),
        );
    }
    Err("the proposed first release is not the newest publication of a new package stream".into())
}

pub(super) fn write_first(
    project: &Project,
    row: &Release,
    payload: &Payload,
    out: &Path,
) -> Result<(), String> {
    let text = toml::to_string_pretty(project).map_err(|e| e.to_string())?;
    write_release(
        row,
        payload,
        &out.join(project_path(&project.id)),
        Some(&text),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/two-ports");
    const CALLER: &str = "name: Sunrise release\non:\n  push:\n    tags: ['clumps/v*']\njobs:\n  release:\n    uses: sunrise-org/sunrise-mods/.github/workflows/release.yml@release-v1\n    with:\n      path: clumps\n";

    #[test]
    fn caller_uses_its_attested_signer_and_a_literal_canonical_path() {
        let signer =
            SignerWorkflowRef::parse(format!("{}@refs/tags/release-v1", schema::RELEASE_WORKFLOW))
                .unwrap();
        let id = ModId::parse("clumps".into()).unwrap();
        let parse = |text: &str| caller_path(text.as_bytes(), &signer, Some(&id));
        assert_eq!(parse(CALLER).unwrap().as_str(), "clumps");
        for literal in [
            "${{ inputs.path }}",
            ".",
            "../clumps",
            "mods//clumps",
            "mods/clumps/",
            "false",
            "[clumps]",
        ] {
            assert!(
                matches!(
                    parse(&CALLER.replace("path: clumps", &format!("path: {literal}"))),
                    Err(Observation::Ineligible(_))
                ),
                "{literal}"
            );
        }
        for bad in [
            CALLER.replace("@release-v1", "@release-v2"),
            CALLER.replace("with:\n      path: clumps", "with: {}"),
            CALLER.replace("path: clumps", "path: clumps\n      path: bogus"),
            format!(
                "{CALLER}{}",
                CALLER
                    .split_once("jobs:\n")
                    .unwrap()
                    .1
                    .replace("  release:", "  other:")
            ),
        ] {
            assert!(
                matches!(parse(&bad), Err(Observation::Ineligible(_))),
                "{bad}"
            );
        }
        assert_eq!(
            caller_path(
                CALLER.replace("path: clumps", "path: '.'").as_bytes(),
                &signer,
                None
            )
            .unwrap()
            .as_str(),
            "."
        );
    }

    #[test]
    fn discovery_sorts_tag_hints_and_skips_claimed_namespaces() {
        let registry = load(Path::new(FIXTURE)).unwrap();
        let repo = serde_json::from_str("123456789").unwrap();
        let releases: Vec<github::Release> = serde_json::from_value(serde_json::json!([
            {"draft":false,"tag_name":"other/v1.0.0","published_at":"2026-11-23T10:00:00Z",
             "assets":[{"name":"other-1.0.0.smod.sigstore.json", "size":1, "browser_download_url":"unused"}]},
            {"draft":false,"tag_name":"second/v2.0.0","published_at":"2026-11-23T11:00:00Z",
             "assets":[{"name":"second-2.0.0.smod.sigstore.json", "size":1, "browser_download_url":"unused"}]},
            {"draft":false,"tag_name":"v3.0.0","published_at":"2026-11-23T12:00:00Z",
             "assets":[{"name":"wayfinder-3.0.0.smod.sigstore.json", "size":1, "browser_download_url":"unused"}]},
            {"draft":false,"tag_name":"other/v4.0.0","published_at":"2026-11-23T13:00:00Z","assets":[]}
        ])).unwrap();
        let proposed = first_candidates(&registry, repo, &releases);
        assert_eq!(
            proposed
                .iter()
                .map(|first| first.candidate.release.tag_name.as_str())
                .collect::<Vec<_>>(),
            ["second/v2.0.0", "other/v1.0.0"]
        );
    }

    #[test]
    fn first_release_compares_only_numeric_registration_authority() {
        let temp = tempfile::tempdir().unwrap();
        let mut registry = load(Path::new(FIXTURE)).unwrap();
        let repository: RepositoryId = serde_json::from_str("123456789").unwrap();
        let branch = temp.path().join(repository.file());
        let old = "name = 'org/old'\n[[package]]\npath = '.'\nproject = 'Ab3dE9xQ'\n\n[[registrant]]\nid = 19\nlogin = 'OldAdmin'\n[[registrant]]\nid = 20\nlogin = 'OtherAdmin'\n";
        fs::write(&branch, old).unwrap();
        let current =
            "name='org/renamed'\nregistrant=[{id=20,login='OtherAdmin'},{id=19,login='NewAdmin'}]";
        registry
            .repositories
            .insert(repository, toml::from_str(current).unwrap());
        assert!(same_registration_authority(&registry, &branch, repository).is_ok());
        fs::write(&branch, old.replace("id = 19", "id = 21")).unwrap();
        assert!(
            same_registration_authority(&registry, &branch, repository)
                .unwrap_err()
                .contains("authority is missing or changed")
        );
        fs::write(&branch, "name = 'org/old'").unwrap();
        assert!(same_registration_authority(&registry, &branch, repository).is_err());
        registry.repositories.remove(&repository);
        assert!(same_registration_authority(&registry, &branch, repository).is_err());
    }

    struct Accounts {
        wrong_id: bool,
        offline: bool,
    }

    impl GitHub for Accounts {
        fn repository(&self, _: Repo<'_>) -> Result<github::Repository, String> {
            unreachable!()
        }
        fn releases(&self, _: Repo<'_>) -> Result<Vec<github::Release>, String> {
            unreachable!()
        }
        fn release_branches(&self) -> Result<github::ReleaseBranches, String> {
            unreachable!()
        }
        fn tag_commit(&self, _: Repo<'_>, _: &str) -> Result<String, String> {
            unreachable!()
        }
        fn account(&self, id: NonZeroU64) -> Result<github::Account, Observation> {
            assert_eq!(id.get(), 19);
            if self.offline {
                return Err(Observation::Unknown("GitHub unavailable".into()));
            }
            Ok(serde_json::from_value(serde_json::json!({"id": if self.wrong_id { 20 } else { 19 }, "login": "CurrentAdmin"})).unwrap())
        }
    }

    #[test]
    fn first_owners_are_resolved_by_registrant_account_not_stored_login_or_tag_publisher() {
        let mut registry = load(Path::new(FIXTURE)).unwrap();
        let repository = serde_json::from_str("123456789").unwrap();
        let row = registry.repositories.get_mut(&repository).unwrap();
        row.registrant = toml::from_str::<schema::Repository>(
            "name = 'example/wayfinder'\n[[registrant]]\nid = 19\nlogin = 'PreviousAdmin'\n",
        )
        .unwrap()
        .registrant;
        let current = owners(
            &registry,
            repository,
            &Accounts {
                wrong_id: false,
                offline: false,
            },
        )
        .unwrap();
        assert_eq!(current[0].as_str(), "CurrentAdmin");
        assert!(matches!(
            owners(
                &registry,
                repository,
                &Accounts {
                    wrong_id: true,
                    offline: false
                }
            ),
            Err(Observation::Ineligible(_))
        ));
        assert!(matches!(
            owners(
                &registry,
                repository,
                &Accounts {
                    wrong_id: false,
                    offline: true
                }
            ),
            Err(Observation::Unknown(_))
        ));
        assert_eq!(
            registry.projects.values().next().unwrap().project.owners[0].as_str(),
            "Example"
        );
    }
}
