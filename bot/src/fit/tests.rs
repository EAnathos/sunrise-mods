use super::*;
use std::{io, slice::from_ref};

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

/// The SDK index of `commit` as far as a fit reads it: its content API, game and mixins
/// revision.
fn index(commit: &Commit, content_api: String) -> sdk::Index {
    sdk::Index {
        schema: 1,
        build: sdk::Build {
            commit: commit.as_str().into(),
            game: "26.3".into(),
            date: "2026-11-24".into(),
            nightly: None,
        },
        package_format: 1,
        toolchain: sdk::Toolchain {
            rust: "1.99.1".into(),
        },
        content_api,
        surface: sdk::Surface {
            hash: "ab".repeat(16),
            previous: None,
            source: None,
            changes: Vec::new(),
            notes: Vec::new(),
        },
        mixins: sdk::Mixins {
            rev: "e".repeat(40),
            patch: "c".repeat(64),
        },
        assets: BTreeMap::new(),
        inputs: sdk::Inputs {
            routes: BTreeMap::new(),
            how: BTreeMap::new(),
            members: BTreeMap::new(),
            unknown: BTreeMap::new(),
        },
    }
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
                index: index(&build.commit, Manifest::parse(&manifest).unwrap().api),
                surface_bytes: vec![],
                surface: Ok(Rc::new(surface())),
            })),
        )]),
        data: BTreeMap::new(),
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

/// A mixin target as a build's function index holds it.
const TARGET: &str = "sunrise_client::game::f";
/// The build a native mod in these tests was made for, another than the one it is judged on.
const MADE_FOR: &str = "dddddddddddddddddddddddddddddddddddddddd";

/// The build data of `commit`: the function index holds [`TARGET`] with `body`.
fn build_data(dir: &Path, commit: &Commit, body: &str) -> Data {
    let header = serde_json::json!({"schema": 1, "build": {"commit": commit.as_str()}});
    for (file, body) in [
        (
            "functions.json",
            serde_json::json!({"crates": {}, "refused": {}, "functions": {TARGET: {
                "module": "sunrise_client::game", "file": "crates/sunrise-client/src/game.rs",
                "span": [0, 1], "signature_hash": "sig", "sha256": body,
            }}}),
        ),
        ("internals.json", serde_json::json!({"internals": []})),
        ("loops.json", serde_json::json!({"loops": []})),
    ] {
        let mut document = header.clone();
        document
            .as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        fs::write(dir.join(file), document.to_string()).unwrap();
    }
    Data::read(dir, commit.as_str()).unwrap()
}

