//! Read-only GitHub boundary for release observation and attestation verification.
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::{Deserialize, de::DeserializeOwned};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::schema::{RepositoryId, RepositoryName, SignerWorkflowRef};

#[derive(Deserialize)]
pub struct Repository {
    pub id: RepositoryId,
    #[serde(rename = "full_name")]
    pub name: RepositoryName,
    pub private: bool,
    pub owner: Owner,
}

/// The account a repository belongs to. Its ID stays the same when the account is renamed.
#[derive(Deserialize)]
pub struct Owner {
    pub id: u64,
    pub login: String,
}

#[derive(Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub name: Option<String>,
    pub published_at: Option<String>,
    pub draft: bool,
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn published(&self) -> Option<OffsetDateTime> {
        if self.draft {
            return None;
        }
        OffsetDateTime::parse(self.published_at.as_deref()?, &Rfc3339).ok()
    }

    pub fn asset(&self, name: &str) -> Result<&Asset, String> {
        let mut rows = self.assets.iter().filter(|row| row.name == name);
        match (rows.next(), rows.next()) {
            (Some(row), None) => Ok(row),
            _ => Err(format!("expected exactly one {name} asset")),
        }
    }
}

#[derive(Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub digest: Option<String>,
    pub browser_download_url: String,
}

#[derive(Clone, Copy)]
pub enum Repo<'a> {
    Named(&'a RepositoryName),
    Registered(RepositoryId),
}

impl Repo<'_> {
    fn endpoint(self) -> String {
        match self {
            Self::Named(name) => format!("repos/{}", name.as_str()),
            Self::Registered(id) => format!("repositories/{}", id.get()),
        }
    }
}

#[derive(Default)]
pub struct ReleaseBranches {
    /// Open release PRs that hold their project's later releases back until they are decided.
    pub waiting: BTreeSet<String>,
    pub handled: BTreeSet<String>,
}

impl ReleaseBranches {
    /// Records one of the bot's release PRs. An open or merged PR handles its version, and so does a
    /// closed, unmerged one while its branch still exists: that release was refused. One whose branch
    /// was deleted was withdrawn, and its release may be opened again. An open PR whose release
    /// check failed does not hold a later release back: the author's fix for it is that release.
    fn record(
        &mut self,
        branch: String,
        open: bool,
        check_failed: bool,
        merged: bool,
        branch_exists: bool,
    ) {
        if open && !check_failed {
            self.waiting.insert(branch.clone());
        }
        if open || merged || branch_exists {
            self.handled.insert(branch);
        }
    }
}

pub trait GitHub {
    fn repository(&self, repo: Repo<'_>) -> Result<Repository, String>;
    fn releases(&self, repo: Repo<'_>) -> Result<Vec<Release>, String>;
    fn release_branches(&self) -> Result<ReleaseBranches, String>;
    fn tag_commit(&self, repo: Repo<'_>, tag: &str) -> Result<String, String>;

    fn download_release(&self, url: &str, dest: &Path, limit: u64) -> Result<(), String> {
        download(url, dest, limit)
    }

    fn verify_attestation(
        &self,
        check: Attestation<'_>,
        file: &Path,
        bundle: &Path,
    ) -> Result<SignerWorkflowRef, Observation> {
        check.verify(file, bundle)
    }
}

pub struct Cli<'a> {
    pub registry: &'a RepositoryName,
}

impl GitHub for Cli<'_> {
    fn repository(&self, repo: Repo<'_>) -> Result<Repository, String> {
        api(&repo.endpoint())
    }

    fn releases(&self, repo: Repo<'_>) -> Result<Vec<Release>, String> {
        pages(&format!("{}/releases?per_page=100", repo.endpoint()))
    }

    fn release_branches(&self) -> Result<ReleaseBranches, String> {
        #[derive(Deserialize)]
        struct Pull {
            head: Head,
            state: String,
            merged_at: Option<String>,
            user: User,
        }
        #[derive(Deserialize)]
        struct Ref {
            #[serde(rename = "ref")]
            name: String,
        }
        #[derive(Deserialize)]
        struct User {
            login: String,
        }
        #[derive(Deserialize)]
        struct Head {
            #[serde(rename = "ref")]
            branch: String,
            sha: String,
            repo: Option<HeadRepo>,
        }
        #[derive(Deserialize)]
        struct Combined {
            statuses: Vec<Status>,
        }
        #[derive(Deserialize)]
        struct Status {
            context: String,
            state: String,
        }
        #[derive(Deserialize)]
        struct HeadRepo {
            full_name: RepositoryName,
        }
        let pulls: Vec<Pull> = pages(&format!(
            "repos/{}/pulls?state=all&per_page=100",
            self.registry.as_str()
        ))?;
        let kept: Vec<Ref> = pages(&format!(
            "repos/{}/git/matching-refs/heads/release/",
            self.registry.as_str()
        ))?;
        let mut branches = ReleaseBranches::default();
        for p in pulls {
            if p.user.login == "github-actions[bot]"
                && p.head.branch.starts_with("release/")
                && p.head.repo.as_ref().is_some_and(|r| {
                    r.full_name
                        .as_str()
                        .eq_ignore_ascii_case(self.registry.as_str())
                })
            {
                let exists = kept
                    .iter()
                    .any(|r| r.name.strip_prefix("refs/heads/") == Some(&*p.head.branch));
                let open = p.state == "open";
                // The newest status of each context on the PR's head.
                let check_failed = open && {
                    let combined: Combined = api(&format!(
                        "repos/{}/commits/{}/status",
                        self.registry.as_str(),
                        encode(&p.head.sha)
                    ))?;
                    combined.statuses.iter().any(|status| {
                        status.context == "registry-v2/release-check" && status.state == "failure"
                    })
                };
                branches.record(
                    p.head.branch,
                    open,
                    check_failed,
                    p.merged_at.is_some(),
                    exists,
                );
            }
        }
        Ok(branches)
    }

    fn tag_commit(&self, repo: Repo<'_>, tag: &str) -> Result<String, String> {
        #[derive(Deserialize)]
        struct Commit {
            sha: String,
        }
        // Fully qualify the ref, including when a branch has the same name. The commits
        // endpoint dereferences annotated tags to the commit the OIDC certificate names.
        let commit: Commit = api(&format!(
            "{}/commits/refs/tags/{}",
            repo.endpoint(),
            encode(tag)
        ))?;
        if !sunrise_mod_package::manifest::revision(&commit.sha) {
            return Err("GitHub returned an invalid tag commit".into());
        }
        Ok(commit.sha)
    }
}

pub fn encode(text: &str) -> percent_encoding::PercentEncode<'_> {
    const SEGMENT: &AsciiSet = &CONTROLS
        .add(b'%')
        .add(b'/')
        .add(b'?')
        .add(b'#')
        .add(b' ')
        .add(b'+');
    utf8_percent_encode(text, SEGMENT)
}

