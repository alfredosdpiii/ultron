//! Replays crates/review/fixtures/autoreview.json, recorded from autoreview_api.py (fixtures/record_autoreview.py):
//! the pull request context, review guides over a rebuilt tree, others' comments, line mapping, reviewer keys,
//! slices, merged specialists, finding records, merging and the public finding shape.

use review::chunks::{Chunk, build_chunks};
use review::context::{
    OTHERS_WINDOW, context_block, diff_line_ranges, hunks_text, load_guides, map_line, raised_by_others, reviewer_keys,
};
use review::diff::{Hunk, parse_diff};
use review::merge::{
    check_view, dedupe_fast, extras, group_root_causes, level_rank, nearly_identical, parse_rules, public,
};
use review::prompts::reviewer;
use review::slices::{describe_failure, member_for, merged_reviewers, pack_chunks};
use serde_json::{Map, Value, json};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/autoreview.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture json")
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().expect("object").clone()
}

fn objects(value: &Value) -> Vec<Map<String, Value>> {
    serde_json::from_value(value.clone()).expect("objects")
}

fn hunk_json(hunk: &Hunk) -> Value {
    json!({
        "old_start": hunk.old_start, "old_len": hunk.old_len, "new_start": hunk.new_start, "new_len": hunk.new_len,
        "header": hunk.header,
        "lines": hunk.lines.iter().map(|l| json!([l.kind.to_string(), l.old, l.new, l.text])).collect::<Vec<_>>(),
    })
}

fn chunk_json(chunk: &Chunk) -> Value {
    json!({ "id": chunk.id, "path": chunk.path, "kind": chunk.kind, "status": chunk.status, "part": chunk.part,
            "parts": chunk.parts, "text": chunk.text, "hunks": chunk.hunks.iter().map(hunk_json).collect::<Vec<_>>() })
}

#[test]
fn review_autoreview_context_block_matches_python() {
    let fx = fixture();
    for case in fx["contexts"].as_array().expect("contexts") {
        let files = &case["files"];
        let block = context_block(&case["context"], |path| {
            serde_json::from_value(files[path].clone()).ok()
        });
        assert_eq!(block, case["block"].as_str().expect("block"), "{}", case["context"]);
    }
}

#[test]
fn review_autoreview_load_guides_matches_python() {
    let fx = fixture();
    let temp = tempfile::tempdir().expect("temp");
    let root_path = std::fs::canonicalize(temp.path()).expect("real");
    let root = root_path.to_str().expect("utf-8").to_string();
    for entry in fx["guide_tree"].as_array().expect("tree") {
        let path = root_path.join(entry[0].as_str().expect("path"));
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(&path, entry[1].as_str().expect("text")).expect("write");
    }
    for link in fx["guide_links"].as_array().expect("links") {
        std::os::unix::fs::symlink(
            link[1].as_str().expect("target"),
            root_path.join(link[0].as_str().expect("link")),
        )
        .expect("symlink");
    }
    let diff = "diff --git a/app/views.py b/app/views.py\n--- a/app/views.py\n+++ b/app/views.py\n@@ -1 +1,2 @@\n x\n\
                +from django.http import HttpResponse  # React? no\n\
                diff --git a/web/App.TSX b/web/App.TSX\n--- a/web/App.TSX\n+++ b/web/App.TSX\n@@ -1 +1 @@\n-a\n+b\n";
    let files = parse_diff(diff);
    assert_eq!(files.len() as u64, fx["guide_diff_files"].as_u64().expect("count"));
    let home = root_path.join("fakehome");
    for case in fx["guides"].as_array().expect("guides") {
        let paths: Value = serde_json::from_str(&case["paths"].to_string().replace("<root>", &root)).expect("paths");
        let with = if case["with_files"].as_bool().expect("flag") {
            &files[..]
        } else {
            &[]
        };
        let (text, names) = load_guides(
            &paths,
            case["repo"].as_str().expect("repo"),
            with,
            case["limit"].as_u64().expect("limit") as usize,
            home.to_str(),
        );
        let got: Value =
            serde_json::from_str(&json!([text, names]).to_string().replace(&root, "<root>")).expect("json");
        assert_eq!(got, case["result"], "{case}");
    }
}