/// Whether the bot stores as fitting a native mod made for [`MADE_FOR`] whose package holds
/// `manifest` (after its `[native]` table) and `files`, judged on the fixture's first build
/// with `surface` (a `surface.json`) and, when given, the build data `body` makes. Only the
/// package can be downloaded.
fn stored(
    native: &str,
    files: &[(&str, &[u8])],
    surface: serde_json::Value,
    body: Option<&str>,
) -> Result<bool, String> {
    struct PackageOnly(Vec<u8>);
    impl Download for PackageOnly {
        fn get(&self, url: &str, file: &Path, _limit: u64) -> Result<(), String> {
            if !url.ends_with(".smod") {
                return Err(format!("{url}: download not found (HTTP 404)"));
            }
            fs::write(file, &self.0).map_err(|e| e.to_string())
        }
    }

    let manifest = format!(
        "id='wayfinder'\nname='Wayfinder'\nversion='1.2.0'\n[native]\ngame='26.3'\nsunrise='{MADE_FOR}'\n{native}"
    );
    let mut all = vec![
        ("Cargo.toml", &b"[package]\nname='wayfinder'\n"[..]),
        ("src/lib.rs", b"pub fn read_map() {}\n"),
    ];
    all.extend_from_slice(files);
    let body_bytes = Package::write_files(&manifest, &all).unwrap();
    let target = scratch();
    let mut registry = judged(target.path());
    registry.projects.retain(|id, _| id.as_str() == "Ab3dE9xQ");
    let project = registry.projects.values_mut().next().unwrap();
    let first = project.releases.pop_first().unwrap();
    project.releases = BTreeMap::from([first]);
    let release = project.releases.values_mut().next().unwrap();
    release.file.size = body_bytes.len() as u64;
    release.file.sha256 = Sha256::parse(sha256_hex(&body_bytes)).unwrap();
    registry.fits.clear();
    let build = candidates(&registry).remove(0);
    // A new build: its surface is the one the tool reads.
    registry.builds.clear();
    let surface_bytes = surface.to_string().into_bytes();
    let data = body.map(|body| {
        let dir = target.path().join("build-data");
        fs::create_dir_all(&dir).unwrap();
        (
            build.commit.clone(),
            Ok(Rc::new(build_data(&dir, &build.commit, body))),
        )
    });
    let mut tool = Tool {
        root: target.path(),
        downloads: PackageOnly(body_bytes),
        packages: BTreeMap::new(),
        sdks: BTreeMap::from([(
            build.commit.clone(),
            Ok(Rc::new(Sdk {
                index: index(&build.commit, Manifest::parse(&manifest).unwrap().api),
                surface: Ok(Rc::new(Surface::read(&surface_bytes).unwrap())),
                surface_bytes,
            })),
        )]),
        data: data.into_iter().collect(),
    };
    let output = target.path().join("fit");
    write_rows(&registry, from_ref(&build), &mut tool, &output)?;
    let row = written(&output, "Ab3dE9xQ").release.remove(0);
    assert_eq!(row.rules, RULES);
    let (fits, does_not_fit) = (
        row.fits.contains_key(&build.id),
        row.does_not_fit.contains_key(&build.id),
    );
    assert!(fits != does_not_fit, "one verdict per cell");
    Ok(fits)
}

fn empty_surface() -> serde_json::Value {
    serde_json::json!({"schema": mod_surface::SCHEMA, "hash": "s"})
}

#[test]
fn a_package_whose_list_cannot_show_it_fits_is_stored_as_not_fitting() {
    // No list: it works on the build it was made for only, as the launcher builds it.
    assert_eq!(stored("", &[], empty_surface(), None), Ok(false));
}

#[test]
fn game_code_off_its_pin_is_compared_with_the_build_data() {
    let native = format!(
        "mixins='{}'\n[[native.mixin]]\nid='m'\ntarget='{TARGET}'\nkind='HEAD'\nhelper='src/m.rs'\n",
        "e".repeat(40)
    );
    let list = serde_json::json!({
        "schema": USES_SCHEMA, "made_for": MADE_FOR,
        "entries": [{"id": format!("game:fn:{TARGET}"), "hash": "sig:body"}],
    })
    .to_string();
    let files: &[(&str, &[u8])] = &[("src/m.rs", b"{}"), (USES_FILE, list.as_bytes())];
    assert_eq!(
        stored(&native, files, empty_surface(), Some("body")),
        Ok(true)
    );
    // A replacement written for the old body could undo what the game changed in it.
    assert_eq!(
        stored(&native, files, empty_surface(), Some("another body")),
        Ok(false)
    );
}

#[test]
fn hook_versions_the_build_it_was_made_for_cannot_give_fail_the_run() {
    let list = serde_json::json!({
        "schema": 1, "made_for": MADE_FOR,
        "entries": [{"id": "hook:joined", "hash": "h1"}],
    })
    .to_string();
    let surface = serde_json::json!({
        "schema": mod_surface::SCHEMA, "hash": "s",
        "hooks": [{
            "id": "hook:joined", "hash": "h1", "engine": {"target": "t"}, "call": "c",
            "version": 1, "side": "server", "contract": "listen", "cancellable": false,
            "arguments": [],
        }],
    });
    let error = stored("", &[(USES_FILE, list.as_bytes())], surface, None).unwrap_err();
    assert!(
        error.contains("cannot judge") && error.contains(MADE_FOR),
        "{error}"
    );
}