fn api<T: DeserializeOwned>(endpoint: &str) -> Result<T, String> {
    decode(&gh(&["api", "--hostname", "github.com", endpoint])?)
}

fn pages<T: DeserializeOwned>(endpoint: &str) -> Result<Vec<T>, String> {
    let pages: Vec<Vec<T>> = decode(&gh(&[
        "api",
        "--hostname",
        "github.com",
        "--paginate",
        "--slurp",
        endpoint,
    ])?)?;
    Ok(pages.into_iter().flatten().collect())
}

fn gh(args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .map_err(|e| format!("gh: {e}"))?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    // What `gh` prints when GitHub or the connection failed, not the request: `HTTP 502` and the
    // other 5xx statuses, and the Go network errors. A 4xx or a refused attestation has neither.
    let server_error = stderr.match_indices("HTTP 5").any(|(at, found)| {
        let status = &stderr.as_bytes()[at + found.len()..];
        status.len() >= 2 && status[..2].iter().all(u8::is_ascii_digit)
    });
    if server_error
        || BROKEN_GH_CONNECTION
            .iter()
            .any(|text| stderr.contains(text))
    {
        WAS_UNAVAILABLE.store(true, Ordering::Relaxed);
        Err(format!("{UNAVAILABLE}: gh: {stderr}"))
    } else {
        Err(format!("gh: {stderr}"))
    }
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    serde_json::from_slice(bytes).map_err(|e| format!("GitHub JSON: {e}"))
}

/// Starts the message of a download or a `gh` call that failed for a reason that is not the
/// release's: the server answered 5xx, or the connection timed out or broke.
const UNAVAILABLE: &str = "GitHub unavailable";

/// Set when such a failure happened in this process. A run that then fails exits with
/// [`UNAVAILABLE_EXIT`], so a caller can tell "could not check" from "checked and refused".
/// It is a flag and not a search of the error text, which carries tag names, file names and
/// GitHub's own messages: none of them can make a refusal look unchecked.
static WAS_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

pub fn was_unavailable() -> bool {
    WAS_UNAVAILABLE.load(Ordering::Relaxed)
}

/// Go's texts for a connection that timed out, was refused or reset, or never resolved, as
/// `gh` passes them on (`net`, `net/http`, `context` and `crypto/tls` errors).
const BROKEN_GH_CONNECTION: [&str; 6] = [
    "i/o timeout",
    "context deadline exceeded",
    "TLS handshake timeout",
    "connection reset by peer",
    "connection refused",
    "no such host",
];

