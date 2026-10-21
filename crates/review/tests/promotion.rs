//! Cherry-picks and promotions (`review::promotion`): what a pull request changes beyond the merged pull requests it
//! carries (fisher-ki-saas#2945, #2951, #2983: 18k to 117k added lines, re-read line by line for hours).

use review::promotion::{
    CommitMessage, PromotionSource, cherry_sources, parse_commits, promotion_delta, referenced_pulls, source_label,
};

fn file(path: &str, hunk: &str, lines: &[&str]) -> String {
    format!(
        "diff --git a/{path} b/{path}\nindex 1111111..2222222 100644\n--- a/{path}\n+++ b/{path}\n{hunk}\n{}\n",
        lines.join("\n")
    )
}

fn new_file(path: &str, lines: &[&str]) -> String {
    let added: Vec<String> = lines.iter().map(|line| format!("+{line}")).collect();
    format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\nindex 0000000..3333333\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n{}\n",
        lines.len(),
        added.join("\n")
    )
}

const PURGE: [&str; 6] = [
    "def purge(rows):",
    "    for row in rows:",
    "        if row.expired:",
    "            delete(row)",
    "    audit('purge', len(rows))",
    "    return len(rows)",
];

fn none(_: &str) -> Option<Vec<String>> {
    None
}

#[test]
fn a_file_carried_over_unchanged_is_left_out_and_an_adaptation_is_read() {
    let source = new_file("purge.py", &PURGE);
    let adaptation = file(
        "settings.py",
        "@@ -10,3 +10,4 @@ SETTINGS = {",
        &["     'a': 1,", "     'b': 2,", "+    'purge': True,", " }"],
    );
    let pull = format!("{}{adaptation}", new_file("purge.py", &PURGE));
    let delta = promotion_delta(
        &pull,
        &[PromotionSource {
            label: "#2931".into(),
            diff: source,
        }],
        &none,
    );
    assert_eq!(delta.sources, ["#2931"]);
    assert_eq!((delta.carried_files, delta.carried_lines), (1, 6));
    assert_eq!(delta.diff, adaptation);
    assert!(delta.missing.is_empty(), "{:?}", delta.missing);
}

#[test]
fn a_conflict_resolution_is_read_with_its_context_and_a_recounted_header() {
    let source = new_file("purge.py", &PURGE);
    // The pick resolved a conflict on line 4: it deletes through the archive instead.
    let mut picked = PURGE;
    picked[3] = "            archive.delete(row)";
    let delta = promotion_delta(
        &new_file("purge.py", &picked),
        &[PromotionSource {
            label: "#2931".into(),
            diff: source,
        }],
        &none,
    );
    let expected = "diff --git a/purge.py b/purge.py\nnew file mode 100644\nindex 0000000..3333333\n--- /dev/null\n+++ b/purge.py\n\
        @@ -0,0 +1,6 @@\n+def purge(rows):\n+    for row in rows:\n+        if row.expired:\n+            archive.delete(row)\n+    audit('purge', len(rows))\n+    return len(rows)\n";
    assert_eq!(delta.diff, expected);
    assert_eq!((delta.carried_files, delta.carried_lines), (0, 5));
    // The plain delete is not in the head, but the file is read: the resolution shows there, nothing is listed.
    assert!(delta.missing.is_empty(), "{:?}", delta.missing);
}