#[test]
fn review_autoreview_raised_by_others_matches_python() {
    let fx = fixture();
    let finding = object(&fx["others_finding"]);
    let comments = fx["others_comments"].as_array().expect("comments");
    for case in fx["others"].as_array().expect("others") {
        let window = case[0].as_i64().expect("window");
        assert_eq!(json!(raised_by_others(&finding, comments, window)), case[1], "{window}");
    }
    assert_eq!(OTHERS_WINDOW, 5);
}

#[test]
fn review_autoreview_line_mapping_matches_python() {
    let fx = fixture();
    let files = parse_diff(fx["map_diff"].as_str().expect("diff"));
    for case in fx["mapped"].as_array().expect("mapped") {
        let item = case[0].as_u64().map(|index| &files[index as usize]);
        let (line, nearest) = (case[1].as_i64().expect("line"), case[2].as_bool().expect("nearest"));
        assert_eq!(json!(map_line(item, line, nearest)), case[3], "{case}");
    }
    assert_eq!(json!(diff_line_ranges(&files)), fx["ranges"]);
    for case in fx["hunks_text"].as_array().expect("hunks_text") {
        let item = case[0].as_u64().map(|index| &files[index as usize]);
        let limit = case[1].as_u64().expect("limit") as usize;
        assert_eq!(hunks_text(item, limit), case[2].as_str().expect("text"), "{case}");
    }
}

#[test]
fn review_autoreview_reviewer_keys_match_python() {
    let fx = fixture();
    for case in fx["keys"].as_array().expect("keys") {
        let got = match reviewer_keys(&case[0]) {
            Ok(keys) => json!({ "ok": keys }),
            Err(error) => json!({ "error": error.0 }),
        };
        assert_eq!(got, case[1], "{}", case[0]);
    }
}

#[test]
fn review_autoreview_slices_and_members_match_python() {
    let fx = fixture();
    let files = parse_diff(fx["slice_diff"].as_str().expect("diff"));
    let (chunks, _) = build_chunks(&files, |_| None, 200, 12);
    assert_eq!(
        json!(chunks.iter().map(chunk_json).collect::<Vec<_>>()),
        fx["slice_chunks"]
    );
    for case in fx["packs"].as_array().expect("packs") {
        let changed: Vec<String> = serde_json::from_value(case["changed"].clone()).expect("changed");
        let (slices, members) = pack_chunks(
            &chunks,
            &changed,
            case["max_chars"].as_u64().expect("max") as usize,
            case["single"].as_bool().expect("single"),
        );
        assert_eq!(
            json!(slices.iter().map(chunk_json).collect::<Vec<_>>()),
            case["slices"],
            "{}",
            case["max_chars"]
        );
        assert_eq!(json!(members), case["members"]);
    }
    let changed: Vec<String> = files.iter().map(|item| item.path.clone()).collect();
    for case in fx["pack_sweep"].as_array().expect("sweep") {
        let max_chars = case[0].as_u64().expect("max") as usize;
        let sizes: Vec<usize> = pack_chunks(&chunks, &changed, max_chars, false)
            .1
            .iter()
            .map(Vec::len)
            .collect();
        assert_eq!(json!(sizes), case[1], "{max_chars}");
    }
    let (_, members) = pack_chunks(&chunks, &changed, 14_000, false);
    for case in fx["members"].as_array().expect("members") {
        let number = case[0].as_u64().expect("slice") as usize;
        let group = if number == 0 {
            vec![0]
        } else {
            members[number - 1].clone()
        };
        assert_eq!(json!(member_for(&chunks, &group, &case[1])), case[2], "{case}");
    }
}

