//! Every documentation link Suede hands an operator must resolve.
//!
//! Health checks and divergences carry a `docsUrl` so that a warning always
//! ends somewhere useful. That promise is only worth anything if the page and
//! the anchor actually exist, and both live in a different tree from the code
//! that names them — so nothing but a test connects the two.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Sources that name documentation pages.
const SOURCES: &[(&str, &str)] = &[
    ("src/checks/mod.rs", include_str!("../src/checks/mod.rs")),
    (
        "src/model/observed.rs",
        include_str!("../src/model/observed.rs"),
    ),
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `"page/#anchor"` string literal in `source`.
///
/// Split on quotes without tracking parity: escaped quotes elsewhere in the
/// file would shift it, and silently dropping a link is the one failure this
/// test cannot afford. The shape filter below is what does the real work.
fn referenced_links(source: &str) -> BTreeSet<String> {
    let mut links = BTreeSet::new();
    for candidate in source.split('"') {
        // A docs path is `page/#anchor` — relative, one segment, no scheme.
        if let Some((page, anchor)) = candidate.split_once("/#") {
            let plausible = !page.is_empty()
                && !anchor.is_empty()
                && !page.contains(['/', ' ', ':'])
                && !anchor.contains([' ', '/']);
            if plausible {
                links.insert(candidate.to_string());
            }
        }
    }
    links
}

/// Slugify a heading the way Python-Markdown's toc extension does.
fn slugify(heading: &str) -> String {
    heading
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect::<String>()
        .trim()
        .to_lowercase()
        .replace(' ', "-")
}

/// Anchors a Markdown page offers: slugified headings, plus any explicit
/// `{: #id }` set with attr_list.
fn anchors_in(markdown: &str) -> BTreeSet<String> {
    let mut anchors = BTreeSet::new();
    let mut in_fence = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || !line.starts_with('#') {
            continue;
        }
        let heading = line.trim_start_matches('#').trim();

        if let Some((text, attributes)) = heading.split_once("{:") {
            if let Some(id) = attributes.split('#').nth(1) {
                anchors.insert(id.trim().trim_end_matches('}').trim().to_string());
            }
            // attr_list replaces the generated id, so `text` contributes none.
            let _ = text;
        } else {
            anchors.insert(slugify(heading));
        }
    }
    anchors
}

#[test]
fn every_documentation_link_resolves() {
    let docs = repo_root().join("docs");
    let mut checked = 0;

    for (origin, source) in SOURCES {
        for link in referenced_links(source) {
            let (page, anchor) = link.split_once("/#").unwrap();
            let path = docs.join(format!("{page}.md"));
            assert!(
                path.exists(),
                "{origin} links to {link}, but {} does not exist",
                path.display()
            );

            let markdown = std::fs::read_to_string(&path).unwrap();
            let anchors = anchors_in(&markdown);
            assert!(
                anchors.contains(anchor),
                "{origin} links to {link}, but {page}.md has no #{anchor}.\n\
                 available: {anchors:?}"
            );
            checked += 1;
        }
    }

    // A scanner that quietly matched nothing would pass forever.
    assert!(
        checked >= 10,
        "only {checked} links found; the scan is broken"
    );
}

#[test]
fn the_docs_base_url_is_not_hardcoded_in_the_checks() {
    // Paths stay relative so a fork can point `docsBaseUrl` at its own site.
    for (origin, source) in SOURCES {
        assert!(
            !source.contains("https://suede.gameshow.pro"),
            "{origin} hardcodes the published docs host"
        );
    }
}

#[test]
fn slugs_match_the_mkdocs_convention() {
    assert_eq!(slugify("A display stays dark"), "a-display-stays-dark");
    assert_eq!(
        slugify("Audio goes to the wrong place, or nowhere"),
        "audio-goes-to-the-wrong-place-or-nowhere"
    );
    assert_eq!(
        slugify("Backgrounds and wallpapers"),
        "backgrounds-and-wallpapers"
    );
}

#[test]
fn explicit_ids_are_preferred_over_generated_ones() {
    let anchors = anchors_in("## Provision the machine {: #service }\n## Plain heading\n");
    assert!(anchors.contains("service"));
    assert!(anchors.contains("plain-heading"));
    assert!(
        !anchors.contains("provision-the-machine"),
        "attr_list replaces the generated id, so linking to it would 404"
    );
}

#[test]
fn fenced_code_is_not_mistaken_for_headings() {
    // Troubleshooting pages are full of shell comments beginning with '#'.
    let anchors = anchors_in("## Real\n\n```bash\n# Not a heading\n```\n");
    assert_eq!(anchors, BTreeSet::from(["real".to_string()]));
}

#[test]
fn the_link_scanner_ignores_ordinary_strings() {
    let found =
        referenced_links(r#" let a = "plain"; let b = "https://x/#y"; let c = "page/#anchor"; "#);
    assert_eq!(found, BTreeSet::from(["page/#anchor".to_string()]));
}
