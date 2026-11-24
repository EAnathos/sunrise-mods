use super::*;
use std::io;

use crate::schema::BuildName;

fn scratch() -> tempfile::TempDir {
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target");
    fs::create_dir_all(&target).unwrap();
    tempfile::tempdir_in(target).unwrap()
}

fn fixture() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/two-ports"
    ))
}

fn offered(build: &Build, surface: &str) -> Offered {
    Offered {
        id: build.id.clone(),
        commit: build.commit.clone(),
        surface: Sha256::parse(surface.repeat(64)).unwrap(),
    }
}

/// A checkout of the fixture registry with `registry/fit` as last written.
fn judged(target: &Path) -> Registry {
    let checkout = target.join("checkout");
    copy(&fixture().join("registry"), &checkout.join("registry"));
    copy(
        &fixture().join("generated-fit"),
        &checkout.join("registry/fit"),
    );
    registry::load_generated(&checkout).unwrap()
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn candidates(registry: &Registry) -> Vec<Build> {
    registry
        .builds
        .iter()
        .map(|build| Build {
            id: build.id.clone(),
            commit: build.commit.clone(),
        })
        .collect()
}

fn written(tree: &Path, id: &str) -> Fit {
    toml::from_str(&fs::read_to_string(tree.join(format!("{id}.toml"))).unwrap()).unwrap()
}

/// Answers from a table; a build's surface is `9…9` unless the table says it is unreadable.
#[derive(Default)]
struct Recorded {
    calls: Vec<(Sha256, BuildName)>,
    unreadable: Vec<BuildName>,
    responses: BTreeMap<(Sha256, BuildName), Result<bool, String>>,
}

impl Judge for Recorded {
    fn surface(&mut self, build: &Build) -> Result<Option<Sha256>, String> {
        Ok((!self.unreadable.contains(&build.id)).then(|| offered(build, "9").surface))
    }

    fn judge(&mut self, release: &Release, build: &Offered) -> Result<bool, String> {
        let key = (release.file.sha256.clone(), build.id.clone());
        self.calls.push(key.clone());
        self.responses.get(&key).cloned().unwrap_or(Ok(false))
    }
}

#[test]
fn current_verdicts_are_kept_and_only_stale_cells_are_judged() {
    let target = scratch();
    let mut registry = judged(target.path());
    let builds = candidates(&registry);
    let output = target.path().join("fit");
    let mut judge = Recorded::default();
    write_rows(&registry, &builds, &mut judge, &output).unwrap();
    assert!(judge.calls.is_empty());
    assert_eq!(
        fs::read(output.join("Ab3dE9xQ.toml")).unwrap(),
        fs::read(fixture().join("generated-fit/Ab3dE9xQ.toml")).unwrap()
    );

    // A build that moved to another commit is a new build: its surface is read again and
    // every release is judged on it. A release whose package changed is judged on every build.
    let mut moved = builds.clone();
    moved[1].commit = Commit::parse("f".repeat(40)).unwrap();
    let changed = Sha256::parse("c".repeat(64)).unwrap();
    let second = registry.projects.values_mut().nth(1).unwrap();
    second.releases.values_mut().next().unwrap().file.sha256 = changed.clone();
    judge
        .responses
        .insert((changed.clone(), moved[0].id.clone()), Ok(true));
    write_rows(&registry, &moved, &mut judge, &output).unwrap();
    let first = registry.projects.values().next().unwrap();
    let kept = first.releases.values().next().unwrap().file.sha256.clone();
    let mut expected = vec![(kept, moved[1].id.clone())];
    expected.extend(
        moved
            .iter()
            .map(|build| (changed.clone(), build.id.clone())),
    );
    assert_eq!(judge.calls, expected);
    let row = &written(&output, "Ab3dE9xQ").release[0];
    assert_eq!(row.fits.keys().collect::<Vec<_>>(), [&moved[0].id]);
    assert_eq!(
        row.does_not_fit[&moved[1].id],
        offered(&moved[1], "9").surface
    );
    let row = &written(&output, "Zz91kLmN").release[0];
    assert_eq!((row.package.clone(), row.rules), (changed, RULES));
    assert_eq!(row.fits.keys().collect::<Vec<_>>(), [&moved[0].id]);
}

#[test]
fn a_cell_that_cannot_be_judged_fails_the_run_and_stores_nothing() {
    let target = scratch();
    let registry = registry::load(fixture()).unwrap();
    let builds = candidates(&judged(target.path()));
    let release = registry.projects.values().next().unwrap();
    let hash = release
        .releases
        .values()
        .next()
        .unwrap()
        .file
        .sha256
        .clone();
    let mut judge = Recorded::default();
    judge.responses.insert(
        (hash, builds[2].id.clone()),
        Err("recorded missing surface".into()),
    );
    let output = target.path().join("fit");
    let error = write_rows(&registry, &builds, &mut judge, &output).unwrap_err();
    assert_eq!(
        error,
        "cannot judge:\nAb3dE9xQ 1.2.0 on dev-2026-11-23-abcdef0: recorded missing surface"
    );
    assert!(!output.exists());
}

#[test]
fn a_build_with_an_unreadable_surface_is_not_offered() {
    let target = scratch();
    let registry = registry::load(fixture()).unwrap();
    let builds = candidates(&judged(target.path()));
    let mut judge = Recorded {
        unreadable: vec![builds[0].id.clone()],
        ..Recorded::default()
    };
    let output = target.path().join("fit");
    write_rows(&registry, &builds, &mut judge, &output).unwrap();
    let stored: Vec<Offered> = builds::read(&output.join("builds.json"), |b: &Offered| {
        (&b.id, &b.commit)
    })
    .unwrap();
    assert_eq!(stored, [offered(&builds[1], "9"), offered(&builds[2], "9")]);
    assert!(judge.calls.iter().all(|(_, build)| build != &builds[0].id));
    let row = &written(&output, "Ab3dE9xQ").release[0];
    assert_eq!(
        row.does_not_fit.keys().collect::<Vec<_>>(),
        [&builds[1].id, &builds[2].id]
    );
}

struct RecordedDownload(&'static [u8]);
impl Download for RecordedDownload {
    fn get(&self, _url: &str, file: &Path, _limit: u64) -> Result<(), String> {
        fs::write(file, self.0).map_err(|e: io::Error| e.to_string())
    }
}

#[test]
fn package_download_checks_hash_before_reading_the_archive() {
    let old = registry::load(fixture()).unwrap();
    let original = &old
        .projects
        .values()
        .next()
        .unwrap()
        .releases
        .values()
        .next()
        .unwrap()
        .file;
    let body = b"a recorded package response that is not really a zip";
    let mut asset: Asset = serde_json::from_value(serde_json::to_value(original).unwrap()).unwrap();
    asset.size = body.len() as u64;
    let target = scratch();
    let error = package(&RecordedDownload(body), target.path(), &asset)
        .err()
        .unwrap();
    assert!(error.contains("SHA-256"), "{error}");
}

#[test]
fn a_comparison_the_surface_cannot_finish_is_an_error_not_a_verdict() {
    use std::cell::Cell;

    struct PackageDownload<'a> {
        body: &'a [u8],
        requests: &'a Cell<usize>,
    }
    impl Download for PackageDownload<'_> {
        fn get(&self, url: &str, file: &Path, _limit: u64) -> Result<(), String> {
            assert!(url.ends_with(".smod"), "unexpected SDK download: {url}");
            self.requests.set(self.requests.get() + 1);
            fs::write(file, self.body).map_err(|e| e.to_string())
        }
    }

    let uses = include_bytes!("../../tests/fixtures/deferred-fit/uses.json");
    let table = UsesTable::read(uses).unwrap();
    let surface = || {
        Surface::new(
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        )
    };
    let report = table.fits(table.entries.iter(), &surface());
    assert!(matches!(report.verdict, SurfaceFit::Fits));
    assert!(report.deferred.is_some());
    let pin = table.made_for;
    let manifest = format!(
        "id='wayfinder'\nname='Wayfinder'\nversion='1.2.0'\n[native]\ngame='26.3'\nsunrise='{pin}'\n"
    );
    let body = Package::write_files(
        &manifest,
        &[
            ("Cargo.toml", b"[package]\nname='wayfinder'\n"),
            ("src/lib.rs", b"pub fn read_map() {}\n"),
            (USES_FILE, uses),
        ],
    )
    .unwrap();
    let mut registry = registry::load(fixture()).unwrap();
    registry.projects.retain(|id, _| id.as_str() == "Ab3dE9xQ");
    let release = registry
        .projects
        .values_mut()
        .next()
        .unwrap()
        .releases
        .values_mut()
        .next()
        .unwrap();
    release.file.size = body.len() as u64;
    release.file.sha256 = Sha256::parse(sha256_hex(&body)).unwrap();
    let release = &*release;
    let target = scratch();
    let builds = candidates(&judged(target.path()));
    let build = offered(&builds[0], "1");
    let requests = Cell::new(0);
    let mut tool = Tool {
        root: target.path(),
        downloads: PackageDownload {
            body: &body,
            requests: &requests,
        },
        packages: BTreeMap::new(),
        sdks: BTreeMap::from([(
            build.commit.clone(),
            Ok(Rc::new(Sdk {
                content_api: Manifest::parse(&manifest).unwrap().api,
                game: "26.3".into(),
                surface_bytes: vec![],
                surface: Ok(surface()),
            })),
        )]),
    };
    // The build was offered with another surface than the one now published for its commit.
    assert!(
        tool.judge(release, &build)
            .unwrap_err()
            .contains("not the one")
    );
    let build = Offered {
        surface: Sha256::parse(sha256_hex(&[])).unwrap(),
        ..build
    };
    assert_eq!(tool.judge(release, &build), Err(report.deferred.unwrap()));
    assert_eq!(requests.get(), 1);
}