/// `EX_TEMPFAIL` of BSD `sysexits.h`: a temporary failure, the caller is invited to retry.
pub const UNAVAILABLE_EXIT: u8 = 75;

/// curl exit codes that say nothing about the file (curl's manual, "EXIT CODES"): 5 and 6 could
/// not resolve the proxy or host, 7 could not connect, 18 partial file, 28 timeout, 35 TLS
/// handshake, 52 empty reply, 55 and 56 failed to send or receive, 92 HTTP/2 stream error.
const BROKEN_CONNECTION: [i32; 10] = [5, 6, 7, 18, 28, 35, 52, 55, 56, 92];

/// The waits before the second, third and fourth attempt, in seconds: curl's own `--retry`
/// starts at one second and doubles. curl's `--retry` itself is not used because it also
/// retries HTTP 408 and 429, and leaves a broken connection alone unless every error,
/// 404 included, is retried.
const RETRY_WAITS: [u64; 3] = [1, 2, 4];

pub fn download(url: &str, dest: &Path, limit: u64) -> Result<(), String> {
    if limit == 0 {
        return Err("empty release asset".into());
    }
    let mut waits = RETRY_WAITS.into_iter();
    loop {
        // All callers pass a canonical GitHub release URL. HTTPS redirects are needed for
        // GitHub's CDN and repository renames. curl never gets an authorization header or an
        // author's options.
        let output = Command::new("curl")
            .args([
                "--disable",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--globoff",
                "--write-out",
                "%{http_code}",
                "--max-filesize",
            ])
            .arg(limit.to_string())
            .arg("--output")
            .arg(dest)
            .arg("--url")
            .arg(url)
            .output()
            .map_err(|e| format!("curl: {e}"))?;
        if output.status.success() {
            break;
        }
        let http = String::from_utf8_lossy(&output.stdout);
        if http == "404" {
            return Err("download not found (HTTP 404)".into());
        }
        let reason = String::from_utf8_lossy(&output.stderr);
        let reason = reason.trim();
        let passing = http.starts_with('5')
            || output
                .status
                .code()
                .is_some_and(|code| BROKEN_CONNECTION.contains(&code));
        if !passing {
            return Err(format!("download failed: {reason}"));
        }
        let Some(wait) = waits.next() else {
            WAS_UNAVAILABLE.store(true, Ordering::Relaxed);
            return Err(format!(
                "{UNAVAILABLE}: download failed {} times: {reason}",
                RETRY_WAITS.len() + 1
            ));
        };
        std::thread::sleep(std::time::Duration::from_secs(wait));
    }
    if fs::metadata(dest).map_err(|e| e.to_string())?.len() > limit {
        return Err("download exceeds its size limit".into());
    }
    Ok(())
}

#[derive(Debug)]
pub enum Observation {
    Unknown(String),
    Ineligible(String),
}

impl std::fmt::Display for Observation {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(reason) => write!(out, "release verification is unknown: {reason}"),
            Self::Ineligible(reason) => out.write_str(reason),
        }
    }
}

pub struct Attestation<'a> {
    pub repository_id: RepositoryId,
    pub repository_name: &'a RepositoryName,
    pub workflow_path: &'a str,
    pub tag: &'a str,
    pub commit: &'a str,
}

