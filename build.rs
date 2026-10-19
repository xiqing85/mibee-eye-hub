//! Embed git provenance at compile time (mibee-rec#1: build identity).
//!
//! The rerun-if-changed set is deliberately git-only: the embedded values
//! depend on repository state, not source files, so a source edit replays
//! the cached output (correct — the sha did not move) while a commit under
//! `.git/refs` reruns the probe and re-stamps the build.

use std::process::Command;

fn main() {
    let (sha, dirty) = probe();
    println!("cargo:rustc-env=MITBEE_BUILD_SHA={sha}");
    println!("cargo:rustc-env=MITBEE_BUILD_DIRTY={dirty}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
}

/// Short HEAD hash and a dirty flag; `("unknown", 0)` when git is absent
/// (e.g. an exported source tree) so the binary still builds and honestly
/// says it cannot map to a commit.
fn probe() -> (String, u8) {
    let run = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    match run(&["rev-parse", "--short=12", "HEAD"]) {
        Some(sha) => {
            let dirty = run(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
            (sha, u8::from(dirty))
        }
        None => ("unknown".to_string(), 0),
    }
}
