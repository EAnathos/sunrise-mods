use serde_json::{Value, json};

use super::*;

struct Recorded {
    list: Value,
    tags: BTreeMap<String, Commit>,
    /// Build IDs whose signed descriptor names this commit instead of the metadata's.
    signed: BTreeMap<String, String>,
}

impl Releases for Recorded {
    fn releases(&self) -> Result<Vec<GithubRelease>, String> {
        serde_json::from_value(self.list.clone()).map_err(|e| e.to_string())
    }

    fn stable_commits(&self) -> Result<BTreeMap<String, Commit>, String> {
        Ok(self.tags.clone())
    }

    fn signed_commit(
        &self,
        release: &GithubRelease,
        build: &str,
        _channel: Option<Channel>,
    ) -> Result<String, String> {
        if let Some(commit) = self.signed.get(build) {
            return Ok(commit.clone());
        }
        // By default the signed descriptor agrees with the release's metadata.
        Ok(match self.tags.get(&release.tag_name) {
            Some(commit) => commit.as_str().to_owned(),
            None => release.target_commitish.clone(),
        })
    }
}

#[test]
fn offered_builds_keep_every_stable_and_ten_newest_with_surfaces_per_channel() {
    let mut rows = Vec::new();
    let mut tags = BTreeMap::new();
    for i in 1..=14 {
        let tag = format!("26.3-0.1.{i}");
        rows.push(
            json!({"tag_name":tag, "target_commitish":"release/26.3", "name":"Stable",
            "published_at":"2026-11-23T00:00:00Z", "draft":false, "prerelease":false, "assets":[]}),
        );
        tags.insert(tag, Commit::parse(format!("{i:040x}")).unwrap());
        for (channel, title, sequence) in [("nightly", "Nightly", i), ("dev", "Dev", i + 100)] {
            let date = format!("2026-11-{i:02}");
            let short = format!("{sequence:07x}");
            let commit = format!("{short}{}", "0".repeat(33));
            let tag = if channel == "dev" {
                format!("dev-{date}-{short}")
            } else if i == 12 {
                "nightly".to_owned()
            } else {
                format!("nightly-{date}")
            };
            rows.push(json!({"tag_name":tag, "target_commitish":commit, "name":format!("{title} {date} · {short}"),
                "published_at":format!("{date}T00:00:00Z"), "draft":i == 13, "prerelease":true, "assets":[{"name":"sdk.json","size":1}]}));
            rows.push(json!({"tag_name":format!("sdk-{commit}"), "target_commitish":commit, "name":"Mod SDK",
                "published_at":format!("{date}T00:00:00Z"), "draft":false, "prerelease":true,
                "assets":[{"name":if i == 14 { "sdk.json" } else { "surface.json" }, "size":1}]}));
        }
    }
    // Deliberately not in newest-first order, like a GitHub list sorted by creation time.
    let considered = select(&Recorded {
        list: json!(rows),
        tags,
        signed: BTreeMap::new(),
    })
    .unwrap();
    let mut selected: Vec<_> = considered
        .iter()
        .filter_map(|row| match &row.outcome {
            Outcome::Offered(build) => Some(build),
            Outcome::Declined(_) => None,
        })
        .collect();
    selected.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    for (channel, offset) in [("nightly", 0), ("dev", 100)] {
        let wanted: Vec<_> = (3..=12)
            .map(|i| format!("{channel}-2026-11-{i:02}-{:07x}", i + offset))
            .collect();
        let got: Vec<_> = selected
            .iter()
            .map(|row| row.id.as_str())
            .filter(|id| id.starts_with(channel))
            .collect();
        assert_eq!(got, wanted);
    }
    assert_eq!(
        selected
            .iter()
            .filter(|row| row.id.as_str().starts_with("26.3-"))
            .count(),
        14
    );
    // Every published release with an SDK descriptor is accounted for, so the timer asks about
    // each only once. The draft is not: it is new when it is published.
    let declined = |tag: &str| {
        let row = considered.iter().find(|row| row.tag == tag).unwrap();
        match &row.outcome {
            Outcome::Declined(why) => why.clone(),
            Outcome::Offered(build) => panic!("{tag} offered as {}", build.id.as_str()),
        }
    };
    assert!(declined("nightly-2026-11-02").contains("ten newest"));
    assert!(declined("dev-2026-11-14-0000072").contains("no SDK release"));
    assert!(declined(&format!("sdk-{:07x}{}", 14, "0".repeat(33))).contains("not a game build"));
    assert!(
        !considered
            .iter()
            .any(|row| row.tag == "dev-2026-11-13-0000071")
    );
}

#[test]
fn build_whose_signed_descriptor_names_another_commit_is_refused() {
    // Both commits have a published SDK with a surface; only the game release's own signed
    // descriptor says which one the build is.
    let made_from = format!("{:040x}", 1);
    let moved_to = format!("{:040x}", 2);
    let short = &moved_to[..7];
    let sdk = |commit: &str| {
        json!({"tag_name":format!("sdk-{commit}"), "target_commitish":commit, "name":"Mod SDK",
            "published_at":"2026-11-01T00:00:00Z", "draft":false, "prerelease":true,
            "assets":[{"name":"surface.json","size":1}]})
    };
    let stable = json!({"tag_name":"26.3-0.1.0", "target_commitish":"release/26.3", "name":"Stable",
        "published_at":"2026-11-23T00:00:00Z", "draft":false, "prerelease":false, "assets":[]});
    let dev = json!({"tag_name":format!("dev-2026-11-02-{short}"), "target_commitish":moved_to,
        "name":format!("Dev 2026-11-02 · {short}"), "published_at":"2026-11-02T00:00:00Z",
        "draft":false, "prerelease":true, "assets":[]});
    for (release, id) in [
        (stable, "26.3-0.1.0".to_owned()),
        (dev, format!("dev-2026-11-02-{short}")),
    ] {
        let source = |signed: &str| Recorded {
            list: json!([release, sdk(&made_from), sdk(&moved_to)]),
            tags: BTreeMap::from([(
                "26.3-0.1.0".to_owned(),
                Commit::parse(moved_to.clone()).unwrap(),
            )]),
            signed: BTreeMap::from([(id.clone(), signed.to_owned())]),
        };
        let considered = select(&source(&moved_to)).unwrap();
        assert!(matches!(&considered[..], [row] if matches!(row.outcome, Outcome::Offered(_))));
        let error = select(&source(&made_from)).unwrap_err();
        assert!(error.contains("names another commit"), "{error}");
    }
}
