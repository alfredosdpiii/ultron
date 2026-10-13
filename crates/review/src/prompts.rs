//! Frame instructions built from the prompt texts (`review_prompts.py`'s functions).

use crate::prompt_text::{
    AUTOREVIEW_FINDER_EXTRA, CELL_METHOD, CELL_PLANNER_RULES, CHECK_CATALOGUE, COMPILED_PLANNER_RULES,
    COMPILED_TESTS_ALLOWED, COMPILED_TESTS_FORBIDDEN, DEEP_LENSES, DEEP_RULES, DEEP_TEST_EVIDENCE, DEEP_TEST_REQUESTS,
    FINDER_RULES, FINDER_SEVERITY, FINDING_RULES, REVIEWERS, RV_API, Reviewer, SEVERITY_RUBRIC,
};

/// The reviewer with this key.
pub fn reviewer(key: &str) -> Option<&'static Reviewer> {
    REVIEWERS.iter().find(|reviewer| reviewer.key == key)
}

/// A `/review` finder frame's instructions.
pub fn finder_task(reviewer: &Reviewer) -> String {
    finder_task_for(reviewer.title, reviewer.focus, reviewer.checklist)
}

/// A finder frame's instructions for a specialty given by its parts (a merged specialist has owned ones).
pub fn finder_task_for(title: &str, focus: &str, checklist: &[impl AsRef<str>]) -> String {
    let checklist = checklist
        .iter()
        .map(|item| format!("- {}", item.as_ref()))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{FINDER_RULES}\n\nYour specialty: {title}.\nYour question: {focus}\nLook in particular for:\n{checklist}")
}

/// The finder task with the automated review's severity rubric and extra fields.
pub fn autoreview_finder_task(reviewer: &Reviewer) -> String {
    autoreview_finder_task_for(reviewer.title, reviewer.focus, reviewer.checklist)
}

/// [`autoreview_finder_task`] for a specialty given by its parts.
pub fn autoreview_finder_task_for(title: &str, focus: &str, checklist: &[impl AsRef<str>]) -> String {
    let task = finder_task_for(title, focus, checklist).replace(FINDER_SEVERITY, "");
    format!("{task}\n\n{SEVERITY_RUBRIC}\n\n{AUTOREVIEW_FINDER_EXTRA}")
}

/// One deep investigator's instructions for `lens`; with `tests`, the host also serves test executions.
pub fn deep_task(lens: &str, tests: bool) -> Option<String> {
    let (_, lens_text) = DEEP_LENSES.iter().find(|(key, _)| *key == lens)?;
    let rules = crate::py::format(
        DEEP_RULES,
        &[
            (
                "executes",
                if tests {
                    ", and you run nothing yourself"
                } else {
                    " and nothing is executed"
                },
            ),
            ("finding_rules", FINDING_RULES),
            ("test_requests", if tests { DEEP_TEST_REQUESTS } else { "" }),
            ("test_evidence", if tests { DEEP_TEST_EVIDENCE } else { "" }),
        ],
    )
    .expect("DEEP_RULES names only these fields");
    Some(format!("{rules}\n\n{lens_text}\n\n{SEVERITY_RUBRIC}"))
}

/// The check catalogue as the planner reads it.
pub fn catalogue_text() -> String {
    let mut lines = vec![
        "Check catalogue (shapes that found real defects before; the host lists as T-items the ones whose trigger is \
         in this change, and each must have a check or be declared uncovered):"
            .to_string(),
    ];
    for shape in CHECK_CATALOGUE {
        lines.push(format!(
            "- {}: when {}. How: {}. Evidence: {}. Level: {}.",
            shape.key, shape.when, shape.how, shape.evidence, shape.level
        ));
    }
    lines.join("\n")
}

fn tests_text(tests: bool) -> &'static str {
    if tests {
        COMPILED_TESTS_ALLOWED
    } else {
        COMPILED_TESTS_FORBIDDEN
    }
}

/// The planner's instructions when it plans in sandboxed cells over the rv API.
pub fn cell_planner_task(tests: bool) -> String {
    format!(
        "{CELL_PLANNER_RULES}\n\n{CELL_METHOD}\n\n{}\n\n{RV_API}\n\n{SEVERITY_RUBRIC}\n\nFields of a finding about tests \
         or maintainability:\n{FINDING_RULES}\n\n{}",
        tests_text(tests),
        catalogue_text()
    )
}

/// The compiled planner's instructions: method, program language, rubric, finding rules and check catalogue.
pub fn compiled_planner_task(tests: bool) -> String {
    format!(
        "{COMPILED_PLANNER_RULES}\n\n{}\n\n{SEVERITY_RUBRIC}\n\nFields of a finding about tests or maintainability:\n\
         {FINDING_RULES}\n\n{}",
        tests_text(tests),
        catalogue_text()
    )
}
