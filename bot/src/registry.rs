use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    path::{Path, PathBuf},
};

use percent_encoding::percent_decode_str;
use semver::Version;

use crate::{
    builds::{self, Offered},
    schema::*,
    tree::{Entry, Tree},
};
use sunrise_mod_package::{
    icon::MAX_ICON,
    index::v2::collection::{Collection, ICON, Part, Registered, TOML, check_collection},
};

/// The site's own pages under `/mods/` (sunny `frontend/site/src/pages/mods/`: `mine.astro`, `policy.astro`,
/// `registry-page.astro`, `submit.astro`). A project's page is `/mods/<slug>`, so no project takes one of these.
const SITE_PAGES: [&str; 4] = ["mine", "policy", "registry-page", "submit"];

pub struct Registry {
    pub repositories: BTreeMap<RepositoryId, Repository>,
    pub release_workflow: WorkflowUse,
    pub categories: Vec<Category>,
    pub projects: BTreeMap<ProjectId, ProjectEntry>,
    pub collections: BTreeMap<ProjectId, CollectionEntry>,
    pub fits: BTreeMap<ProjectId, BTreeMap<Version, FitRelease>>,
    pub builds: Vec<Offered>,
    /// The SHA-512 of each original jar Modrinth does not list, as the judging job stored them.
    pub unlisted_jars: BTreeSet<String>,
    pub revoked: Vec<Revocation>,
    claims: ProjectClaims,
}

pub struct ProjectEntry {
    pub project: Project,
    pub repository_name: RepositoryName,
    pub page: Option<Page>,
    pub releases: BTreeMap<Version, Release>,
}

pub struct CollectionEntry {
    pub collection: Collection,
    /// The one GitHub account that owns every member's repository.
    pub publisher: String,
}

pub fn load(checkout: &Path) -> Result<Registry, Vec<String>> {
    load_inner(checkout, false)
}

pub fn load_generated(checkout: &Path) -> Result<Registry, Vec<String>> {
    load_inner(checkout, true)
}

