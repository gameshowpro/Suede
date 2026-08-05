//! Records which build this is, so a running daemon can say so.
//!
//! `CARGO_PKG_VERSION` alone answers "which release is this meant to be",
//! which is not the question anyone actually has. Every build between two
//! releases carries the same number, so an operator installing a fix and
//! seeing no change cannot tell whether the package failed to install, the
//! daemon failed to restart, or the fix simply was not in it. That cost real
//! time before this existed.
//!
//! The identity comes from `git describe`, which yields `v0.1.0-12-g81226ee`
//! — the last release, how far past it, and exactly which commit.

fn main() {
    // A source tarball has no git, and CI would rather state the answer than
    // have it rediscovered; both go through this variable.
    println!("cargo:rerun-if-env-changed=SUEDE_BUILD_ID");
    // Without these, the recorded identity would be whatever it was when the
    // build script last ran, which is worse than not recording one at all.
    for path in [".git/HEAD", ".git/refs/heads", ".git/packed-refs"] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    let id = std::env::var("SUEDE_BUILD_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(describe)
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=SUEDE_BUILD_ID={id}");

    // Composed here rather than at runtime so it can be a `&'static str`,
    // which is what a command-line parser wants for `--version`.
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let shown = if id == "unknown" || id == version {
        version
    } else {
        format!("{version} ({id})")
    };
    println!("cargo:rustc-env=SUEDE_VERSION_STRING={shown}");
}

/// `git describe`, or `None` when this is not a checkout.
fn describe() -> Option<String> {
    let output = std::process::Command::new("git")
        // `--always` so a checkout with no tags yet still yields the commit;
        // `--dirty` so a build from uncommitted changes admits it, which
        // matters most on the machine where the changes are being made.
        .args(["describe", "--tags", "--always", "--dirty", "--match", "v*"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let described = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!described.is_empty()).then_some(described)
}
