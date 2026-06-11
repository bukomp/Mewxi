//! Embeds the commit this binary is built from as `MEWXI_BUILD_COMMIT`
//! so the dev-channel update check can compare the *installed binary*
//! against origin — the checkout's HEAD is a bad proxy when you develop
//! in the same checkout you install from (HEAD is already in sync with
//! origin right after a push, while the binary is still old).

use std::process::Command;

fn main() {
    // Re-run when the checked-out commit moves so a plain `cargo build`
    // picks up the new hash without needing a clean build.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");

    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    // Empty when there's no git context (e.g. building from a source
    // tarball) — update::dev_baseline falls back to the checkout HEAD.
    println!("cargo:rustc-env=MEWXI_BUILD_COMMIT={hash}");
}