fn load_inner(checkout: &Path, generated: bool) -> Result<Registry, Vec<String>> {
    let mut tree = Tree::new(checkout);
    let root = Path::new("registry");
    let mut top = match tree.entry(root) {
        Some(Entry::Directory) => tree.list(root),
        Some(Entry::File) => return Err(vec!["registry: expected a directory".to_owned()]),
        None => return Err(tree.finish()),
    };
    let repositories_root = root.join("repositories");
    let mut repository_files = match top.remove(OsStr::new("repositories")) {
        Some(Entry::Directory) => tree.list(&repositories_root),
        _ => {
            tree.problem(&repositories_root, "expected a repositories directory");
            Default::default()
        }
    };
    if repository_files.remove(OsStr::new(".gitkeep")) == Some(Entry::Directory) {
        tree.problem(
            &repositories_root.join(".gitkeep"),
            "expected a regular file",
        );
    }
    let categories = tree
        .toml::<Categories>(
            &root.join("categories.toml"),
            top.remove(OsStr::new("categories.toml")),
        )
        .map_or_else(Vec::new, |value| value.category);
    let revoked_path = root.join("revoked.toml");
    let revoked = tree
        .toml::<Revocations>(&revoked_path, top.remove(OsStr::new("revoked.toml")))
        .map_or_else(Vec::new, |value| value.revoked);
    let release_workflow = tree.toml::<ReleaseWorkflow>(
        &root.join("release-workflow.toml"),
        top.remove(OsStr::new("release-workflow.toml")),
    );
    let projects_root = root.join("projects");
    let mut project_dirs = match top.remove(OsStr::new("projects")) {
        Some(Entry::Directory) => tree.list(&projects_root),
        _ => {
            tree.problem(&projects_root, "expected a projects directory");
            Default::default()
        }
    };
    if project_dirs.remove(OsStr::new(".gitkeep")) == Some(Entry::Directory) {
        tree.problem(&projects_root.join(".gitkeep"), "expected a regular file");
    }
    if let Some(entry) = top.remove(OsStr::new("index-key.pub"))
        && entry != Entry::File
    {
        tree.problem(
            &root.join("index-key.pub"),
            "expected a regular public key file",
        );
    }
    let collections_root = root.join("collections");
    let collections_dir = top.remove(OsStr::new("collections"));
    let fit_path = root.join("fit");
    let fit_dir = top.remove(OsStr::new("fit"));
    if !generated && fit_dir.is_some() {
        tree.problem(
            &fit_path,
            "fit is generated during publishing and must not be committed",
        );
    }
    tree.unknown(root, top);

    let mut by_category = BTreeMap::new();
    for category in &categories {
        if by_category.insert(&category.id, category).is_some() {
            tree.problem(
                &root.join("categories.toml"),
                format!("duplicate category {}", category.id.as_str()),
            );
        }
    }
    let mut repositories = BTreeMap::new();
    for (name, entry) in repository_files {
        let path = repositories_root.join(&name);
        let Some(id) = name.to_str().and_then(RepositoryId::of_file) else {
            tree.problem(&path, "expected a file named <numeric repository ID>.toml");
            continue;
        };
        if let Some(repository) = tree.toml::<Repository>(&path, Some(entry)) {
            let mut accounts = BTreeSet::new();
            if repository
                .registrant
                .iter()
                .any(|account| !accounts.insert(account.id))
            {
                tree.problem(&path, "duplicate registrant account ID");
            }
            repositories.insert(id, (path, repository));
        }
    }
    // CUTOVER: delete this cross-check of legacy `[[package]]` lists against project files, with
    // `Repository::package` (`Option<Vec<Package>>`) and the `registrations` use below.
    let mut registrations = BTreeMap::new();
    for (id, (path, repo)) in &repositories {
        let mut package_paths = BTreeSet::new();
        for package in repo.package.iter().flatten() {
            if !package_paths.insert(&package.path) {
                tree.problem(
                    path,
                    format!(
                        "duplicate package path in {} for project {}",
                        repo.name.as_str(),
                        package.project.as_str()
                    ),
                );
            }
            if registrations
                .insert(&package.project, (*id, path, package))
                .is_some()
            {
                tree.problem(
                    path,
                    format!(
                        "duplicate registered project ID {}",
                        package.project.as_str()
                    ),
                );
            }
        }
    }

    let mut projects = BTreeMap::new();
    let mut claims = ProjectClaims::default();
    for (name, entry) in project_dirs {
        let dir = projects_root.join(&name);
        let Ok(id) = ProjectId::parse(name.to_string_lossy().into_owned()) else {
            tree.problem(
                &dir,
                "expected a directory named by an 8-character base62 project ID",
            );
            continue;
        };
        if entry != Entry::Directory {
            tree.problem(&dir, "expected a project directory");
            continue;
        }
        let project_path = dir.join("project.toml");
        let mut entries = tree.list(&dir);
        let project =
            tree.toml::<Project>(&project_path, entries.remove(OsStr::new("project.toml")));
        let releases_dir = dir.join("releases");
        let releases = match entries.remove(OsStr::new("releases")) {
            Some(Entry::Directory) => read_releases(&mut tree, &releases_dir, project.as_ref()),
            Some(Entry::File) => {
                tree.problem(&releases_dir, "expected a releases directory");
                BTreeMap::new()
            }
            None => BTreeMap::new(),
        };
        let page_dir = dir.join("page");
        let page = match entries.remove(OsStr::new("page")) {
            Some(Entry::Directory) => read_page(
                &mut tree,
                &page_dir,
                &by_category,
                project.as_ref().map(|p| p.kind),
            ),
            entry => {
                if entry.is_some() || !releases.is_empty() {
                    tree.problem(
                        &page_dir,
                        "expected a page directory with page.toml and images",
                    );
                }
                None
            }
        };
        tree.unknown(&dir, entries);
        // CUTOVER: delete (legacy package-list cross-check, see above).
        let registration = registrations.remove(&id);
        if let Some(project) = project {
            if project.id != id {
                tree.problem(&project_path, "project ID must match the directory name");
            }
            for problem in claims.conflicts(&id, &project) {
                tree.problem(&project_path, problem);
            }
            claims.insert(id.clone(), &project);
            if project.workflow.as_str() != RELEASE_WORKFLOW {
                tree.problem(
                    &project_path,
                    "workflow must match the path in release-workflow.toml before '@'",
                );
            }
            if let Some((repository, _, package)) = registration {
                if repository != project.repository {
                    tree.problem(
                        &project_path,
                        "repository ID does not match the repository file that lists this project",
                    );
                }
                if package.path != project.path {
                    tree.problem(
                        &project_path,
                        "package path does not match its repository file",
                    );
                }
            }
            if let Some((_, repo)) = repositories.get(&project.repository) {
                projects.insert(
                    id,
                    ProjectEntry {
                        project,
                        repository_name: repo.name.clone(),
                        page,
                        releases,
                    },
                );
            } else {
                tree.problem(
                    &project_path,
                    "repository ID does not match any registry/repositories file",
                );
            }
        }
    }
    // CUTOVER: delete (legacy package-list cross-check, see above).
    for (id, (_, path, _)) in &registrations {
        tree.problem(path, format!("project {} has no project.toml", id.as_str()));
    }
    for (id, entry) in &projects {
        for release in entry.releases.values() {
            let path = projects_root
                .join(id.as_str())
                .join("releases")
                .join(format!("{}.toml", release.version));
            if release.kind != entry.project.kind {
                tree.problem(&path, "release kind does not match the project kind");
            }
            for dependency in &release.dependencies {
                if let Some(target) = &dependency.project {
                    match projects.get(target) {
                        None => tree.problem(
                            &path,
                            format!("dependency names unregistered project {}", target.as_str()),
                        ),
                        Some(provider) if provider.project.mod_id != dependency.mod_id => tree
                            .problem(
                                &path,
                                format!(
                                    "dependency project {} does not provide {}",
                                    target.as_str(),
                                    dependency.mod_id.as_str()
                                ),
                            ),
                        Some(_) => {}
                    }
                }
            }
        }
    }
    let collections = match collections_dir {
        Some(Entry::Directory) => read_collections(&mut tree, &collections_root, &projects),
        Some(Entry::File) => {
            tree.problem(&collections_root, "expected a collections directory");
            BTreeMap::new()
        }
        None => BTreeMap::new(),
    };
    for id in collections.keys() {
        if !claims.ids.insert(id.clone()) {
            tree.problem(
                &collections_root.join(id.as_str()),
                "project already uses this collection ID",
            );
        }
    }
    let mut revocations = BTreeSet::new();
    for row in &revoked {
        if !revocations.insert((&row.project, &row.sha256)) {
            tree.problem(
                &revoked_path,
                format!(
                    "duplicate takedown for project {} and hash",
                    row.project.as_str()
                ),
            );
        }
    }
    let mut fits = BTreeMap::new();
    let mut builds = Vec::new();
    let mut unlisted_jars = BTreeSet::new();
    if generated {
        match fit_dir {
            Some(Entry::Directory) => {
                let mut contents = tree.list(&fit_path);
                let builds_path = fit_path.join("builds.json");
                match contents.remove(OsStr::new("builds.json")) {
                    Some(Entry::File) => {
                        match builds::read(&checkout.join(&builds_path), |build: &Offered| {
                            (&build.id, &build.commit)
                        }) {
                            Ok(rows) => builds = rows,
                            Err(message) => tree.problem(&builds_path, message),
                        }
                    }
                    _ => tree.problem(&builds_path, "expected the generated offered-build list"),
                }
                // The judging job's own record of the Sunrise releases it has looked at.
                if contents.remove(OsStr::new(crate::fit::CONSIDERED)) == Some(Entry::Directory) {
                    tree.problem(
                        &fit_path.join(crate::fit::CONSIDERED),
                        "expected a regular file",
                    );
                }
                // Absent until the judging job has first asked Modrinth.
                if contents.remove(OsStr::new(crate::jar::UNLISTED)).is_some() {
                    let path = fit_path.join(crate::jar::UNLISTED);
                    match std::fs::read(checkout.join(&path))
                        .map_err(|e| e.to_string())
                        .and_then(|bytes: Vec<u8>| {
                            serde_json::from_slice(&bytes).map_err(|e| e.to_string())
                        }) {
                        Ok(jars) => unlisted_jars = jars,
                        Err(message) => tree.problem(&path, message),
                    }
                }
                fits = read_fits(&mut tree, &fit_path, contents);
            }
            _ => tree.problem(&fit_path, "expected the generated fit directory"),
        }
    }
    let problems = tree.finish();
    if !problems.is_empty() {
        return Err(problems);
    }
    let Some(release_workflow) = release_workflow else {
        return Err(vec!["registry: missing release-workflow.toml".to_owned()]);
    };
    Ok(Registry {
        repositories: repositories
            .into_iter()
            .map(|(id, (_, repository))| (id, repository))
            .collect(),
        release_workflow: release_workflow.uses,
        categories,
        projects,
        collections,
        fits,
        builds,
        unlisted_jars,
        revoked,
        claims,
    })
}