#[test]
fn review_autoreview_merged_reviewers_match_python() {
    let fx = fixture();
    for case in fx["merges"].as_array().expect("merges") {
        let reviewers: Vec<_> = case["keys"]
            .as_array()
            .expect("keys")
            .iter()
            .map(|key| reviewer(key.as_str().expect("key")).expect("reviewer"))
            .collect();
        let got: Vec<Value> = merged_reviewers(&reviewers, case["lines"].as_u64().expect("lines") as usize)
            .iter()
            .map(|r| {
                json!({ "key": r.key, "title": r.title, "focus": r.focus, "checklist": r.checklist,
                        "reads_docs": r.reads_docs, "llm_only": r.llm_only })
            })
            .collect();
        assert_eq!(json!(got), case["out"], "{}", case["keys"]);
    }
    let pair = [reviewer("bugs").expect("bugs"), reviewer("arch").expect("arch")];
    assert_eq!(
        merged_reviewers(&pair, 1)[0].autoreview_finder_task(),
        fx["merged_task"].as_str().expect("task")
    );
}

#[test]
fn review_autoreview_describe_failure_matches_python() {
    let fx = fixture();
    for case in fx["failures"].as_array().expect("failures") {
        assert_eq!(
            json!(describe_failure(case[0].as_str(), case[1].as_str())),
            case[2],
            "{case}"
        );
    }
}

#[test]
fn review_autoreview_finding_records_match_python() {
    let fx = fixture();
    for case in fx["rules"].as_array().expect("rules") {
        let mut finding = Map::new();
        finding.insert("file".into(), "a.py".into());
        parse_rules(&case[0], &mut finding);
        assert_eq!(Value::Object(finding), case[1], "{}", case[0]);
    }
    for case in fx["extras"].as_array().expect("extras") {
        let mut finding = object(&case[1]);
        extras(&case[0], &mut finding);
        assert_eq!(Value::Object(finding), case[2], "{}", case[0]);
    }
    for case in fx["ranks"].as_array().expect("ranks") {
        let (rank, confidence) = level_rank(&object(&case[0]));
        assert_eq!(json!([rank, confidence]), case[1], "{}", case[0]);
    }
    for case in fx["identical"].as_array().expect("identical") {
        let (a, b) = (case[0].as_str().expect("a"), case[1].as_str().expect("b"));
        assert_eq!(
            json!(nearly_identical(a, b, case[2].as_f64().expect("t"))),
            case[3],
            "{case}"
        );
    }
    for case in fx["checks"].as_array().expect("checks") {
        assert_eq!(check_view(&case[0]), case[1].as_str().expect("view"), "{}", case[0]);
    }
    for case in fx["public"].as_array().expect("public") {
        let got = public(&object(&case[0]), case[1].as_str().expect("verification"));
        assert_eq!(Value::Object(got), case[2], "{}", case[0]);
    }
}

#[test]
fn review_autoreview_dedupe_fast_and_root_causes_match_python() {
    let fx = fixture();
    for case in fx["dedupe_fast"].as_array().expect("dedupe") {
        let mut records = Vec::new();
        let out = dedupe_fast(&objects(&case["input"]), &mut records);
        assert_eq!(json!(out), case["out"]);
        assert_eq!(json!(records), case["records"]);
    }
    for case in fx["roots"].as_array().expect("roots") {
        let mut records = Vec::new();
        let (kept, merged) = group_root_causes(&objects(&case["input"]), Some(&mut records));
        assert_eq!(json!(kept), case["kept"]);
        assert_eq!(json!(merged), case["merged"]);
        assert_eq!(json!(records), case["records"]);
        let (alone, count) = group_root_causes(&objects(&case["input"]), None);
        assert_eq!(
            (json!(alone), json!(count)),
            (case["kept"].clone(), case["merged"].clone())
        );
    }
}
