# Release scanner

Call `.github/workflows/scan.yml` from the registry's trusted release PR workflow, with `source-artifact` and optionally `previous-artifact`. Each is an artifact from the same run holding one file, `release.tar`, with the unpacked release at the archive's root. The caller verifies package integrity and unpacks the package before archiving it. `source-path` and `previous-path` are alternatives for directories already present in the trusted registry checkout, not paths shared from another job. Keep the workflow and its input selection on the trusted side of the PR.

The `scan-summary` artifact contains the full `summary.md`: hard failures, any incomplete-scan reason, warning subjects, then the release diff. If the report reaches GitHub's 1 MiB step-summary limit, the job summary shows an initial portion and points to the full artifact. A separate reporting job can append a `### Capabilities` section without access to the release. Do not treat a clean summary as a safety verdict; review the actual release and dependencies.

- The only hard findings are non-crates.io sources (including dependency paths outside the package) and advisories with `[advisory].categories` containing `malicious`. If a missing or stale lockfile, an invalid checksum, or another scan error prevents completion, the report gives the reason as `scan-incomplete` and the check still fails. Other advisories and yanks are warnings; licences are not checked.
- RustSec is fetched once and its commit is recorded. cargo-deny checks every registry entry in the lockfile before Cargo can fetch a removed crate, then uses the same advisory checkout and Cargo's local index for yanks. Cargo fetches locked crates.io dependencies from a clean working directory and Cargo home; it does not build anything or load the release's Cargo configuration.
- Semgrep runs offline with no author-controlled ignore rules. It flags recognizable syntax in `.rs` files and in-package manifest-declared targets and build scripts, regardless of their suffix. The scan does not expand macros or follow `include!` and `#[path]`, and flags both for review. A plain `mod name;` is flagged only when the release has no file for it at Rust's default path (`name.rs` or `name/mod.rs`), such as a module a build script writes; every `.rs` file in the release is scanned whichever module names it. Paths under `assets/` only suppress a few literal, recognizable asset reads; computed paths need review. Large encoded string flags start at 128 characters.
- The release diff reports when a manifest activates or deactivates a build script or proc-macro crate, even when its source is unchanged.
- Cackle's non-build summary reports configured allowlists, not observed capabilities. Run capability checks separately in the isolated build job; neither this job nor its fixtures run a mod, build script or proc macro.

For local checks with `semgrep`, `cargo-deny`, `cargo` and `rustc` on `PATH`:

```sh
semgrep --test --metrics=off --disable-version-check \
  --config scanner/rules/rust.yml scanner/tests/flags/src/lib.rs
python3 -I scanner/scan.py scanner/tests/flags --previous scanner/tests/prior \
  --output target/scanner/summary.md
```

The runner test checks summary truncation and calls the reusable workflow with clean, flagged, git dependency, RustSec malware, stale-lock and missing-target fixtures; the expected hard failures and incomplete scan are asserted only in that test caller.
