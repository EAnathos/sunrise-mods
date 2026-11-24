//! What a reviewer is told about a pull request that moves a project to another repository.
use std::path::Path;

use crate::{
    github::{GitHub, Repo},
    registry,
    schema::{Project, ProjectId},
};

/// One Markdown line for each project whose `repository` differs between `base` and `head`:
/// whether the two repositories belong to the same GitHub account, by the account's numeric ID.
/// A move to another account is allowed and labelled; a move whose repositories cannot be
/// looked up is an error, because the reviewer would be told nothing. Empty when nothing moves.
pub fn report(base: &Path, head: &Path, github: &dyn GitHub) -> Result<String, String> {
    let after = registry::load(head).map_err(|e| e.join("\n"))?;
    let mut lines = String::new();
    for (id, now) in &after.projects {
        let file = base
            .join("registry/projects")
            .join(id.as_str())
            .join("project.toml");
        let before = match std::fs::read_to_string(&file) {
            Ok(text) => toml::from_str::<Project>(&text)
                .map_err(|error| format!("{}: {error}", file.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{}: {error}", file.display())),
        };
        if before.repository == now.project.repository {
            continue;
        }
        let lookup = |repository| {
            github
                .repository(Repo::Registered(repository))
                .map_err(|error| {
                    format!(
                        "{}: repository {} could not be looked up, so its owner is unknown: {error}",
                        id.as_str(),
                        repository.get()
                    )
                })
        };
        let (from, to) = (lookup(before.repository)?, lookup(now.project.repository)?);
        lines.push_str(&line(id, &from, &to));
    }
    Ok(lines)
}

fn line(
    id: &ProjectId,
    from: &crate::github::Repository,
    to: &crate::github::Repository,
) -> String {
    let moves = format!(
        "- `{}` moves from `{}` to `{}`",
        id.as_str(),
        from.name.as_str(),
        to.name.as_str()
    );
    if from.owner.id == to.owner.id {
        format!(
            "{moves}: **same owner** (`{}`, account {}).\n",
            to.owner.login, to.owner.id
        )
    } else {
        format!(
            "{moves}: **DIFFERENT OWNER**, from `{}` (account {}) to `{}` (account {}). \
             Players who have it installed are asked to confirm the new owner.\n",
            from.owner.login, from.owner.id, to.owner.login, to.owner.id
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;
    use crate::github::{ReleaseBranches, Repository};

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/two-ports");
    const PROJECT: &str = "registry/projects/Ab3dE9xQ/project.toml";

    /// Repository 123456789 is the fixture's `Example/Wayfinder-Renamed`; 987654321 is its
    /// `other/port-collection`. `collection_owner` is the account that owns the second.
    struct Owners {
        collection_owner: u64,
    }

    impl GitHub for Owners {
        fn repository(&self, repo: Repo<'_>) -> Result<Repository, String> {
            let (id, name, owner) = match repo {
                Repo::Registered(id) if id.get() == 123456789 => {
                    (123456789, "Example/Wayfinder-Renamed", 7)
                }
                Repo::Registered(id) if id.get() == 987654321 && self.collection_owner != 0 => {
                    (987654321, "other/port-collection", self.collection_owner)
                }
                _ => return Err("HTTP 404".into()),
            };
            serde_json::from_value(serde_json::json!({
                "id": id, "full_name": name, "private": false,
                "owner": {"id": owner, "login": name.split_once('/').unwrap().0},
            }))
            .map_err(|e| e.to_string())
        }

        fn releases(&self, _: Repo<'_>) -> Result<Vec<crate::github::Release>, String> {
            unreachable!()
        }

        fn release_branches(&self) -> Result<ReleaseBranches, String> {
            unreachable!()
        }

        fn tag_commit(&self, _: Repo<'_>, _: &str) -> Result<String, String> {
            unreachable!()
        }
    }

    /// The fixture as the base, and a copy of it whose first project has moved to the
    /// repository the second project already uses, as a move's pull request leaves it: the
    /// old repository's file is gone and the project's release row still names it.
    fn moved() -> (PathBuf, tempfile::TempDir) {
        let head = tempfile::tempdir().unwrap();
        for entry in walkdir(Path::new(FIXTURE).join("registry")) {
            let to = head.path().join(entry.strip_prefix(FIXTURE).unwrap());
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::copy(&entry, to).unwrap();
        }
        let file = head.path().join(PROJECT);
        let text = fs::read_to_string(&file).unwrap();
        let there = text.replace("repository = 123456789", "repository = 987654321");
        assert_ne!(text, there);
        fs::write(file, there).unwrap();
        let repositories = head.path().join("registry/repositories");
        fs::remove_file(repositories.join("123456789.toml")).unwrap();
        let file = repositories.join("987654321.toml");
        let listed = fs::read_to_string(&file).unwrap()
            + "\n[[package]]\npath = \".\"\nproject = \"Ab3dE9xQ\"\n";
        fs::write(file, listed).unwrap();
        (FIXTURE.into(), head)
    }

    fn walkdir(dir: PathBuf) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .flat_map(|entry| {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walkdir(path)
                } else {
                    vec![path]
                }
            })
            .collect()
    }

    #[test]
    fn nothing_is_said_when_no_project_moves() {
        let github = Owners {
            collection_owner: 0,
        };
        assert_eq!(
            report(Path::new(FIXTURE), Path::new(FIXTURE), &github).unwrap(),
            ""
        );
    }

    #[test]
    fn a_move_says_whether_the_owner_account_is_the_same() {
        let (base, head) = moved();
        let same = report(
            &base,
            head.path(),
            &Owners {
                collection_owner: 7,
            },
        )
        .unwrap();
        assert_eq!(
            same,
            "- `Ab3dE9xQ` moves from `Example/Wayfinder-Renamed` to `other/port-collection`: \
             **same owner** (`other`, account 7).\n"
        );
        let different = report(
            &base,
            head.path(),
            &Owners {
                collection_owner: 8,
            },
        )
        .unwrap();
        assert_eq!(
            different,
            "- `Ab3dE9xQ` moves from `Example/Wayfinder-Renamed` to `other/port-collection`: \
             **DIFFERENT OWNER**, from `Example` (account 7) to `other` (account 8). \
             Players who have it installed are asked to confirm the new owner.\n"
        );
    }

    #[test]
    fn a_move_whose_repository_cannot_be_looked_up_is_an_error() {
        let (base, head) = moved();
        let error = report(
            &base,
            head.path(),
            &Owners {
                collection_owner: 0,
            },
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Ab3dE9xQ: repository 987654321 could not be looked up, so its owner is unknown: HTTP 404"
        );
    }
}
