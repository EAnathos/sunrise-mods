use super::*;
use std::fs;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/signed-index");

#[test]
fn minisign_fixture_round_trips_over_exact_index_bytes() {
    let root = Path::new(FIXTURE);
    verify(
        &root.join("index.json"),
        &root.join("index.json.minisig"),
        &root.join("index-key.pub"),
    )
    .unwrap();
}

#[test]
fn minisign_refuses_an_index_with_modified_whitespace() {
    let root = Path::new(FIXTURE);
    let path = root.join("index.json");
    let mut body = fs::read(path).unwrap();
    body.push(b' ');
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target");
    fs::create_dir_all(&target).unwrap();
    let tampered = tempfile::NamedTempFile::new_in(target).unwrap();
    fs::write(tampered.path(), body).unwrap();
    let error = verify(
        tampered.path(),
        &root.join("index.json.minisig"),
        &root.join("index-key.pub"),
    )
    .err()
    .unwrap();
    assert!(
        error.contains("index signature verification failed"),
        "{error}"
    );
}
