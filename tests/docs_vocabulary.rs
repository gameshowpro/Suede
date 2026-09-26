//! The pipeline vocabulary is fixed: canvas -> slice -> warp -> output ->
//! display (see `docs/how-it-works.md#vocabulary`). "Screen" and "monitor"
//! are banned as device nouns anywhere in the docs prose; every device
//! reference is either the connector (`output`) or the physical thing
//! attached to it (`display`), and "projector" is reserved for prose that is
//! actually about projector-specific workflows.
//!
//! Nothing enforces that by construction, so this test scans every
//! `docs/*.md` page for the banned words and fails, naming every hit, rather
//! than letting one slip back in through an edit that did not know the rule.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The top-level documentation pages: `docs/*.md`, not its subdirectories
/// (`docs/developer/`, `docs/plans/`, ...), matching the scope of this lint.
fn doc_pages() -> Vec<PathBuf> {
    let docs = repo_root().join("docs");
    let mut pages: Vec<PathBuf> = std::fs::read_dir(&docs)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", docs.display()))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    pages.sort();
    pages
}

/// A banned device noun, matched the way `\b(screens?|monitors?)\b` would:
/// case-insensitive, and only as a whole word — "offscreen", "fullscreen",
/// "monitoring" and "monitored" do not count, only the bare noun does.
fn is_banned_word(word: &str) -> bool {
    matches!(
        word.to_lowercase().as_str(),
        "screen" | "screens" | "monitor" | "monitors"
    )
}

/// Every whole word in `line`, split the same way `\b` would: on anything
/// that is not a letter, digit, or underscore.
fn words(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
}

/// One line's worth of banned-word hits, or none.
///
/// Two kinds of text are exempt, because they are identifiers rather than
/// prose: lines naming an mkdocs Material icon token
/// (`:material-monitor-multiple:`, `:material-projector-screen:`, ...), and
/// inline code spans — a third party's literal flag or component name, such
/// as `pw-dump --monitor` or WirePlumber's `v4l2 monitor` objects, is a fact
/// about that third party, and renaming it in the docs would misdocument it.
/// Nothing else is allowlisted — a real hit is fixed by rephrasing, not by
/// adding an exception here.
fn banned_words_in(line: &str) -> Vec<&str> {
    if line.contains(":material-") {
        return Vec::new();
    }
    // Backticks alternate prose and code span: even-indexed segments are
    // prose. An unbalanced line leaves its tail counted as code, which is
    // the safe failure for a lint that names third-party identifiers.
    line.split('`')
        .step_by(2)
        .flat_map(words)
        .filter(|word| is_banned_word(word))
        .collect()
}

#[test]
fn docs_use_display_not_screen_or_monitor() {
    let pages = doc_pages();
    let mut violations = Vec::new();
    let mut scanned_lines = 0;

    for page in &pages {
        let markdown = std::fs::read_to_string(page)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", page.display()));
        for (number, line) in markdown.lines().enumerate() {
            scanned_lines += 1;
            for word in banned_words_in(line) {
                violations.push(format!(
                    "{}:{}: {word:?} — {}",
                    page.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "docs use a banned device noun (\"screen\"/\"monitor\"); the pipeline \
         vocabulary is canvas -> slice -> warp -> output -> display, and \
         \"display\" (or \"output\", where the connector is meant) is the \
         only device noun. Rephrase rather than allowlisting:\n{}",
        violations.join("\n")
    );

    // A scanner that read no files, or somehow no lines, would pass forever.
    assert!(
        !pages.is_empty(),
        "no docs/*.md pages found; the scan is broken"
    );
    assert!(
        scanned_lines >= 100,
        "only {scanned_lines} lines scanned across {} pages; the scan is broken",
        pages.len()
    );
}

#[test]
fn the_word_scanner_respects_boundaries() {
    assert!(is_banned_word("screen"));
    assert!(is_banned_word("Screen"));
    assert!(is_banned_word("SCREENS"));
    assert!(is_banned_word("monitor"));
    assert!(is_banned_word("Monitors"));

    // Compounds and inflections are not the banned noun itself.
    assert!(!is_banned_word("offscreen"));
    assert!(!is_banned_word("fullscreen"));
    assert!(!is_banned_word("monitoring"));
    assert!(!is_banned_word("monitored"));
    assert!(!is_banned_word("screening"));
}

#[test]
fn a_hyphenated_compound_still_splits_into_the_bare_word() {
    // "off-screen" is exactly the case the docs used to get wrong: the
    // hyphen is a word boundary, so "screen" inside it is still a hit,
    // which is why the docs spell it "offscreen" (one word) instead.
    assert_eq!(
        banned_words_in("park the cursor off-screen"),
        vec!["screen"]
    );
    assert_eq!(
        banned_words_in("park the cursor offscreen"),
        Vec::<&str>::new()
    );
}

#[test]
fn material_icon_tokens_are_the_only_allowlisted_lines() {
    assert_eq!(
        banned_words_in("-   :material-monitor-multiple:{ .lg .middle } **Full output control**"),
        Vec::<&str>::new()
    );
    assert_eq!(
        banned_words_in("-   :material-projector-screen:{ .lg .middle } **Projection**"),
        Vec::<&str>::new()
    );
    // A line that merely mentions "material" without the icon-token prefix
    // is not exempt.
    assert_eq!(
        banned_words_in("this material is shown on the screen"),
        vec!["screen"]
    );
}

#[test]
fn inline_code_spans_are_exempt_but_the_prose_around_them_is_not() {
    // A third party's literal flag or component name stays literal.
    assert_eq!(
        banned_words_in("a continuously running `pw-dump --monitor` feeds it"),
        Vec::<&str>::new()
    );
    assert_eq!(
        banned_words_in("WirePlumber's `v4l2 monitor` objects list the devices"),
        Vec::<&str>::new()
    );
    // The exemption is the span, never the sentence it sits in.
    assert_eq!(
        banned_words_in("the screen runs `pw-dump --monitor` all day"),
        vec!["screen"]
    );
    // An unbalanced backtick treats the tail as code: safe for identifiers,
    // and the balanced form is what well-formed docs write anyway.
    assert_eq!(banned_words_in("run `pw-dump --monitor"), Vec::<&str>::new());
}

#[test]
fn projector_and_source_are_not_linted_here() {
    // Deliberately not this test's job: both words have legitimate uses in
    // docs prose (projector-specific workflows; source code, audio sources,
    // EventSource), and the glossary states the policy instead of a lint.
    assert_eq!(
        banned_words_in("a four-projector rig, sampled from the source canvas"),
        Vec::<&str>::new()
    );
}