#[derive(Default)]
struct ProjectClaims {
    ids: BTreeSet<ProjectId>,
    slugs: BTreeSet<Slug>,
    paths: BTreeSet<(RepositoryId, PackagePath)>,
    subfolder_mods: BTreeSet<(RepositoryId, ModId)>,
}

impl ProjectClaims {
    fn conflicts(&self, id: &ProjectId, project: &Project) -> Vec<String> {
        let mut problems = Vec::new();
        if self.ids.contains(id) {
            problems.push(format!(
                "project or collection already uses ID {}",
                id.as_str()
            ));
        }
        let slug = project.slug.as_str();
        if SITE_PAGES.contains(&slug) {
            problems.push(format!(
                "reserved slug {slug}: the site has its own page at /mods/{slug}"
            ));
        }
        if self.slugs.contains(&project.slug) {
            problems.push(format!("duplicate slug {slug}"));
        }
        if self
            .paths
            .contains(&(project.repository, project.path.clone()))
        {
            problems.push("duplicate package path in this repository".into());
        }
        if matches!(project.path, PackagePath::Folder(_))
            && self
                .subfolder_mods
                .contains(&(project.repository, project.mod_id.clone()))
        {
            problems.push("another subfolder package in this repository has the same in-game ID (shared tag and caller workflow names)".into());
        }
        problems
    }

