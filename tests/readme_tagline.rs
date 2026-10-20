//! The README is plain Markdown and cannot be templated, so the tagline it
//! carries is checked against the one source of truth instead.

#[test]
fn readme_carries_the_tagline_from_cargo_toml() {
    let tagline = env!("CARGO_PKG_DESCRIPTION");
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("README.md should be readable");
    assert!(
        readme.contains(tagline),
        "README.md must contain the tagline verbatim. The tagline is the \
         `description` in Cargo.toml, which is its only source: update README.md \
         to match it, or change Cargo.toml and then README.md.\ntagline: {tagline}"
    );
}