#[test]
fn a_change_deep_in_a_long_hunk_is_read_as_its_own_hunk() {
    let lines: Vec<String> = (1..=30).map(|n| format!("line {n} = {n}")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let source = new_file("big.py", &refs);
    let mut picked = lines.clone();
    picked[19] = "line 20 = 'twenty'".into();
    let picked_refs: Vec<&str> = picked.iter().map(String::as_str).collect();
    let delta = promotion_delta(
        &new_file("big.py", &picked_refs),
        &[PromotionSource {
            label: "`abc1234`".into(),
            diff: source,
        }],
        &none,
    );
    // Lines 17 to 23 of the new file: the changed one and three of context on each side.
    let hunk: Vec<&str> = delta.diff.lines().skip(5).collect();
    assert_eq!(hunk[0], "@@ -0,0 +17,7 @@");
    assert_eq!(hunk[1], "+line 17 = 17");
    assert_eq!(hunk[4], "+line 20 = 'twenty'");
    assert_eq!(hunk.len(), 8);
    assert_eq!(delta.carried_lines, 29);
}

#[test]
fn removed_lines_match_too_and_windows_that_touch_are_one_hunk() {
    let source = file(
        "a.py",
        "@@ -1,4 +1,4 @@",
        &[" x = 1", "-y = 2", "+y = 3", " z = 4", " w = 5"],
    );
    let pull = file(
        "a.py",
        "@@ -1,7 +1,9 @@",
        &[
            " x = 1",
            "-y = 2",
            "+y = 3",
            " z = 4",
            "+extra = 1",
            " w = 5",
            " v = 6",
            "+more = 2",
            " u = 7",
            " t = 8",
        ],
    );
    let delta = promotion_delta(
        &pull,
        &[PromotionSource {
            label: "#1".into(),
            diff: source,
        }],
        &none,
    );
    assert_eq!(delta.carried_lines, 2);
    // `extra` (index 4) and `more` (index 7) are within three lines: one window from index 1 to the end.
    let hunks: Vec<&str> = delta.diff.lines().filter(|line| line.starts_with("@@")).collect();
    assert_eq!(hunks, ["@@ -2,6 +2,8 @@"]);
    assert!(delta.diff.contains("-y = 2\n+y = 3\n"));
}

#[test]
fn a_source_the_pull_request_does_not_carry_changes_nothing() {
    let pull = new_file("purge.py", &PURGE);
    // Named in the description for context: none of its lines are in this pull request.
    let other = new_file(
        "other.py",
        &["import os", "def other():", "    return 1", "def purge(rows):"],
    );
    let delta = promotion_delta(
        &pull,
        &[PromotionSource {
            label: "#100".into(),
            diff: other,
        }],
        &none,
    );
    assert!(delta.sources.is_empty());
    assert_eq!(delta.diff, pull);
}

#[test]
fn a_file_left_out_of_the_pick_is_missing_unless_the_head_has_its_lines() {
    let source = format!(
        "{}{}",
        new_file("purge.py", &PURGE),
        new_file(
            "purge_test.py",
            &["def test_purge():", "    assert purge([]) == 0", "}"]
        )
    );
    let delta = promotion_delta(
        &new_file("purge.py", &PURGE),
        &[PromotionSource {
            label: "#2931".into(),
            diff: source.clone(),
        }],
        &none,
    );
    assert_eq!(delta.diff, "");
    let missing: Vec<(&str, u64)> = delta.missing.iter().map(|m| (m.path.as_str(), m.lines)).collect();
    // The brace is too short to list.
    assert_eq!(missing, [("purge_test.py", 2)]);
    // Already on the base branch: not missing.
    let delta = promotion_delta(
        &new_file("purge.py", &PURGE),
        &[PromotionSource {
            label: "#2931".into(),
            diff: source,
        }],
        &|path| (path == "purge_test.py").then(|| vec!["def test_purge():".into(), "    assert purge([]) == 0".into()]),
    );
    assert!(delta.missing.is_empty(), "{:?}", delta.missing);
}

#[test]
fn cherry_pick_trailers_name_the_nearest_source_once() {
    let out = "aaa\x1fMerge pull request #2761 from o/feat\x1fMerge pull request #2761 from o/feat\n\n\
        (cherry picked from commit 62dfe5777c408c7383d53c64d611b1eb43be0d34)\n\
        (cherry picked from commit 14155d689fbeafa55981b4b47da100439b8ec491)\n\x1e\n\
        bbb\x1fAdapt for staging\x1fAdapt for staging\n\x1e\n\
        ccc\x1fFix (#2962)\x1fFix (#2962)\n\n(cherry picked from commit 14155d689fbeafa55981b4b47da100439b8ec491)\n\x1e\n";
    let commits = parse_commits(out);
    assert_eq!(
        commits
            .iter()
            .map(|c| (c.sha.as_str(), c.subject.as_str()))
            .collect::<Vec<_>>(),
        [
            ("aaa", "Merge pull request #2761 from o/feat"),
            ("bbb", "Adapt for staging"),
            ("ccc", "Fix (#2962)")
        ]
    );
    assert_eq!(cherry_sources(&commits), ["14155d689fbeafa55981b4b47da100439b8ec491"]);
    let one = CommitMessage {
        sha: "x".into(),
        subject: String::new(),
        message: "(cherry picked from commit d8576d36a)".into(),
    };
    assert_eq!(cherry_sources(&[one]), ["d8576d36a"]);
    assert_eq!(
        source_label("Merge pull request #2931 from o/fix", "d8576d36a"),
        "#2931"
    );
    assert_eq!(
        source_label("Fix the retention review findings (#2941)", "c409e424a"),
        "#2941"
    );
    assert_eq!(source_label("Adapt retention readers", "c409e424a733"), "`c409e42`");
}

#[test]
fn only_this_repositorys_pull_request_numbers_are_referenced() {
    assert_eq!(
        referenced_pulls(
            "Cherry-pick #2931, #2941, #2936 and #2937 to soc2-staging (#2945); follows #2929. See o/r#12, #12345678, \
             #x, a#5, #2931 again, (#7)",
            2945
        ),
        [2931, 2941, 2936, 2937, 2929, 7]
    );
}

#[test]
fn a_line_a_later_source_replaces_is_not_missing() {
    // #2761 adds the setting; #2791, a follow-up, replaces it; the pick has the follow-up's line.
    let first = new_file("rechunk.py", &["BATCH = 10", "def rechunk(): pass"]);
    let follow_up = file(
        "rechunk.py",
        "@@ -1,2 +1,2 @@",
        &["-BATCH = 10", "+BATCH = 50", " def rechunk(): pass"],
    );
    let delta = promotion_delta(
        &new_file("rechunk.py", &["BATCH = 50", "def rechunk(): pass"]),
        &[
            PromotionSource {
                label: "#2761".into(),
                diff: first,
            },
            PromotionSource {
                label: "#2791".into(),
                diff: follow_up,
            },
        ],
        &|_| Some(vec!["BATCH = 50".into(), "def rechunk(): pass".into()]),
    );
    assert_eq!(delta.diff, "");
    assert_eq!(delta.sources, ["#2761", "#2791"]);
    assert!(delta.missing.is_empty(), "{:?}", delta.missing);
}