    fn insert(&mut self, id: ProjectId, project: &Project) {
        self.ids.insert(id);
        self.slugs.insert(project.slug.clone());
        self.paths
            .insert((project.repository, project.path.clone()));
        if matches!(project.path, PackagePath::Folder(_)) {
            self.subfolder_mods
                .insert((project.repository, project.mod_id.clone()));
        }
    }
}

impl Registry {
    pub fn has_package_path(&self, repository: RepositoryId, path: &PackagePath) -> bool {
        self.claims.paths.contains(&(repository, path.clone()))
    }

    pub fn first_slug(&self, id: &ProjectId, mod_id: &ModId) -> Result<Slug, &'static str> {
        let slug = Slug::parse(mod_id.as_str().into())?;
        if self.claims.slugs.contains(&slug) && !SITE_PAGES.contains(&slug.as_str()) {
            Slug::parse(format!(
                "{}-{}",
                slug.as_str(),
                id.as_str().to_ascii_lowercase()
            ))
        } else {
            Ok(slug)
        }
    }

    /// Check an addition against main, not a merged tree in which it could replace a claim.
    pub fn check_new_project(&self, project: &Project) -> Result<(), String> {
        let problems = self.claims.conflicts(&project.id, project);
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }
}

pub fn first_project_id(
    repository: RepositoryId,
    path: &PackagePath,
) -> Result<ProjectId, &'static str> {
    const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut key = String::with_capacity(8);
    // Domain-separated counter mode; reject the remainder so every digit has four preimages.
    for block in 0..=u64::MAX {
        let hash = sunrise_mod_package::index::sha256(
            format!(
                "sunrise-registry-project-v1\0{}\0{}\0{block}",
                repository.get(),
                path.as_str()
            )
            .as_bytes(),
        );
        for at in (0..hash.len()).step_by(2) {
            let byte = u8::from_str_radix(&hash[at..at + 2], 16)
                .map_err(|_| "SHA-256 was not hexadecimal")?;
            if byte < 248 {
                key.push(char::from(BASE62[usize::from(byte % 62)]));
                if key.len() == 8 {
                    return ProjectId::parse(key);
                }
            }
        }
    }
    Err("could not derive a project ID")
}

