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
    println!("cargo:rerun-if-env-changed=MEWXI_SOURCE_REPO");
    println!("cargo:rerun-if-env-changed=MEWXI_ORIGIN_URL");

    // Where the source checkout lives, as seen by the *installed*
    // binary. Normally the dir being built — but when the self-updater
    // builds in a throwaway temp clone, it sets MEWXI_SOURCE_REPO to
    // the original checkout so the new binary doesn't bake in a temp
    // path that's deleted the moment the install finishes.
    let source_repo = std::env::var("MEWXI_SOURCE_REPO")
        .unwrap_or_else(|_| std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    println!("cargo:rustc-env=MEWXI_SOURCE_REPO={source_repo}");

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

    // Users who grab a prebuilt `mewxi`/`mewxi.exe` off GitHub
    // Actions/Releases have no source checkout at all — MEWXI_SOURCE_REPO
    // above bakes in the *builder's* path (e.g. a CI runner's
    // `D:\a\Mewxi\Mewxi`), which doesn't exist on their machine, so
    // `update::repo_dir` always fails for them. Embedding the origin URL
    // too lets the updater fall back to a remote-only check/apply
    // (`git ls-remote`, then clone straight from this URL) instead of
    // just giving up. MEWXI_ORIGIN_URL lets CI override this explicitly
    // (e.g. to pin the public repo URL regardless of what remote the
    // runner's checkout happens to use); otherwise we read the checkout's
    // own `origin` — same fallback shape as MEWXI_SOURCE_REPO/BUILD_COMMIT,
    // empty when there's no git context.
    let origin_url = std::env::var("MEWXI_ORIGIN_URL").ok().or_else(|| {
        Command::new("git")
            .args(["remote", "get-url", "origin"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }).unwrap_or_default();
    println!("cargo:rustc-env=MEWXI_ORIGIN_URL={origin_url}");

    embed_default_blocks();
}

/// Embed the default status-line blocks (`blocks/*.toml`) so `mewxi
/// status` composes the line with zero config. We codegen a slice of
/// `(stem, include_str!(abs_path))` into `OUT_DIR/default_blocks.rs`
/// rather than hand-maintaining an `include_str!` list (which would
/// silently drop newly added blocks).
fn embed_default_blocks() {
    use std::fmt::Write as _;

    println!("cargo:rerun-if-changed=blocks");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let blocks_dir = std::path::Path::new(&manifest).join("blocks");

    let mut blocks: Vec<(String, String)> = Vec::new();
    for entry in walkdir::WalkDir::new(&blocks_dir)
        .min_depth(1)
        .max_depth(1)
        .into_iter()
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Forward slashes so the include_str! literal works on Windows too.
        blocks.push((stem.to_string(), path.to_string_lossy().replace('\\', "/")));
    }
    blocks.sort();

    let mut gen = String::from("pub static DEFAULT_BLOCKS: &[(&str, &str)] = &[\n");
    for (stem, abspath) in &blocks {
        let _ = writeln!(gen, "    ({stem:?}, include_str!({abspath:?})),");
    }
    gen.push_str("];\n");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let dest = std::path::Path::new(&out_dir).join("default_blocks.rs");
    std::fs::write(&dest, gen).expect("write default_blocks.rs");
}
