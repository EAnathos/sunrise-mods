use super::*;

/// One listed release: project, in-game ID, version, required and optional dependencies as
/// `(mod, range, required)`, and its own verdict on each build.
type Row<'a> = (
    &'a str,
    &'a str,
    &'a str,
    &'a [(&'a str, &'a str, bool)],
    &'a [Option<bool>],
);

const FITS: &[Option<bool>] = &[Some(true)];
const DOES_NOT_FIT: &[Option<bool>] = &[Some(false)];
const NOT_JUDGED: &[Option<bool>] = &[None];

fn sha256(project: &str, version: &str) -> String {
    sunrise_release_sign::sha256_hex(format!("{project} {version}").as_bytes())
}

/// What `unavailable` answers for `rows`: per row, the `(build, mod)` pairs.
fn answer(rows: &[Row<'_>], revoked: &str) -> Vec<Vec<(usize, String)>> {
    let parsed: Vec<(ProjectId, ModId, Release)> = rows
        .iter()
        .map(|(project, mod_id, version, dependencies, _)| {
            let dependencies: String = dependencies
                .iter()
                .map(|(mod_id, range, required)| {
                    format!(
                        "[[dependencies]]\nmod = '{mod_id}'\nversion = '{range}'\nrequired = {required}\n"
                    )
                })
                .collect();
            let asset = |name: &str| {
                format!(
                    "url = 'https://example.com/{name}'\nsize = 1\nsha256 = '{}'\nattestation = 'https://example.com/{name}.sigstore.json'\n",
                    sha256(project, version)
                )
            };
            let row = format!(
                "version = '{version}'\nkind = 'mod'\ntag = 'v{version}'\npublished = '2026-11-24T18:46:10Z'\nsunrise = '26.3-0.1.0'\nworkflow = 'sunrise-org/sunrise-mods/.github/workflows/release.yml@refs/tags/release-v1'\nrepository = 7\n[file]\n{}[page]\n{}{dependencies}",
                asset("mod.smod"),
                asset("mod.page.tar"),
            );
            (
                ProjectId::parse((*project).into()).unwrap(),
                ModId::parse((*mod_id).into()).unwrap(),
                toml::from_str(&row).unwrap(),
            )
        })
        .collect();
    let listed: Vec<Listed<'_>> = parsed
        .iter()
        .zip(rows)
        .map(|((project, mod_id, release), row)| Listed {
            project,
            mod_id,
            release,
            fits: row.4.to_vec(),
        })
        .collect();
    let revoked: crate::schema::Revocations = toml::from_str(revoked).unwrap();
    unavailable(&listed, &revoked.revoked)
        .into_iter()
        .map(|entries| {
            entries
                .into_iter()
                .map(|(build, mod_id)| (build, mod_id.as_str().to_owned()))
                .collect()
        })
        .collect()
}

fn missing(mod_id: &str) -> Vec<(usize, String)> {
    vec![(0, mod_id.to_owned())]
}

const NEEDS_BASE: &[(&str, &str, bool)] = &[("base", "^1", true)];
const BASE: &str = "Ab3dE9xQ";
const USER: &str = "Zz91kLmN";
const OTHER: &str = "Qm4tZ81c";

#[test]
fn a_release_whose_dependency_does_not_fit_the_build_names_it() {
    let rows: &[Row<'_>] = &[
        (BASE, "base", "1.0.0", &[], DOES_NOT_FIT),
        (USER, "user", "1.0.0", NEEDS_BASE, FITS),
    ];
    assert_eq!(answer(rows, ""), [vec![], missing("base")]);
}

#[test]
fn a_chain_names_the_direct_dependency_at_every_level() {
    let rows: &[Row<'_>] = &[
        (BASE, "base", "1.0.0", &[], DOES_NOT_FIT),
        (USER, "user", "1.0.0", NEEDS_BASE, FITS),
        (OTHER, "top", "1.0.0", &[("user", "*", true)], FITS),
    ];
    assert_eq!(answer(rows, ""), [vec![], missing("base"), missing("user")]);
}

#[test]
fn releases_that_need_each_other_satisfy_each_other() {
    let rows: &[Row<'_>] = &[
        (BASE, "base", "1.0.0", &[("user", "*", true)], FITS),
        (USER, "user", "1.0.0", NEEDS_BASE, FITS),
    ];
    assert_eq!(answer(rows, ""), [vec![], vec![]]);
}

#[test]
fn a_range_no_listed_release_is_in_is_unavailable() {
    let rows: &[Row<'_>] = &[
        (BASE, "base", "2.0.0", &[], FITS),
        (USER, "user", "1.0.0", NEEDS_BASE, FITS),
    ];
    assert_eq!(answer(rows, ""), [vec![], missing("base")]);
}

#[test]
fn a_revoked_project_or_file_provides_nothing() {
    let rows: &[Row<'_>] = &[
        (BASE, "base", "1.0.0", &[], FITS),
        (USER, "user", "1.0.0", NEEDS_BASE, FITS),
    ];
    let whole = format!("[[revoked]]\nproject = '{BASE}'\nreason = 'x'\ndate = '2026-11-24'\n");
    let file = format!("{whole}sha256 = '{}'\n", sha256(BASE, "1.0.0"));
    assert_eq!(answer(rows, &whole)[1], missing("base"));
    assert_eq!(answer(rows, &file)[1], missing("base"));
}

#[test]
fn a_provider_not_judged_yet_leaves_the_release_out() {
    let rows: &[Row<'_>] = &[
        (BASE, "base", "1.0.0", &[], NOT_JUDGED),
        (USER, "user", "1.0.0", NEEDS_BASE, FITS),
    ];
    assert_eq!(answer(rows, ""), [vec![], vec![]]);
}

#[test]
fn one_fitting_provider_among_several_is_enough() {
    let rows: &[Row<'_>] = &[
        (BASE, "base", "1.0.0", &[], DOES_NOT_FIT),
        (OTHER, "base", "1.3.0", &[], FITS),
        (USER, "user", "1.0.0", NEEDS_BASE, FITS),
    ];
    assert_eq!(answer(rows, ""), [vec![], vec![], vec![]]);
}

#[test]
fn an_optional_dependency_and_a_build_the_release_does_not_fit_add_nothing() {
    let optional: &[Row<'_>] = &[(USER, "user", "1.0.0", &[("base", "^1", false)], FITS)];
    assert_eq!(answer(optional, ""), [vec![]]);
    let rows: &[Row<'_>] = &[
        (
            BASE,
            "base",
            "1.0.0",
            &[],
            &[Some(false), Some(false), Some(true)],
        ),
        (
            USER,
            "user",
            "1.0.0",
            NEEDS_BASE,
            &[Some(true), Some(false), None],
        ),
    ];
    assert_eq!(answer(rows, ""), [vec![], missing("base")]);
}

#[test]
fn a_release_cannot_supply_its_own_projects_dependency() {
    let rows: &[Row<'_>] = &[
        (BASE, "base", "1.0.0", &[], FITS),
        (BASE, "base", "1.1.0", NEEDS_BASE, FITS),
    ];
    assert_eq!(answer(rows, ""), [vec![], missing("base")]);
}