/// The caller checks immutable PR metadata and Git modes; this is the repository row boundary.
pub fn check_registration(path: &Path) -> Result<(), String> {
    if path
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(RepositoryId::of_file)
        .is_none()
    {
        return Err("expected <numeric repository ID>.toml".into());
    }
    let mut tree = Tree::new(Path::new(""));
    let entry = tree.entry(path);
    let row = tree.toml::<Repository>(path, entry);
    if let Some(row) = row
        && (row.package.is_some() || row.registrant.len() != 1)
    {
        tree.problem(
            path,
            "a new repository requires one registrant and no package list",
        );
    }
    let problems = tree.finish();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

fn read_collections(
    tree: &mut Tree<'_>,
    root: &Path,
    projects: &BTreeMap<ProjectId, ProjectEntry>,
) -> BTreeMap<ProjectId, CollectionEntry> {
    let mut collections = BTreeMap::new();
    for (name, entry) in tree.list(root) {
        let dir = root.join(&name);
        if entry != Entry::Directory {
            tree.problem(&dir, "expected a collection directory");
            continue;
        }
        let folder = name.to_string_lossy().into_owned();
        let file = dir.join(TOML);
        let icon_path = dir.join(ICON);
        let mut entries = tree.list(&dir);
        let collection = tree.toml::<Collection>(&file, entries.remove(OsStr::new(TOML)));
        let icon = match entries.get(OsStr::new(ICON)) {
            Some(Entry::File) => tree.bytes(&icon_path, MAX_ICON as u64 + 1),
            _ => None,
        };
        let names: Vec<String> = entries
            .keys()
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let lookup = |project: &str| {
            let entry = projects.get(&ProjectId::parse(project.to_owned()).ok()?)?;
            Some(Registered {
                publisher: entry
                    .repository_name
                    .as_str()
                    .split_once('/')
                    .expect("validated when parsed")
                    .0
                    .to_owned(),
                owners: entry
                    .project
                    .owners
                    .iter()
                    .map(|login| login.as_str().to_owned())
                    .collect(),
            })
        };
        let report = check_collection(
            &folder,
            &names,
            collection.as_ref(),
            icon.as_deref(),
            &lookup,
        );
        for problem in report.problems {
            let path = match problem.part {
                Part::Folder => dir.clone(),
                Part::Toml => file.clone(),
                Part::Icon => icon_path.clone(),
                Part::Entry(name) => dir.join(name),
            };
            tree.problem(&path, problem.message);
        }
        if let (Some(collection), Some(publisher), Ok(id)) =
            (collection, report.publisher, ProjectId::parse(folder))
        {
            collections.insert(
                id,
                CollectionEntry {
                    collection,
                    publisher,
                },
            );
        }
    }
    collections
}

/// A pull request never takes a listed project off the registry by accident: a project with
/// releases on `base` still has one on `head`, unless `head` pauses it. Of `base` only the names
/// under `registry/projects/` are read, so a pull request that changes what a file holds is
/// still compared.
pub fn stay_listed(base: &Path, head: &Path) -> Result<(), String> {
    let after = load(head).map_err(|e| e.join("\n"))?;
    let projects = base.join("registry/projects");
    let mut unlisted = Vec::new();
    for entry in std::fs::read_dir(&projects).map_err(|e| format!("{}: {e}", projects.display()))? {
        let name = entry.map_err(|e| e.to_string())?.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| ProjectId::parse(name.into()).ok())
        else {
            continue;
        };
        let was_listed = std::fs::read_dir(projects.join(&name).join("releases"))
            .is_ok_and(|mut releases| releases.next().is_some());
        let gone = after
            .projects
            .get(&id)
            .is_none_or(|now| now.releases.is_empty() && !now.project.paused);
        if was_listed && gone {
            unlisted.push(format!(
                "registry/projects/{}: unlisted project: this change removes its last listed release. \
                 A release of another kind replaces the earlier ones in its own release pull request; \
                 to take the project off the registry, set `paused = true` in the same change",
                id.as_str()
            ));
        }
    }
    unlisted.sort();
    if unlisted.is_empty() {
        Ok(())
    } else {
        Err(unlisted.join("\n"))
    }
}

