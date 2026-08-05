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
    println!("cargo:rerun-if-changed=.build-id");
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
        .or_else(build_id_file)
        .or_else(describe)
        .unwrap_or_else(|| "unknown".to_string());

    // When the identity is not handed to us, say so at build time and say
    // what was looked for. Twice now a build has produced a binary that could
    // not name its own commit, and both times the build itself looked
    // perfectly healthy - the value was reported correctly by everything
    // except the one process that needed it.
    {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let file = manifest.join(".build-id");
        // Whether the mount carries files git does not track. If `.build-id`
        // is missing while `.git` is present, the workspace reached the
        // container by a route that filtered it, and no amount of writing
        // that file on the host will ever help.
        let untracked_visible = manifest.join("target").exists();
        println!(
            "cargo:warning=suede build identity: {id} (env {}, .build-id {}, \
             .git {}, target/ {}, manifest {})",
            match std::env::var("SUEDE_BUILD_ID") {
                Ok(value) if !value.trim().is_empty() => format!("set to {value}"),
                _ => "absent".to_string(),
            },
            if file.exists() { "exists" } else { "ABSENT" },
            if manifest.join(".git").exists() {
                "exists"
            } else {
                "absent"
            },
            if untracked_visible {
                "exists"
            } else {
                "absent"
            },
            manifest.display(),
        );
    }

    println!("cargo:rustc-env=SUEDE_BUILD_ID={id}");
    // A greppable copy for anything that inspects the artifact rather than
    // running it. Deliberately long and bracketed: a six-byte string used at
    // runtime is materialised as store-immediates under optimisation and
    // never appears contiguously in the file, which made an identity check
    // grepping for the bare id fail against perfectly correct binaries.
    println!("cargo:rustc-env=SUEDE_BUILD_STAMP=[suede build id: {id}]");

    // Composed here rather than at runtime so it can be a `&'static str`,
    // which is what a command-line parser wants for `--version`.
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    // A release build's identity is its own tag, so showing both would read
    // "0.1.1 (v0.1.1)". The parenthesis earns its place only when it says
    // something the version does not.
    let is_its_own_tag = id == version || id == format!("v{version}");
    let shown = if id == "unknown" || is_its_own_tag {
        version
    } else {
        format!("{version} ({id})")
    };
    println!("cargo:rustc-env=SUEDE_VERSION_STRING={shown}");
}

/// The id from `.build-id`, or `None` when nobody wrote one.
///
/// An environment variable is the obvious way to tell a build script
/// something, and it is the wrong one when `cross` is involved: the compiler
/// runs inside a container that receives only an allowlist of variables, and
/// what does not arrive fails silently - the script simply falls back and
/// produces a plausible answer. That shipped a release whose binary reported
/// its identity as `unknown` while CI logged the correct value.
///
/// A file does not have that problem. The workspace is mounted into the
/// container because that is where the source is, so anything written here
/// arrives by the same route the code does.
fn build_id_file() -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".build-id");
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
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
