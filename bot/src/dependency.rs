//! Which listed releases have a required dependency that nothing listed can satisfy on a build.
//! Derived from the rows and the stored verdicts each time the index is assembled: the answer
//! changes whenever another project's rows change, so it is never stored.
use std::collections::BTreeSet;

use sunrise_dependency_rule::{Offer, Requirement, Revoked, satisfies};

use crate::schema::{Dependency, ModId, ProjectId, Release, Revocation};

pub fn revocations(rows: &[Revocation]) -> Vec<Revoked<'_>> {
    rows.iter()
        .map(|row| Revoked {
            project: row.project.as_str(),
            sha256: row.sha256.as_ref().map(|sha256| sha256.as_str()),
        })
        .collect()
}

/// Whether `release` of another project's `provider` satisfies `dependency`, by the rule the
/// launcher's install planner shares.
pub fn satisfied_by(
    dependency: &Dependency,
    provider: &ProjectId,
    mod_id: &ModId,
    release: &Release,
    revoked: &[Revoked<'_>],
) -> bool {
    satisfies(
        &Requirement {
            mod_id: dependency.mod_id.as_str(),
            range: &dependency.version,
            project: dependency.project.as_ref().map(|pinned| pinned.as_str()),
        },
        &Offer {
            project: provider.as_str(),
            mod_id: mod_id.as_str(),
            version: &release.version,
            sha256: release.file.sha256.as_str(),
        },
        revoked,
    )
}

/// A release the index lists, with its own verdict on each offered build, in the builds' order.
pub struct Listed<'a> {
    pub project: &'a ProjectId,
    pub mod_id: &'a ModId,
    pub release: &'a Release,
    pub fits: Vec<Option<bool>>,
}

/// For each listed release, in order: the build (by position) and in-game ID of every direct
/// required dependency that no listed release of another project can satisfy on that build,
/// for the builds the release itself fits.
///
/// A provider counts on a build only if it fits there and its own required dependencies can be
/// satisfied in turn. A provider not judged yet counts as fitting, so a release appears here
/// only when no verdict still to come could make it installable; releases that depend on each
/// other count as satisfying each other.
pub fn unavailable<'a>(
    listed: &[Listed<'a>],
    revoked: &[Revocation],
) -> Vec<Vec<(usize, &'a ModId)>> {
    let revoked = revocations(revoked);
    let builds = listed.first().map_or(0, |release| release.fits.len());
    let mut unavailable = vec![Vec::new(); listed.len()];
    for build in 0..builds {
        let mut possible: BTreeSet<usize> = (0..listed.len())
            .filter(|&release| listed[release].fits[build] != Some(false))
            .collect();
        let missing = |possible: &BTreeSet<usize>, release: usize| {
            let dependent = &listed[release];
            dependent
                .release
                .dependencies
                .iter()
                .filter(|dependency| dependency.required)
                .filter(|dependency| {
                    !possible.iter().any(|&provider| {
                        let provider = &listed[provider];
                        provider.project != dependent.project
                            && satisfied_by(
                                dependency,
                                provider.project,
                                provider.mod_id,
                                provider.release,
                                &revoked,
                            )
                    })
                })
                .map(|dependency| &dependency.mod_id)
                .collect::<Vec<&'a ModId>>()
        };
        // The greatest fixed point: start from every release that may fit and drop those a
        // required dependency rules out, until none is dropped.
        while let Some(&dropped) = possible
            .iter()
            .find(|&&release| !missing(&possible, release).is_empty())
        {
            possible.remove(&dropped);
        }
        for (release, entries) in unavailable.iter_mut().enumerate() {
            if listed[release].fits[build] == Some(true) && !possible.contains(&release) {
                entries.extend(
                    missing(&possible, release)
                        .into_iter()
                        .map(|id| (build, id)),
                );
            }
        }
    }
    unavailable
}

#[cfg(test)]
mod tests;