/// Refuses a change to a collection by anyone but an owner of every project it lists, before
/// and after the change. Ownership is read from `base`, so a pull request cannot grant it to
/// itself. Of `base` only the collection files and the members' `project.toml` are read: the
/// base commit was checked by its own checker, and a pull request may change what a row holds.
pub fn collection_author(base: &Path, head: &Path, login: &str) -> Result<(), String> {
    let after = load(head).map_err(|e| e.join("\n"))?;
    let read = |path: PathBuf| match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    };
    let collections = Path::new("registry/collections");
    let mut ids: BTreeSet<ProjectId> = after.collections.keys().cloned().collect();
    if let Ok(entries) = std::fs::read_dir(base.join(collections)) {
        for entry in entries {
            let name = entry.map_err(|e| e.to_string())?.file_name();
            if let Some(id) = name
                .to_str()
                .and_then(|name| ProjectId::parse(name.into()).ok())
            {
                ids.insert(id);
            }
        }
    }
    let files = |checkout: &Path, id: &ProjectId| {
        let dir = checkout.join(collections).join(id.as_str());
        (
            std::fs::read(dir.join("collection.toml")).ok(),
            std::fs::read(dir.join("icon.png")).ok(),
        )
    };
    for id in &ids {
        if files(base, id) == files(head, id) {
            continue;
        }
        let file = base
            .join(collections)
            .join(id.as_str())
            .join("collection.toml");
        let before = read(file.clone())?
            .map(|text| toml::from_str::<Collection>(&text))
            .transpose()
            .map_err(|error| format!("{}: {error}", file.display()))?;
        let members = before
            .iter()
            .chain(after.collections.get(id).map(|entry| &entry.collection))
            .flat_map(|collection| &collection.member);
        for member in members {
            let file = base
                .join("registry/projects")
                .join(member.project.as_str())
                .join("project.toml");
            let project = read(file.clone())?
                .map(|text| toml::from_str::<Project>(&text))
                .transpose()
                .map_err(|error| format!("{}: {error}", file.display()))?;
            let owns = project.is_some_and(|project| {
                project
                    .owners
                    .iter()
                    .any(|owner| owner.as_str().eq_ignore_ascii_case(login))
            });
            if !owns {
                return Err(format!(
                    "collection {} lists project {}, which {login} does not own on the base commit: only the owner of its mods curates a collection",
                    id.as_str(),
                    member.project.as_str()
                ));
            }
        }
    }
    Ok(())
}

fn read_releases(
    tree: &mut Tree<'_>,
    dir: &Path,
    project: Option<&Project>,
) -> BTreeMap<Version, Release> {
    let mut releases = BTreeMap::new();
    for (name, entry) in tree.list(dir) {
        let file = dir.join(name);
        let version = file
            .file_stem()
            .and_then(OsStr::to_str)
            .and_then(|stem| Version::parse(stem).ok());
        let Some(version) = version.filter(|_| file.extension() == Some(OsStr::new("toml"))) else {
            tree.problem(
                &file,
                "expected a <version>.toml release file named by a semantic version",
            );
            continue;
        };
        let Some(row) = tree.toml::<Release>(&file, Some(entry)) else {
            continue;
        };
        if version != row.version {
            tree.problem(&file, "release version must match its filename");
        }
        if let Some(project) = project {
            let expected = match project.path {
                PackagePath::Root(_) => format!("v{}", row.version),
                PackagePath::Folder(_) => format!("{}/v{}", project.mod_id.as_str(), row.version),
            };
            if row.tag.as_str() != expected {
                tree.problem(&file, format!("tag must be {expected}"));
            }
            if row.workflow.as_str().split_once('@').map(|(path, _)| path)
                != Some(project.workflow.as_str())
            {
                tree.problem(&file, "signer workflow path must match project.toml");
            }
            for (field, asset, suffix) in
                [("file", &row.file, "smod"), ("page", &row.page, "page.tar")]
            {
                let name = format!("{}-{}.{suffix}", project.mod_id.as_str(), row.version);
                let bundle = format!("{name}.sigstore.json");
                for (key, url, name) in [
                    ("url", &asset.url, &name),
                    ("attestation", &asset.attestation, &bundle),
                ] {
                    if release_download_repository(url.as_str(), row.tag.as_str(), name).is_none() {
                        tree.problem(&file, format!("{field}.{key} must be a GitHub release download HTTPS URL for {name} with the release tag"));
                    }
                }
            }
        }
        releases.insert(version, row);
    }
    releases
}

