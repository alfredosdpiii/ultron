//! The Rust runtime guide names spawn contracts (decision row 210) only when the host serves them (the durable APIs
//! option; `ULTRON_GUIDE=ts` keeps the TypeScript text). New in Rust: literal expectations.

use resources::runtime_guide::{RuntimeGuideOptions, rlm_runtime_prompt};

#[test]
fn resources_runtime_guide_names_spawn_contracts_only_with_the_rust_apis() {
    let plain = rlm_runtime_prompt(&["rlm"], &RuntimeGuideOptions::default()).unwrap();
    let rust = rlm_runtime_prompt(
        &["rlm"],
        &RuntimeGuideOptions {
            durable_apis: true,
            ..Default::default()
        },
    )
    .unwrap();
    let sentence = r#"Contracts: `rlm.spawn(brief, name=..., contract={"checks": [{"run": "cargo test -q"}, {"changed_files_within": ["src/**"]}]}, hop={"retry": 1})` makes the host run the checks itself when the child ends, and its result's `check["contract"]["verdict"]` is "verified" or "contract_failed" (`help(rlm.spawn)`)."#;
    assert!(rust.contains(sentence), "{rust}");
    assert!(!plain.contains("contract_failed"));
    // It is in the delegation section.
    let delegation = rust.find("## Delegation").unwrap();
    let next = delegation + rust[delegation..].find("\n## ").unwrap();
    assert!(rust[delegation..next].contains(sentence));
}

/// The `schedule` API (kernel_apis::prompt_schedule, Rust host only) is one pointer line in the Rust guide's other
/// APIs; the TypeScript guide never names it.
#[test]
fn resources_runtime_guide_points_at_schedule_only_with_the_rust_apis() {
    let plain = rlm_runtime_prompt(&["rlm"], &RuntimeGuideOptions::default()).unwrap();
    let rust = rlm_runtime_prompt(
        &["rlm"],
        &RuntimeGuideOptions {
            durable_apis: true,
            ..Default::default()
        },
    )
    .unwrap();
    let line = r#"- `await schedule.every("1h", prompt)` or `schedule.at(when, prompt)` sends this session `prompt` later as a new turn (`help(schedule)`)."#;
    assert!(rust.contains(&format!("{line}\n- `await diagram.mermaid(")), "{rust}");
    assert!(!plain.contains("schedule.every"));
    assert_eq!(rust.len() - rust.replace(line, "").len(), line.len());
}

/// `diagram.mermaid` (kernel_apis::diagram, Rust host only) is the last pointer line of the Rust guide; the
/// TypeScript guide never names it.
#[test]
fn resources_runtime_guide_points_at_diagram_mermaid_only_with_the_rust_apis() {
    let plain = rlm_runtime_prompt(&["rlm"], &RuntimeGuideOptions::default()).unwrap();
    let rust = rlm_runtime_prompt(
        &["rlm"],
        &RuntimeGuideOptions {
            durable_apis: true,
            ..Default::default()
        },
    )
    .unwrap();
    let line = r#"- `await diagram.mermaid(text)` renders Mermaid to an SVG and PNG in your theme and shows the user the PNG; you get its path (`help(diagram)`)."#;
    assert!(rust.ends_with(&format!("\n{line}")), "{rust}");
    assert!(!plain.contains("diagram"));
    assert_eq!(rust.matches("diagram.mermaid").count(), 1);
}
