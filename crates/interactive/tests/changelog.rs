//! `/changelog`: replays fixtures/changelog.json, recorded from utils/changelog.ts by fixtures/changelog.mjs.

use interactive::changelog::{changelog_markdown, changelog_tokens, normalize_links, parse};
use serde_json::Value;
use tui::marked::markdown_tokens;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/changelog.json")).unwrap()
}

#[test]
fn interactive_changelog_links_pin_to_the_release_like_ts() {
    for sample in fixture()["samples"].as_array().unwrap() {
        assert_eq!(
            normalize_links(sample["markdown"].as_str().unwrap(), (1, 2, 3)),
            sample["normalized"].as_str().unwrap()
        );
    }
}

#[test]
fn interactive_changelog_entries_match_ts() {
    let content = include_str!("../../../packages/coding-agent/CHANGELOG.md");
    let fixture = fixture();
    // TS measures UTF-16 units; the fixture is regenerated when CHANGELOG.md changes.
    assert_eq!(
        content.encode_utf16().count() as u64,
        fixture["changelog"]["content"].as_u64().unwrap(),
        "CHANGELOG.md changed: rerun fixtures/changelog.mjs"
    );
    let entries = parse(content);
    let versions: Vec<String> = entries
        .iter()
        .map(|e| format!("{}.{}.{}", e.version.0, e.version.1, e.version.2))
        .collect();
    let want: Vec<String> = fixture["changelog"]["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(versions, want);
    for (entry, want) in entries.iter().zip(fixture["changelog"]["newest"].as_array().unwrap()) {
        assert_eq!(normalize_links(&entry.content, entry.version), want.as_str().unwrap());
    }
}

#[test]
fn interactive_changelog_oldest_first_and_empty_notice() {
    let text = "# Changelog\n\n## [Unreleased]\n\nx\n\n## [0.2.0] - d\n\nnew\n\n## 0.1.0\n\nold [a](docs/a.md)\n";
    assert_eq!(
        changelog_markdown(Some(text)),
        "## 0.1.0\n\nold [a](https://github.com/earendil-works/pi/blob/v0.1.0/packages/coding-agent/docs/a.md)\n\n## [0.2.0] - d\n\nnew"
    );
    assert_eq!(changelog_markdown(None), "No changelog entries found.");
    assert_eq!(changelog_markdown(Some("no versions")), "No changelog entries found.");
}

#[test]
fn interactive_changelog_lexed_per_entry_matches_the_whole_document() {
    // The newest 20 releases of the real CHANGELOG.md (lists, code, links, nested items), lexed entry by entry on
    // several threads, give the tokens of the joined document that MarkdownNotice lexed.
    let content = include_str!("../../../packages/coding-agent/CHANGELOG.md");
    let mut starts = content.match_indices("\n## [").map(|(i, _)| i + 1);
    let end = starts.nth(21).unwrap();
    let sample = &content[..end];
    assert_eq!(parse(sample).len(), 20);
    assert_eq!(
        changelog_tokens(Some(sample)),
        markdown_tokens(changelog_markdown(Some(sample)).trim())
    );
    assert_eq!(changelog_tokens(None), markdown_tokens("No changelog entries found."));
}