pub(crate) fn release_download_repository<'a>(
    url: &'a str,
    tag: &str,
    asset: &str,
) -> Option<&'a str> {
    // Check the raw authority and repository path, then decode only the release tag and asset.
    // GitHub's browser_download_url can percent-encode a tag such as wayfinder/v1.2.0.
    let (repository, download) = url
        .strip_prefix("https://github.com/")?
        .split_once("/releases/download/")?;
    let (tag_part, asset_part) = download.rsplit_once('/')?;
    (github_repository(repository)
        && !download.contains(['?', '#'])
        && percent_decode_str(tag_part).eq(tag.bytes())
        && percent_decode_str(asset_part).eq(asset.bytes()))
    .then_some(repository)
}

fn read_page(
    tree: &mut Tree<'_>,
    dir: &Path,
    categories: &BTreeMap<&Slug, &Category>,
    kind: Option<Kind>,
) -> Option<Page> {
    let mut entries = tree.list(dir);
    let file = dir.join("page.toml");
    let page = tree.toml::<Page>(&file, entries.remove(OsStr::new("page.toml")));
    let mut assets = BTreeSet::<PathBuf>::new();
    let mut pending = vec![(dir.to_owned(), entries)];
    while let Some((dir, entries)) = pending.pop() {
        for (name, entry) in entries {
            let path = dir.join(name);
            match entry {
                Entry::Directory => pending.push((path.clone(), tree.list(&path))),
                Entry::File => {
                    assets.insert(path);
                }
            }
        }
    }
    let page = page?;
    // The thresholds are from Modrinth validate/project/{summary.rs:13,description.rs:17}.
    // Counting trimmed Unicode scalar values matches the author-side release check.
    if page.summary.as_str().trim().chars().count() < 25 {
        tree.problem(&file, "summary needs at least 25 characters");
    }
    if page.description.as_str().trim().chars().count() < 125 {
        tree.problem(&file, "description needs at least 125 characters");
    }
    // Modrinth validate/project/tags.rs:10.
    if page.categories.len() > 8 {
        tree.problem(&file, "at most 8 categories are allowed");
    }
    let mut selected = BTreeSet::new();
    for id in &page.categories {
        if !selected.insert(id) {
            tree.problem(&file, format!("duplicate category {}", id.as_str()));
        }
        match categories.get(id) {
            None => tree.problem(&file, format!("unknown category {}", id.as_str())),
            Some(category) if kind.is_some_and(|kind| !category.kinds.contains(&kind)) => {
                tree.problem(
                    &file,
                    format!(
                        "category {} does not apply to this project kind",
                        id.as_str()
                    ),
                );
            }
            Some(_) => {}
        }
    }
    if page.screenshots.is_empty() {
        tree.problem(&file, "at least one screenshot is required");
    }
    let images: BTreeSet<_> = std::iter::once(&page.icon)
        .chain(page.screenshots.iter().map(|s| &s.path))
        .collect();
    for image in images {
        let path = dir.join(image.as_str());
        if assets.remove(&path) {
            if image == &page.icon {
                tree.icon(&path);
            } else {
                tree.screenshot(&path);
            }
        } else {
            tree.problem(
                &file,
                format!(
                    "image {:?} is not a regular file in the page directory",
                    image.as_str()
                ),
            );
        }
    }
    for asset in assets {
        tree.problem(&asset, "page file is not referenced by page.toml");
    }
    Some(page)
}