impl Attestation<'_> {
    pub fn verify(&self, file: &Path, bundle: &Path) -> Result<SignerWorkflowRef, Observation> {
        let path = |p: &Path| {
            p.to_str()
                .map(str::to_owned)
                .ok_or("verification path is not UTF-8")
        };
        let file = path(file).map_err(|error| Observation::Unknown(error.into()))?;
        let bundle = path(bundle).map_err(|error| Observation::Unknown(error.into()))?;
        let source_ref = format!("refs/tags/{}", self.tag);
        let output = gh(&[
            "attestation",
            "verify",
            &file,
            "--bundle",
            &bundle,
            "--hostname",
            "github.com",
            "--repo",
            self.repository_name.as_str(),
            "--signer-workflow",
            self.workflow_path,
            "--source-ref",
            &source_ref,
            "--source-digest",
            self.commit,
            "--format",
            "json",
        ])
        .or_else(|_| {
            // After a repository is renamed, verification by owner still scopes the certificate
            // to GitHub's account; the numeric repository ID check below remains mandatory.
            let (owner, _) = self
                .repository_name
                .as_str()
                .split_once('/')
                .ok_or("invalid repository name")?;
            gh(&[
                "attestation",
                "verify",
                &file,
                "--bundle",
                &bundle,
                "--hostname",
                "github.com",
                "--owner",
                owner,
                "--signer-workflow",
                self.workflow_path,
                "--source-ref",
                &source_ref,
                "--source-digest",
                self.commit,
                "--format",
                "json",
            ])
        })
        .map_err(Observation::Unknown)?;
        self.claims(&output)
    }

    fn claims(&self, output: &[u8]) -> Result<SignerWorkflowRef, Observation> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Verified {
            verification_result: Verification,
        }
        #[derive(Deserialize)]
        struct Verification {
            signature: Signature,
        }
        #[derive(Deserialize)]
        struct Signature {
            certificate: Certificate,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Certificate {
            subject_alternative_name: String,
            #[serde(rename = "buildSignerURI")]
            build_signer_uri: String,
            source_repository_identifier: String,
            source_repository_ref: String,
            source_repository_digest: String,
        }
        let results: Vec<Verified> = decode(output).map_err(Observation::Unknown)?;
        let mut refs = BTreeSet::new();
        for result in results {
            let cert = result.verification_result.signature.certificate;
            if cert.source_repository_identifier.parse::<u64>().ok()
                != Some(self.repository_id.get())
                || cert.source_repository_ref != format!("refs/tags/{}", self.tag)
                || cert.source_repository_digest != self.commit
                || cert.build_signer_uri != cert.subject_alternative_name
            {
                continue;
            }
            if let Some(signer) = cert.build_signer_uri.strip_prefix("https://github.com/")
                && signer
                    .split_once('@')
                    .is_some_and(|(path, _)| path == self.workflow_path)
                && let Ok(workflow) = SignerWorkflowRef::parse(signer.into())
            {
                refs.insert(workflow);
            }
        }
        if refs.len() != 1 {
            return Err(Observation::Ineligible("expected one verified signer workflow at a release-v<number> tag for the registered repository ID and release ref/commit".into()));
        }
        refs.into_iter()
            .next()
            .ok_or_else(|| Observation::Ineligible("no verified signer workflow".into()))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn verified_certificate_must_bind_numeric_id_tag_commit_and_a_versioned_signer() {
        let repo = RepositoryName::parse("Example/Renamed".into()).unwrap();
        let id = serde_json::from_str::<RepositoryId>("123456789").unwrap();
        let check = Attestation {
            repository_id: id,
            repository_name: &repo,
            workflow_path: crate::schema::RELEASE_WORKFLOW,
            tag: "mod/v1.2.0",
            commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        };
        let uri = format!(
            "https://github.com/{}@refs/tags/release-v2",
            check.workflow_path
        );
        let cert = json!({
            "subjectAlternativeName": uri,
            "buildSignerURI": uri,
            "sourceRepositoryIdentifier": "123456789",
            "sourceRepositoryRef": "refs/tags/mod/v1.2.0",
            "sourceRepositoryDigest": check.commit,
        });
        let verified =
            |cert| json!([{"verificationResult":{"signature":{"certificate":cert}}}]).to_string();
        assert!(check.claims(verified(cert.clone()).as_bytes()).is_ok());
        for (field, wrong) in [
            ("sourceRepositoryIdentifier", "123456788"),
            ("sourceRepositoryRef", "refs/heads/mod/v1.2.0"),
            (
                "sourceRepositoryDigest",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
            ("buildSignerURI", "another workflow"),
        ] {
            let mut wrong_cert = cert.clone();
            wrong_cert[field] = json!(wrong);
            assert!(
                check.claims(verified(wrong_cert).as_bytes()).is_err(),
                "accepted wrong {field}"
            );
        }
        let mut moving = cert;
        let moving_uri = uri.replace("refs/tags/release-v2", "refs/heads/main");
        moving["buildSignerURI"] = json!(moving_uri);
        moving["subjectAlternativeName"] = json!(moving_uri);
        assert!(check.claims(verified(moving).as_bytes()).is_err());
    }

    #[test]
    fn a_closed_release_pr_handles_its_version_only_while_its_branch_exists() {
        let mut branches = ReleaseBranches::default();
        branches.record("release/Ab3dE9xQ/1.0.0".into(), true, false, false, true);
        branches.record("release/Ab3dE9xQ/0.9.0".into(), false, false, true, false);
        branches.record("release/Zz91kLmN/1.0.0".into(), false, false, false, true);
        branches.record("release/Zz91kLmN/1.1.0".into(), false, false, false, false);
        // Open with a failed release check: handled, but a later release need not wait for it.
        branches.record("release/Cc3dE9xQ/1.0.0".into(), true, true, false, true);
        assert_eq!(
            branches.waiting,
            BTreeSet::from(["release/Ab3dE9xQ/1.0.0".to_owned()])
        );
        assert_eq!(
            branches.handled,
            BTreeSet::from([
                "release/Ab3dE9xQ/0.9.0".to_owned(),
                "release/Ab3dE9xQ/1.0.0".to_owned(),
                "release/Cc3dE9xQ/1.0.0".to_owned(),
                "release/Zz91kLmN/1.0.0".to_owned(),
            ])
        );
    }
}
