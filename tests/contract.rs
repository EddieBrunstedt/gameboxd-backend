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

/// The published `AuthCode` union names exactly the codes Doorman can emit.
///
/// Doorman's codes reach the frontend through Gameboxd's own application
/// routes, not just through `/auth`: its session layer fronts the whole router,
/// so `GET /api/me` without a cookie answers `401 session_not_found`. The
/// contract has to be able to name that code, which means keeping a union of
/// codes this repository does not own — and a hand-copied list against a git
/// dependency is exactly the kind of thing that rots the first time the pin
/// moves. Both directions are checked: a code added upstream and a code left
/// behind here are both failures.
#[test]
fn the_published_auth_codes_match_the_pinned_doorman_revision() {
    let pinned = pinned_doorman_revision();
    let checkout = doorman_checkout(&pinned);

    let error_source = fs::read_to_string(checkout.join("crates/axum/src/error.rs")).unwrap();
    // `classify` is the single match assigning every `Error` variant its stable
    // code. Slice it out before the unit tests below it, which repeat the same
    // literals and would otherwise be counted as a second source of truth.
    let body = error_source
        .split_once("fn classify(")
        .expect("Doorman error.rs should define classify")
        .1
        .split_once("#[cfg(test)]")
        .map_or_else(
            || panic!("classify should be followed by Doorman's own tests"),
            |(body, _)| body.to_owned(),
        );
    let mut upstream = string_literals(&body);

    // The CSRF layer rejects before `classify` is ever reached, so its code is
    // not in that match and has to be picked up where it is actually written.
    let csrf_source = fs::read_to_string(checkout.join("crates/axum/src/csrf.rs")).unwrap();
    let csrf = csrf_source
        .lines()
        .find_map(|line| line.trim().strip_prefix("code: "))
        .map(|rest| rest.trim_end_matches(',').trim_matches('"').to_owned())
        .expect("Doorman csrf.rs should name a stable code");
    upstream.push(csrf);
    upstream.sort();
    upstream.dedup();

    let published = published_union("AuthCode");
    assert_eq!(
        published,
        upstream,
        "contract/errors.ts `AuthCode` disagrees with Doorman {pinned}.\n\
         missing here: {:?}\n\
         no longer upstream: {:?}",
        upstream
            .iter()
            .filter(|code| !published.contains(code))
            .collect::<Vec<_>>(),
        published
            .iter()
            .filter(|code| !upstream.contains(code))
            .collect::<Vec<_>>(),
    );
}

/// Every `"snake_case"` literal in a slice of Rust source, sorted and deduped.
///
/// Deliberately crude: within `classify`'s match arms the only string literals
/// are the codes themselves, so anything more clever would be pretending to
/// parse Rust for no gain.
fn string_literals(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = source;
    while let Some((_, after)) = rest.split_once('"') {
        let Some((literal, remainder)) = after.split_once('"') else {
            break;
        };
        if !literal.is_empty() && literal.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
            found.push(literal.to_owned());
        }
        rest = remainder;
    }
    found.sort();
    found.dedup();
    found
}

/// The members of a published TypeScript string-literal union, sorted.
fn published_union(name: &str) -> Vec<String> {
    let source = repo_file("contract/errors.ts");
    let declaration = source
        .split_once(&format!("export type {name} ="))
        .unwrap_or_else(|| panic!("contract/errors.ts should declare {name}"))
        .1
        .split_once(';')
        .expect("a terminated type declaration")
        .0;
    let mut members: Vec<String> = declaration
        .split('|')
        .map(|member| member.trim().trim_matches('"').to_owned())
        .filter(|member| !member.is_empty())
        .collect();
    members.sort();
    members.dedup();
    members
}

/// Every code the two unions publish has user-facing guidance.
///
/// `guidanceFor` falls back to generic wording, so a missing entry is not a
/// crash — it is a user reading "Something went wrong. Try again." when the
/// server said something specific enough to act on. The server codes that
/// describe internal faults are exempt: there is nothing a user can do about a
/// storage failure, and inventing reassuring copy for one would be worse than
/// the generic line.
#[test]
fn every_published_code_a_user_can_act_on_has_guidance() {
    let source = repo_file("contract/errors.ts");
    let guidance = source
        .split_once("export const ERROR_GUIDANCE")
        .expect("contract/errors.ts should declare ERROR_GUIDANCE")
        .1;
    const INTERNAL: [&str; 6] = [
        "storage_error",
        "email_error",
        "internal_error",
        "invalid_relying_party",
        "invalid_operator_actor",
        "signup_not_allowed",
    ];
    for code in published_union("AuthCode")
        .into_iter()
        .chain(published_union("ClientCode"))
    {
        if INTERNAL.contains(&code.as_str()) {
            continue;
        }
        assert!(
            guidance.contains(&format!("{code}:")),
            "contract/errors.ts has no ERROR_GUIDANCE entry for `{code}`"
        );
    }
}
