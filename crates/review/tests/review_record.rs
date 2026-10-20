//! The per-review record: every posted review leaves one, and only the newest are kept.

use review::reviewer::{MAX_REVIEW_RECORDS, write_review_record};
use serde_json::json;

#[test]
fn review_records_keep_the_newest_and_drop_the_oldest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().to_str().expect("utf-8");
    for n in 0..MAX_REVIEW_RECORDS + 2 {
        write_review_record(base, &format!("o-r-1-{n:04}-me"), &json!({ "n": n })).expect("written");
    }
    let reviews = dir.path().join("reviews");
    let mut names: Vec<String> = std::fs::read_dir(&reviews)
        .expect("listed")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), MAX_REVIEW_RECORDS);
    assert_eq!(names[0], "o-r-1-0002-me.json");
    let last: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(reviews.join(&names[names.len() - 1])).expect("read"))
            .expect("json");
    assert_eq!(last["n"], json!(MAX_REVIEW_RECORDS + 1));
}
