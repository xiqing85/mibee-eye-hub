//! Same git provenance stamp as the root build script, for the crate that
//! serves `/api/capabilities` — the wire face of build identity
//! (mibee-rec#1). Paths are relative to this package root.

use std::process::Command;

fn main() {
    let (sha, dirty) = probe();
    println!("cargo:rustc-env=MITBEE_BUILD_SHA={sha}");
    println!("cargo:rustc-env=MITBEE_BUILD_DIRTY={dirty}");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
}

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
