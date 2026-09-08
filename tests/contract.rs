//! Checks that keep the published contract honest.
//!
//! The generated half of the contract cannot drift: `contract/types` is written
//! from the same structs that serialize the responses, and CI fails on a dirty
//! working tree after regenerating it. The hand-written half — the documented
//! error codes, the Doorman revision the docs point at, and the browser adapter
//! derived from Doorman's reference client — has no such guarantee, so it is
//! checked here instead of trusted to a habit.

use std::{fs, path::PathBuf};

use gameboxd_backend::error::ErrorCode;

fn repo_file(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
}

/// The Doorman commit `Cargo.lock` actually resolves to.
///
/// Read from the lock file rather than `Cargo.toml` because the lock file is
/// what the build uses, and it is the file that changes when the pin moves.
fn pinned_doorman_revision() -> String {
    let lock = repo_file("Cargo.lock");
    let source = lock
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("source = ") && line.contains("/Doorman?rev="))
        .expect("Cargo.lock should record a git source for Doorman");
    let revision = source
        .rsplit_once("?rev=")
        .expect("a rev-pinned git source")
        .1
        .trim_end_matches('"')
        .split('#')
        .next()
        .expect("a revision");
    assert_eq!(
        revision.len(),
        40,
        "expected a full commit hash, got {revision:?}"
    );
    revision.to_owned()
}

/// Every documentation link to Doorman names the revision the build resolves.
///
/// This is the failure the readiness review found by hand: the pin moved and
/// two documents kept pointing at the previous commit, so the authoritative
/// source for the auth DTOs was a commit the backend no longer built against.
#[test]
fn documentation_points_at_the_pinned_doorman_revision() {
    let pinned = pinned_doorman_revision();
    for name in ["README.md", "docs/api.md", "contract/README.md"] {
        let text = repo_file(name);
        assert!(
            text.contains(&pinned),
            "{name} does not mention the pinned Doorman revision {pinned}"
        );
        // A stale link is worse than a missing one: it looks authoritative.
        for other in commit_hashes(&text) {
            assert_eq!(
                other, pinned,
                "{name} references Doorman commit {other}, but the build pins {pinned}"
            );
        }
    }
}

/// Every 40-character hex string in the text, which for these documents means
/// every commit a link or a prose reference names.
fn commit_hashes(text: &str) -> Vec<String> {
    let bytes: Vec<char> = text.chars().collect();
    let mut found = Vec::new();
    let mut start = 0;
    while start + 40 <= bytes.len() {
        let candidate = &bytes[start..start + 40];
        let boundary_before = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let boundary_after =
            start + 40 == bytes.len() || !bytes[start + 40].is_ascii_alphanumeric();
        if boundary_before
            && boundary_after
            && candidate.iter().all(|c| c.is_ascii_hexdigit())
            && candidate.iter().any(|c| c.is_ascii_digit())
        {
            found.push(candidate.iter().collect());
            start += 40;
        } else {
            start += 1;
        }
    }
    found
}

/// The vendored copy of Doorman's reference browser client still matches the
/// pinned revision.
///
/// `contract/auth.ts` is derived from that file by hand, so a change upstream
/// is a change this app has to review rather than inherit silently. Comparing
/// the pristine copy — not the adapted one — is what makes the check exact.
#[test]
fn the_vendored_doorman_client_matches_the_pinned_revision() {
    let pinned = pinned_doorman_revision();
    let vendored = repo_file("contract/doorman/passkey.upstream.ts");
    let upstream_path = doorman_checkout(&pinned).join("examples/web-client/passkey.ts");
    let upstream = fs::read_to_string(&upstream_path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", upstream_path.display()));
    assert_eq!(
        vendored,
        upstream,
        "contract/doorman/passkey.upstream.ts differs from Doorman {pinned}.\n\
         Copy {} over it, then re-derive contract/auth.ts from the difference.",
        upstream_path.display()
    );
}

/// Cargo's checkout of the pinned Doorman revision.
///
/// Any run of this test has already compiled Doorman from that checkout, so it
/// is present; the directory is named by the short revision, under a database
/// directory whose suffix Cargo chooses.
fn doorman_checkout(revision: &str) -> PathBuf {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .expect("CARGO_HOME or HOME should be set");
    let checkouts = cargo_home.join("git/checkouts");
    let short = &revision[..7];
    let mut candidates: Vec<PathBuf> = fs::read_dir(&checkouts)
        .unwrap_or_else(|error| panic!("reading {}: {error}", checkouts.display()))
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("Doorman-"))
        })
        .map(|entry| entry.path().join(short))
        .filter(|path| path.is_dir())
        .collect();
    candidates.sort();
    candidates.pop().unwrap_or_else(|| {
        panic!(
            "no checkout of Doorman {revision} under {}; run `cargo fetch` first",
            checkouts.display()
        )
    })
}

/// The documented error-code list covers every code the application can emit.
///
/// The codes are an enum precisely so a client can match them exhaustively;
/// that is only worth anything if the list a frontend author reads is complete.
#[test]
fn every_error_code_is_documented() {
    let documentation = repo_file("docs/api.md");
    for code in ErrorCode::ALL {
        assert!(
            documentation.contains(&format!("`{}`", code.as_str())),
            "docs/api.md does not document the `{}` error code",
            code.as_str()
        );
    }
}

/// Every generated type the client imports exists.
///
/// The generated directory is rewritten by `cargo test`, so this catches the
/// opposite mistake: a type removed or renamed in Rust while `contract/*.ts`
/// still imports it. Without a TypeScript toolchain in CI, nothing else would.
#[test]
fn the_hand_written_client_imports_types_that_exist() {
    for source in ["contract/client.ts", "contract/index.ts"] {
        let text = repo_file(source);
        let mut imported = 0;
        for line in text.lines() {
            let Some((_, rest)) = line.split_once("from \"./types/") else {
                continue;
            };
            let name = rest.trim_end_matches("\";").trim_end_matches('"');
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("contract/types")
                .join(format!("{name}.ts"));
            assert!(
                path.is_file(),
                "{source} imports ./types/{name}, which was not generated"
            );
            imported += 1;
        }
        assert!(imported > 0, "{source} should import generated types");
    }
}
