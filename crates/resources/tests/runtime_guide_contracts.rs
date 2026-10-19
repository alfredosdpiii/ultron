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