/// Reads the stored verdicts as written. A row for a release, project or build that is gone is
/// not a problem here: the judging run drops it. A release with no current verdict on an
/// offered build is not one either: the index leaves that cell out until it is judged.
fn read_fits(
    tree: &mut Tree<'_>,
    dir: &Path,
    contents: crate::tree::Entries,
) -> BTreeMap<ProjectId, BTreeMap<Version, FitRelease>> {
    let mut fits = BTreeMap::new();
    for (name, entry) in contents {
        let file = dir.join(name);
        let id = file
            .file_stem()
            .and_then(OsStr::to_str)
            .and_then(|stem| ProjectId::parse(stem.to_owned()).ok());
        let Some(id) = id.filter(|_| file.extension() == Some(OsStr::new("toml"))) else {
            tree.problem(
                &file,
                "expected a <project>.toml fit file named by a base62 project ID",
            );
            continue;
        };
        let Some(fit) = tree.toml::<Fit>(&file, Some(entry)) else {
            continue;
        };
        let mut releases = BTreeMap::new();
        for row in fit.release {
            if let Some(build) = row.fits.keys().find(|b| row.does_not_fit.contains_key(*b)) {
                tree.problem(
                    &file,
                    format!(
                        "{} both fits and does not fit {}",
                        row.version,
                        build.as_str()
                    ),
                );
            }
            let version = row.version.clone();
            if releases.insert(version.clone(), row).is_some() {
                tree.problem(&file, format!("duplicate fit row for {version}"));
            }
        }
        fits.insert(id, releases);
    }
    fits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_project_claim_checks_all_collisions_without_overlaying_main() {
        let root = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/two-ports"
        ));
        let mut registry = load(root).unwrap();
        let repository = serde_json::from_str("123456789").unwrap();
        let path = PackagePath::parse("mods/demo".into()).unwrap();
        let id = first_project_id(repository, &path).unwrap();
        assert_eq!(id, first_project_id(repository, &path).unwrap());
        assert_ne!(
            id,
            first_project_id(repository, &PackagePath::parse(".".into()).unwrap()).unwrap()
        );
        let project = |registry: &Registry| {
            let mod_id = ModId::parse("wayfinder".into()).unwrap();
            Project {
                id: id.clone(),
                slug: registry.first_slug(&id, &mod_id).unwrap(),
                mod_id,
                kind: Kind::Mod,
                owners: vec![],
                repository,
                path: path.clone(),
                workflow: Text::parse(RELEASE_WORKFLOW.into()).unwrap(),
                registered: CalendarDate::parse("2026-11-25".into()).unwrap(),
                paused: false,
            }
        };
        let claim = project(&registry);
        assert_eq!(
            claim.slug.as_str(),
            format!("wayfinder-{}", id.as_str().to_ascii_lowercase())
        );
        assert!(registry.check_new_project(&claim).is_ok());
        registry.claims.slugs.insert(claim.slug.clone());
        assert!(
            registry
                .check_new_project(&claim)
                .unwrap_err()
                .contains("duplicate slug")
        );
        registry.claims.slugs.remove(&claim.slug);
        registry
            .claims
            .subfolder_mods
            .insert((repository, claim.mod_id.clone()));
        assert!(
            registry
                .check_new_project(&claim)
                .unwrap_err()
                .contains("same in-game ID")
        );
        registry
            .claims
            .subfolder_mods
            .remove(&(repository, claim.mod_id.clone()));
        registry.claims.paths.insert((repository, path.clone()));
        assert!(
            registry
                .check_new_project(&claim)
                .unwrap_err()
                .contains("duplicate package path")
        );
        registry.claims.paths.remove(&(repository, path));
        registry.claims.ids.insert(id);
        assert!(
            registry
                .check_new_project(&claim)
                .unwrap_err()
                .contains("project or collection already uses ID")
        );
    }
}
