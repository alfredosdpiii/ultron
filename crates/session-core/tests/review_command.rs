//! `/review` expansion (packages/coding-agent/src/ultron/review.ts `expandReviewCommand`, applied by the worker's
//! AgentController to `prompt`, `steer`, `followUp` and `nextRun`). Replays crates/session-core/fixtures/
//! review_command.json, recorded from the TypeScript controller by generate-review-command.mjs, plus the ported
//! literal expectations of packages/coding-agent/test/ultron-review.test.ts ("/review command expansion").

use serde_json::{Value, json};
use session_core::core::{Op, QueueKind};
use session_core::review_command::{expand_client_op, expand_review_command, review_prompt};

fn cases() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/review_command.json")).unwrap();
    fixture["cases"].as_array().unwrap().clone()
}

#[test]
fn session_core_review_command_expands_like_the_typescript_controller() {
    let cases = cases();
    assert!(cases.len() >= 80);
    for case in cases {
        let text = case["text"].as_str().unwrap();
        let expected = case["message"].as_str().unwrap();
        assert_eq!(expand_review_command(text), expected, "{case}");
        // Every member hands the lane the same expansion (the worker's ops and controller members alike).
        let op = match case["operation"].as_str().unwrap() {
            "prompt" => Op::PromptAndWait {
                message: text.into(),
                images: Vec::new(),
            },
            kind => Op::Enqueue {
                kind: QueueKind::parse(kind).unwrap(),
                message: text.into(),
                images: vec![json!({"type": "image"})],
                entry_id: Some("e-1".into()),
            },
        };
        let expanded = expand_client_op(op.clone());
        let message = match (&expanded, op) {
            (Op::PromptAndWait { message, images }, Op::PromptAndWait { .. }) => {
                assert!(images.is_empty());
                message.clone()
            }
            (
                Op::Enqueue {
                    kind,
                    message,
                    images,
                    entry_id,
                },
                Op::Enqueue { kind: original, .. },
            ) => {
                assert_eq!(*kind, original);
                assert_eq!(images, &[json!({"type": "image"})]);
                assert_eq!(entry_id.as_deref(), Some("e-1"));
                message.clone()
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(message, expected, "{case}");
    }
}

#[test]
fn session_core_review_command_expands_a_plain_prompt_op_and_leaves_other_ops() {
    let expanded = expand_client_op(Op::Prompt {
        message: "/review main".into(),
        images: Vec::new(),
        queue: Some(QueueKind::FollowUp),
    });
    assert_eq!(
        expanded,
        Op::Prompt {
            message: review_prompt("main"),
            images: Vec::new(),
            queue: Some(QueueKind::FollowUp),
        }
    );
    let bash = Op::Bash {
        command: "/review".into(),
        exclude: false,
        id: None,
    };
    assert_eq!(expand_client_op(bash.clone()), bash);
}

/// ultron-review.test.ts "/review becomes a one-cell prompt ..." and "--post adds the explicit-confirmation rule".
#[test]
fn session_core_review_command_ported_typescript_expectations() {
    let prompt = expand_review_command("/review");
    assert!(prompt.contains("review = await review_api.run(rlm, \"\")"));
    assert!(prompt.contains("print(review.report)"));
    assert!(!prompt.contains("post"));
    let with_args = expand_review_command("/review main --only sec,bugs \"src/a b.py\"");
    assert_eq!(
        with_args.split('\n').next().unwrap(),
        "/review main --only sec,bugs \"src/a b.py\""
    );
    assert!(with_args.contains(r#"review_api.run(rlm, "main --only sec,bugs \"src/a b.py\"")"#));
    let post = review_prompt("12 --post");
    let rule = post.find("only after I answer yes").unwrap();
    assert!(post[rule..].contains("confirm=True"));
    assert_eq!(expand_review_command("/reviewer x"), "/reviewer x");
    assert_eq!(expand_review_command("please /review this"), "please /review this");
}
