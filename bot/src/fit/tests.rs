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

    fn judge(&mut self, release: &Release, build: &Offered) -> Result<Option<bool>, String> {
        let key = (release.file.sha256.clone(), build.id.clone());
        self.calls.push(key.clone());
        self.responses
            .get(&key)
            .cloned()
            .unwrap_or(Ok(false))
            .map(Some)
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
fn a_release_not_yet_merged_is_judged_on_every_offered_build_or_refused() {
    let target = scratch();
    let registry = judged(target.path());
    let release = registry
        .projects
        .values()
        .next()
        .unwrap()
        .releases
        .values()
        .next()
        .unwrap();
    let builds = &registry.builds;
    assert!(builds.len() >= 2);
    let key = |build: &Offered| (release.file.sha256.clone(), build.id.clone());
    let mut judge = Recorded::default();
    judge.responses.insert(key(&builds[1]), Ok(true));
    let judged = cells(release, builds, &mut judge).unwrap();
    assert_eq!(judged.fits, [&builds[1].id]);
    assert_eq!(judged.does_not_fit.len(), builds.len() - 1);
    assert!(!judged.does_not_fit.contains(&&builds[1].id));

    judge
        .responses
        .insert(key(&builds[0]), Err("recorded missing surface".into()));
    assert_eq!(
        cells(release, builds, &mut judge),
        Err(format!(
            "cannot judge on:\n{}: recorded missing surface",
            builds[0].id.as_str()
        ))
    );
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

/// What the bot makes of a package of `files` (after a manifest with `native`) on the fixture's
/// first build, whose mod tool is the stand-in `tests/fixtures/native-check/cargo-sunrise-mod`
/// when `tool` is set. Only the package can be downloaded.
fn judged_by_tool(
    native: &str,
    files: &[(&str, &[u8])],
    tool: bool,
) -> Result<Option<bool>, String> {
    struct PackageOnly(Vec<u8>);
    impl Download for PackageOnly {
        fn get(&self, url: &str, file: &Path, _limit: u64) -> Result<(), String> {
            assert!(url.ends_with(".smod"), "unexpected SDK download: {url}");
            fs::write(file, &self.0).map_err(|e| e.to_string())
        }
    }

    let manifest = format!("id='wayfinder'\nname='Wayfinder'\nversion='1.2.0'\n{native}");
    let body = Package::write_files(&manifest, files).unwrap();
    let mut registry = registry::load(fixture()).unwrap();
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
    let target = scratch();
    let build = Offered {
        surface: Sha256::parse(sha256_hex(&[])).unwrap(),
        ..offered(&candidates(&judged(target.path()))[0], "1")
    };
    Tool {
        root: target.path(),
        downloads: PackageOnly(body),
        packages: BTreeMap::new(),
        sdks: BTreeMap::from([(
            build.commit.clone(),
            Ok(Rc::new(Sdk {
                index: index(&build.commit, Manifest::parse(&manifest).unwrap().api),
                surface_bytes: vec![],
                surface: Ok(()),
                tool: tool.then(|| {
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/native-check/cargo-sunrise-mod")
                }),
            })),
        )]),
    }
    .judge(release, &build)
}

const NATIVE: &str = "[native]\ngame='26.3'\nsunrise='dddddddddddddddddddddddddddddddddddddddd'\n";

/// A native package whose stand-in tool answers `reply`, and `supplied` once given the surface.
fn native(reply: &str, supplied: &str) -> Result<Option<bool>, String> {
    judged_by_tool(
        NATIVE,
        &[
            ("Cargo.toml", b"[package]\nname='wayfinder'\n"),
            ("src/lib.rs", b"pub fn read_map() {}\n"),
            ("src/reply.rs", reply.as_bytes()),
            ("src/supplied.rs", supplied.as_bytes()),
        ],
        true,
    )
}

fn checked(mod_reply: &str) -> String {
    format!(r#"0 {{"contract":2,"route":null,"mods":[{{"id":"wayfinder",{mod_reply}}}]}}"#)
}

#[test]
fn a_native_package_fits_where_the_builds_own_tool_says_it_is_usable() {
    for (verdict, fits) in [
        (r#"{"kind":"made-for"}"#, true),
        (r#"{"kind":"fits","made_for":"d"}"#, true),
        (r#"{"kind":"no-list","made_for":"d"}"#, false),
        (
            r#"{"kind":"exact","made_for":"d","why":"internals"}"#,
            false,
        ),
        (
            r#"{"kind":"does-not-fit","made_for":"d","misfits":[]}"#,
            false,
        ),
    ] {
        let reply = checked(&format!(
            r#""needs":null,"refusal":null,"verdict":{verdict}"#
        ));
        assert_eq!(native(&reply, ""), Ok(Some(fits)), "{verdict}");
    }
}

/// The page entries of sunrise#5973: an older build's tool refuses the whole package.
#[test]
fn a_package_the_builds_own_tool_cannot_read_or_refuses_does_not_fit() {
    assert_eq!(
        native(
            r#"1 {"contract":2,"error":{"kind":"unread","message":"unexpected package path: page.toml"}}"#,
            ""
        ),
        Ok(Some(false))
    );
    assert_eq!(
        native(&checked(r#""refusal":"wayfinder needs a newer game""#), ""),
        Ok(Some(false))
    );
}

#[test]
fn the_tool_is_given_the_surface_it_asks_for_once() {
    let needs = checked(r#""needs":"surface""#);
    let fits = checked(r#""verdict":{"kind":"fits","made_for":"d"}"#);
    assert_eq!(native(&needs, &fits), Ok(Some(true)));
    assert!(native(&needs, &needs).is_err());
}

#[test]
fn an_answer_that_is_no_reply_is_an_error_not_a_verdict() {
    for reply in [
        "2 no plan",
        r#"0 {"contract":2,"error":{"kind":"unread","message":"m"}}"#,
        r#"0 {"contract":2,"route":null,"mods":[]}"#,
        &checked(r#""needs":null,"refusal":null,"verdict":null"#),
    ] {
        assert!(native(reply, "").is_err(), "{reply}");
    }
}

#[test]
fn a_native_package_gets_no_verdict_on_a_build_without_a_tool_for_this_computer() {
    let files: &[(&str, &[u8])] = &[
        ("Cargo.toml", b"[package]\nname='wayfinder'\n"),
        ("src/lib.rs", b"pub fn read_map() {}\n"),
    ];
    assert_eq!(judged_by_tool(NATIVE, files, false), Ok(None));
    // A content-only package needs no tool.
    assert_eq!(judged_by_tool("", &[], false), Ok(Some(true)));
}

#[test]
fn a_cell_without_a_verdict_is_stored_in_neither_list() {
    struct Unjudged;
    impl Judge for Unjudged {
        fn surface(&mut self, build: &Build) -> Result<Option<Sha256>, String> {
            Ok(Some(offered(build, "9").surface))
        }
        fn judge(&mut self, _: &Release, _: &Offered) -> Result<Option<bool>, String> {
            Ok(None)
        }
    }
    let target = scratch();
    let registry = registry::load(fixture()).unwrap();
    let builds = candidates(&judged(target.path()));
    let output = target.path().join("fit");
    write_rows(&registry, &builds, &mut Unjudged, &output).unwrap();
    let row = &written(&output, "Ab3dE9xQ").release[0];
    assert!(row.fits.is_empty() && row.does_not_fit.is_empty());
}
