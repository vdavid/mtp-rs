//! Two things the code needs to know at compile time:
//!
//! - `has_libmtp`: whether the libmtp baseline is built in (the `libmtp` feature, and not
//!   Windows, where the dependency is left out). One cfg, so the sources don't repeat the pair.
//! - `MTP_BENCH_GIT_REV`: the commit this binary was built from. The report asks the checkout at
//!   run time first; this is the answer for a binary copied to a machine without one (a Windows
//!   laptop with no toolchain), where that lookup can't work.

use std::path::Path;
use std::process::Command;

/// The paths whose last commit is "the code under test". Keep in sync with `run_info::CODE_PATHS`.
const CODE_PATHS: [&str; 5] = [
    "crates/mtp-rs",
    "benchmarks/mtp-rs-vs-libmtp/src",
    "benchmarks/mtp-rs-vs-libmtp/Cargo.toml",
    "Cargo.toml",
    "Cargo.lock",
];

fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_libmtp)");
    let feature = std::env::var_os("CARGO_FEATURE_LIBMTP").is_some();
    let windows = std::env::var("CARGO_CFG_WINDOWS").is_ok();
    if feature && !windows {
        println!("cargo::rustc-cfg=has_libmtp");
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = root.to_string_lossy();
    println!("cargo::rustc-env=MTP_BENCH_GIT_REV={}", git_rev(&root));

    // Rerun when the commit or the code changes, so the embedded rev doesn't go stale.
    for p in [
        "crates/mtp-rs/src",
        "crates/mtp-rs/Cargo.toml",
        "benchmarks/mtp-rs-vs-libmtp/src",
        "benchmarks/mtp-rs-vs-libmtp/Cargo.toml",
        "Cargo.toml",
        "Cargo.lock",
    ] {
        println!("cargo::rerun-if-changed={root}/{p}");
    }
    let mut git_paths = vec!["HEAD".to_string(), "index".into(), "packed-refs".into()];
    if let Some(r) = git(&root, &["rev-parse", "--symbolic-full-name", "HEAD"]) {
        git_paths.push(r);
    }
    for p in git_paths {
        // A path that doesn't exist (no `packed-refs` yet) would rerun this on every build.
        match git(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-path", &p],
        ) {
            Some(path) if Path::new(&path).exists() => {
                println!("cargo::rerun-if-changed={path}")
            }
            _ => {}
        }
    }
}

/// The last commit that touched [`CODE_PATHS`], `-dirty` with local changes there.
fn git_rev(root: &str) -> String {
    let mut log = vec!["log", "-1", "--format=%h", "--abbrev=12", "--"];
    log.extend(CODE_PATHS);
    let Some(rev) = git(root, &log) else {
        return "unknown".to_string();
    };
    let mut status = vec!["status", "--porcelain", "--untracked-files=no", "--"];
    status.extend(CODE_PATHS);
    match git(root, &status) {
        Some(_) => format!("{rev}-dirty"),
        None => rev,
    }
}

fn git(root: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}
