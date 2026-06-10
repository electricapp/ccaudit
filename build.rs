//! Build script: stamps the binary with build-time provenance so
//! `ccaudit --version` can report when and from which commit it was built
//! (cloudflared-style). Values are exposed as compile-time env vars and
//! read with `env!` in `src/cli.rs`.

use std::process::Command;

fn main() {
    // UTC build timestamp, ISO 8601 `YYYY-MM-DDThh:mm:ssZ`.
    let build_time = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    println!("cargo:rustc-env=CCAUDIT_BUILD_TIME={build_time}");

    // Short git SHA when built inside a checkout; "unknown" otherwise
    // (e.g. a `cargo install` from a crates.io tarball with no `.git`).
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=CCAUDIT_GIT_SHA={sha}");

    // Always watch a path that exists (build.rs) so a tree WITHOUT `.git`
    // (crates.io tarball, vendored build) doesn't fall into cargo's
    // "no rerun-if directives → rerun every build" mode — which would
    // re-stamp the timestamp and recompile the whole crate on every build.
    println!("cargo:rerun-if-changed=build.rs");
    emit_git_rerun_paths();
}

// Watch the files that actually move on a commit/checkout so the stamped
// SHA stays current. A new commit updates `.git/refs/heads/<branch>` (or
// `.git/packed-refs`), NOT `.git/HEAD` — watching only HEAD (as before)
// missed every commit on the same branch.
fn emit_git_rerun_paths() {
    use std::path::Path;
    let git = Path::new(".git");
    if !git.exists() {
        return;
    }
    // HEAD itself changes on checkout / detach.
    println!("cargo:rerun-if-changed=.git/HEAD");
    let Ok(head) = std::fs::read_to_string(git.join("HEAD")) else {
        return;
    };
    // "ref: refs/heads/main" → watch the resolved ref; detached HEAD (a
    // bare SHA) needs nothing beyond HEAD itself.
    if let Some(reference) = head.strip_prefix("ref:").map(str::trim) {
        if git.join(reference).exists() {
            println!("cargo:rerun-if-changed=.git/{reference}");
        } else {
            // Loose ref absent → it's packed.
            println!("cargo:rerun-if-changed=.git/packed-refs");
        }
    }
}
