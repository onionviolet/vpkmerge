// Embed a build id in `vpkmerge --version`.
//
// WHY. The package version alone cannot tell two binaries apart when one of
// them is a local build carrying unreleased patches. That is not a cosmetic
// problem: Grimoire's Settings has an engine switcher whose whole purpose is to
// point a packaged app at a locally built engine, and it reports the engine by
// running `--version`. A patched local build and upstream's pinned release both
// answered "vpkmerge 0.19.0", so the one thing the switcher exists to
// disambiguate was the one thing it could not show. A user could be running an
// engine that lacks `catalog globalsounds` and have the UI confirm the version
// they expected.
//
// Bumping the crate version was the other option and is worse: the version
// number is upstream's release identity, so a fork that bumps it is claiming a
// release that does not exist, and it desyncs the workspace's internal
// `version = "0.19"` dependency pins. A commit id is the honest answer to
// "which build is this" and leaves the release identity alone.
//
// Degrades quietly. No git, no repo (a crates.io or tarball build), or no
// `git` on PATH all fall back to the bare package version, which is exactly
// today's behaviour, so nothing depends on the git metadata existing.

use std::process::Command;

fn main() {
    let pkg = env!("CARGO_PKG_VERSION");

    // Re-run when HEAD moves. This catches commits and branch switches, which
    // is the case that matters. It does NOT catch going dirty/clean between
    // builds without a commit; a stale `-dirty` suffix is a cosmetic miss, and
    // watching the whole worktree to fix it would rebuild the crate constantly.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");

    let version = match git_describe() {
        Some(id) => format!("{pkg} ({id})"),
        None => pkg.to_string(),
    };
    println!("cargo:rustc-env=VPKMERGE_VERSION={version}");
}

/// `<short sha>`, plus `-dirty` when the worktree has uncommitted changes.
fn git_describe() -> Option<String> {
    let sha = run(&["rev-parse", "--short", "HEAD"])?;
    // `--quiet --exit-code` makes diff exit non-zero when there ARE changes, so
    // a failed status means dirty. An error running git at all is treated as
    // clean rather than guessing, since the sha is the useful half.
    let dirty = Command::new("git")
        .args(["diff", "--quiet", "--exit-code"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(false);
    Some(if dirty { format!("{sha}-dirty") } else { sha })
}

fn run(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
