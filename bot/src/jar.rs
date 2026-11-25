//! The release check of a package's original Java jars (`[[jar]]`): each is on Modrinth with
//! the exact bytes the manifest names, and the mod's resources resolve with the jars beneath
//! them. A jar lives in the check's working directory only: the registry stores and publishes
//! none.

use std::{io::Read, path::Path};

use sunrise_jar_fetch::{Http, MAX_JAR, Modrinth};
use sunrise_mod_package::{
    Content, Manifest,
    content::{JarLayer, check_references},
};

use crate::github;

const MODRINTH: Modrinth<'static> = Modrinth {
    api: "https://api.modrinth.com/v2",
    cdn_host: "cdn.modrinth.com",
};

/// The largest answer read from Modrinth's API (16 MiB), `MAX_API_RESPONSE` of
/// `sunrise-jar-fetch`, which reads no more of it.
const MAX_ANSWER: u64 = 16 << 20;

/// Modrinth asks every client to name itself (docs.modrinth.com, "User Agents").
const AGENT: [&str; 2] = ["--user-agent", "sunrise-org/sunrise-mods registry-bot"];

/// curl with no redirects followed, as [`Http`] requires. A server error, a rate limit or a
/// broken connection marks the run as unchecked ([`github::was_unavailable`]).
pub struct Curl;

impl Curl {
    fn answer(options: &[&str], url: &str, limit: u64) -> Result<Option<Box<dyn Read>>, String> {
        let body = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        if !github::transfer(options, url, body.path(), limit)? {
            return Ok(None);
        }
        Ok(Some(Box::new(body.reopen().map_err(|e| e.to_string())?)))
    }
}

impl Http for Curl {
    fn get(&self, url: &str) -> Result<Option<Box<dyn Read>>, String> {
        Self::answer(&AGENT, url, MAX_JAR)
    }

    fn post_json(&self, url: &str, body: &str) -> Result<Box<dyn Read>, String> {
        let [agent, name] = AGENT;
        let options = [
            agent,
            name,
            "--header",
            "Content-Type: application/json",
            "--data-binary",
            body,
        ];
        Self::answer(&options, url, MAX_ANSWER)?.ok_or_else(|| format!("{url}: HTTP 404"))
    }
}

/// Fetches every `[[jar]]` of `manifest` from Modrinth into `work` and checks the references of
/// the package at `smod` against its own files and those jars together.
pub fn check(smod: &Path, manifest: &Manifest, work: &Path, http: &dyn Http) -> Result<(), String> {
    let mut layer = JarLayer::new();
    for jar in &manifest.jar {
        let named = |error: String| {
            format!(
                "original jar `{} {}` (SHA-512 {}): {error}",
                jar.id(),
                jar.version(),
                jar.sha512()
            )
        };
        let path = jar.cache_path(work);
        sunrise_jar_fetch::fetch(http, MODRINTH, jar.sha512(), &path).map_err(named)?;
        layer.add_archive(&path).map_err(named)?;
    }
    let content = Content::read(smod, manifest)?;
    let findings = check_references(&content, manifest, Some(&layer))?;
    if findings.errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the mod's resources do not resolve with its original jars beneath them:\n{}",
            findings.errors.join("\n")
        ))
    }
}

#[cfg(test)]
mod tests;
