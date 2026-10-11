//! Differential parity of the interactive conversation components against packages/coding-agent (TypeScript): the
//! fixture holds the TS components' lines for scripted messages, per theme (dark truecolor, light 256color) and width.
//! Fixture: fixtures/conversation-parity.json, produced by `node crates/interactive/fixtures/generate.mjs`.

use std::rc::Rc;

use interactive::word_diff::diff_words;
use interactive::{
    AssistantBlock, AssistantMessage, AssistantView, BashExecution, FooterModel, FooterView, SummaryMessage,
    ToolContent, ToolExecution, ToolRenderers, ToolResultView, UserMessage, format_key_text, key_text, render_diff,
    render_footer,
};
use serde_json::Value;
use tui::components::{Component, Text};
use tui::keybindings::{KeybindingsManager, Platform, app_keybindings};
use tui::keys::KeyContext;
use tui::theme::{ColorMode, Theme, builtin_theme_json, create_theme};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/conversation-parity.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn themes() -> Vec<Rc<Theme>> {
    [("dark", ColorMode::Truecolor), ("light", ColorMode::Ansi256)]
        .into_iter()
        .map(|(n, m)| Rc::new(create_theme(&builtin_theme_json(n).expect("theme"), m, None).expect("create")))
        .collect()
}

fn widths(f: &Value) -> Vec<usize> {
    f["widths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_u64().unwrap() as usize)
        .collect()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(s).collect()
}

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        KeyContext::default(),
    )
}

fn expand_key() -> String {
    key_text(&keys(), "app.tools.expand", false)
}

/// Renders every case of `section` with `make(case, theme)` at every width and compares with the recorded lines.
type Make<'a> = &'a dyn Fn(&Value, &Rc<Theme>) -> Box<dyn Component>;

fn check(section: &str, make: Make) {
    let f = fixture();
    let ws = widths(&f);
    let mut failures = Vec::new();
    let mut total = 0;
    for (ci, case) in f[section].as_array().unwrap().iter().enumerate() {
        for (theme, per_width) in themes().iter().zip(case["outputs"].as_array().unwrap()) {
            for (w, want) in ws.iter().zip(per_width.as_array().unwrap()) {
                total += 1;
                let got = make(case, theme).render(*w);
                let want = strings(want);
                if got != want {
                    failures.push(format!("{section}[{ci}] width {w}:\n got {got:?}\nwant {want:?}"));
                }
            }
        }
    }
    assert!(total > 0);
    assert!(
        failures.is_empty(),
        "{} of {total} differ; first: {}",
        failures.len(),
        failures[0]
    );
}

#[test]
fn interactive_key_text_matches_ts() {
    let f = fixture();
    let k = keys();
    assert_eq!(key_text(&k, "app.tools.expand", false), s(&f["keys"]["expand"]));
    assert_eq!(key_text(&k, "tui.select.cancel", false), s(&f["keys"]["cancel"]));
    assert_eq!(key_text(&k, "app.interrupt", false), s(&f["keys"]["interrupt"]));
    for row in f["keys"]["format"].as_array().unwrap() {
        assert_eq!(format_key_text(row[0].as_str().unwrap(), false, false), s(&row[1]));
        assert_eq!(format_key_text(row[0].as_str().unwrap(), true, false), s(&row[2]));
    }
    assert_eq!(format_key_text("alt+x/Alt+y", true, true), "Option+X/Option+Y");
}

#[test]
fn interactive_user_message_matches_ts() {
    check("user", &|c, t| {
        Box::new(UserMessage {
            theme: Rc::clone(t),
            text: s(&c["text"]),
            output_pad: c["pad"].as_u64().unwrap() as usize,
        })
    });
}

fn assistant_view(m: &Value) -> AssistantView {
    AssistantView {
        content: m["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| match b["type"].as_str().unwrap() {
                "text" => AssistantBlock::Text(s(&b["text"])),
                "thinking" => AssistantBlock::Thinking(s(&b["thinking"])),
                _ => AssistantBlock::ToolCall,
            })
            .collect(),
        stop_reason: s(&m["stopReason"]),
        error_message: m["errorMessage"].as_str().map(str::to_string),
    }
}

#[test]
fn interactive_assistant_message_matches_ts() {
    check("assistant", &|c, t| {
        Box::new(AssistantMessage {
            theme: Rc::clone(t),
            message: assistant_view(&c["m"]),
            hide_thinking: c["hide"].as_bool().unwrap_or(false),
            hidden_thinking_label: c["label"].as_str().unwrap_or("Thinking...").to_string(),
            output_pad: c["pad"].as_u64().unwrap_or(1) as usize,
            thinking_overrides: c["overrides"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|o| (o[0].as_u64().unwrap() as usize, o[1].as_bool().unwrap()))
                        .collect()
                })
                .unwrap_or_default(),
            code_highlight: None,
        })
    });
}

#[test]
fn interactive_summary_messages_match_ts() {
    check("summary", &|c, t| {
        Box::new(SummaryMessage {
            theme: Rc::clone(t),
            tokens_before: (c["kind"] == "compaction").then(|| c["tokensBefore"].as_u64().unwrap()),
            summary: s(&c["summary"]),
            expanded: c["expanded"].as_bool().unwrap(),
            expand_key: expand_key(),
        })
    });
}

#[test]
fn interactive_render_diff_matches_ts() {
    let f = fixture();
    let themes = themes();
    for case in f["diff"].as_array().unwrap() {
        for (theme, want) in themes.iter().zip(case["outputs"].as_array().unwrap()) {
            assert_eq!(
                render_diff(theme, case["diff"].as_str().unwrap()),
                s(want),
                "diff {:?}",
                case["diff"]
            );
        }
    }
}

#[test]
fn interactive_diff_words_matches_jsdiff() {
    let f = fixture();
    for case in f["words"].as_array().unwrap() {
        let got: Vec<(String, bool, bool)> = diff_words(case["old"].as_str().unwrap(), case["new"].as_str().unwrap())
            .into_iter()
            .map(|c| (c.value, c.added, c.removed))
            .collect();
        let want: Vec<(String, bool, bool)> = case["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    s(&c["value"]),
                    c["added"].as_bool().unwrap(),
                    c["removed"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(got, want, "{} -> {}", case["old"], case["new"]);
    }
}

fn tool_result(v: &Value) -> Option<ToolResultView> {
    (!v.is_null()).then(|| ToolResultView {
        content: v["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| match c["type"].as_str().unwrap() {
                "image" => ToolContent::Image {
                    data: s(&c["data"]),
                    mime_type: c["mimeType"].as_str().map(str::to_string),
                },
                _ => ToolContent::Text(s(&c["text"])),
            })
            .collect(),
        is_error: v["isError"].as_bool().unwrap(),
    })
}

/// Rust equivalents of the generator's renderers (`callR`, `resultR`, `thrower`).
fn renderers(def: &str) -> Option<ToolRenderers> {
    let call: interactive::CallRenderer = Box::new(|args, theme, _| {
        let code = args["code"].as_str().unwrap_or("?");
        Some(Box::new(Text::new(
            &theme.fg("accent", &format!("call {code}")).unwrap(),
            0,
            0,
            None,
        )))
    });
    let result: interactive::ResultRenderer = Box::new(|r, theme, st| {
        let parts: Vec<&str> = r
            .content
            .iter()
            .map(|c| match c {
                ToolContent::Text(t) => t.as_str(),
                ToolContent::Image { .. } => "image",
            })
            .collect();
        let text = format!(
            "res expanded={} partial={} {}",
            st.expanded,
            st.is_partial,
            parts.join("|")
        );
        Some(Box::new(Text::new(&theme.fg("toolOutput", &text).unwrap(), 0, 0, None)))
    });
    match def {
        "none" => None,
        "empty" => Some(ToolRenderers {
            self_shell: false,
            call: None,
            result: None,
        }),
        "both" | "self" => Some(ToolRenderers {
            self_shell: def == "self",
            call: Some(call),
            result: Some(result),
        }),
        "selfEmpty" => Some(ToolRenderers {
            self_shell: true,
            call: Some(Box::new(|_, _, _| Some(Box::new(Text::new("", 0, 0, None))))),
            result: None,
        }),
        _ => Some(ToolRenderers {
            self_shell: false,
            call: Some(Box::new(|_, _, _| None)),
            result: Some(Box::new(|_, _, _| None)),
        }),
    }
}

#[test]
fn interactive_tool_execution_matches_ts() {
    check("tool", &|c, t| {
        let result = tool_result(&c["result"]);
        Box::new(ToolExecution {
            theme: Rc::clone(t),
            tool_name: "rlm".into(),
            args: c["args"].clone(),
            is_partial: result.is_none() || c["partial"].as_bool().unwrap_or(false),
            result,
            expanded: c["expanded"].as_bool().unwrap_or(false),
            show_images: true,
            execution_started: false,
            args_complete: false,
            renderers: renderers(c["def"].as_str().unwrap()),
            expand_key: expand_key(),
        })
    });
}

#[test]
fn interactive_bash_execution_matches_ts() {
    let cancel = key_text(&keys(), "tui.select.cancel", false);
    check("bash", &|c, t| {
        let mut b = BashExecution::new(
            Rc::clone(t),
            c["cmd"].as_str().unwrap(),
            c["exclude"].as_bool().unwrap_or(false),
            &expand_key(),
            &cancel,
        );
        for step in c["steps"].as_array().unwrap() {
            match step[0].as_str().unwrap() {
                "append" => b.append_output(step[1].as_str().unwrap()),
                "expand" => b.expanded = true,
                _ => b.set_complete(
                    step[1].as_i64(),
                    step[2].as_bool().unwrap(),
                    step.get(3).and_then(Value::as_bool).unwrap_or(false),
                    step.get(4).and_then(Value::as_str),
                ),
            }
        }
        Box::new(b)
    });
}

struct Footer(Rc<Theme>, FooterView);

impl Component for Footer {
    fn render(&mut self, width: usize) -> Vec<String> {
        render_footer(&self.0, &self.1, width)
    }
}

fn footer_view(c: &Value) -> FooterView {
    let u = &c["usage"];
    let n = |k: &str| u[k].as_u64().unwrap();
    let prompt = n("input") + n("cacheRead") + n("cacheWrite");
    let model = c["model"].as_object().map(|m| FooterModel {
        id: s(&m["id"]),
        provider: s(&m["provider"]),
        reasoning: m["reasoning"].as_bool().unwrap(),
        context_window: m["contextWindow"].as_u64().unwrap(),
    });
    let percent = c.get("percent").map(Value::as_f64);
    FooterView {
        cwd: s(&c["cwd"]),
        home: Some("/home/u".into()),
        git_branch: c["branch"].as_str().map(str::to_string),
        session_name: c["name"].as_str().map(str::to_string),
        input: n("input"),
        output: n("output"),
        cache_read: n("cacheRead"),
        cache_write: n("cacheWrite"),
        cost: u["cost"].as_f64().unwrap(),
        latest_cache_hit_rate: (prompt > 0).then(|| n("cacheRead") as f64 / prompt as f64 * 100.0),
        context_window: percent.map(|_| {
            c["contextWindow"]
                .as_u64()
                .or(model.as_ref().map(|m| m.context_window))
                .unwrap_or(0)
        }),
        context_percent: percent,
        model,
        thinking_level: s(&c["thinking"]),
        using_subscription: c["subscription"].as_bool().unwrap_or(false),
        provider_count: c["providers"].as_u64().unwrap() as usize,
        auto_compact: c["autoCompact"].as_bool().unwrap_or(true),
        experimental: false,
        statuses: c["statuses"]
            .as_array()
            .map(|a| a.iter().map(|p| (s(&p[0]), s(&p[1]))).collect())
            .unwrap_or_default(),
    }
}

#[test]
fn interactive_footer_matches_ts() {
    check("footer", &|c, t| Box::new(Footer(Rc::clone(t), footer_view(c))));
}

/// Literal values from node 26 (`Number.prototype.toFixed`, footer.ts formatTokens, `toLocaleString`).
#[test]
fn interactive_number_formatting_matches_js() {
    use interactive::{format_cwd_for_footer, format_tokens, group_thousands, js_to_fixed};
    let fixed: Vec<String> = [0.25, 1.005, 2.5, 0.05, 99.95, -0.04]
        .iter()
        .map(|x| js_to_fixed(*x, 1))
        .collect();
    assert_eq!(fixed, ["0.3", "1.0", "2.5", "0.1", "100.0", "-0.0"]);
    assert_eq!(js_to_fixed(1.0005, 3), "1.000");
    assert_eq!(js_to_fixed(0.0125, 3), "0.013");
    let tokens: Vec<String> = [
        999, 1000, 9999, 10500, 999_499, 999_500, 1_050_000, 9_999_999, 10_000_000, 25_000_000,
    ]
    .iter()
    .map(|n| format_tokens(*n))
    .collect();
    assert_eq!(
        tokens,
        [
            "999", "1.0k", "10.0k", "11k", "999k", "1000k", "1.1M", "10.0M", "10M", "25M"
        ]
    );
    assert_eq!(group_thousands(1_234_567), "1,234,567");
    assert_eq!(group_thousands(100), "100");
    assert_eq!(format_cwd_for_footer("/home/u/a/", Some("/home/u")), "~/a");
    assert_eq!(format_cwd_for_footer("/home/u", Some("/home/u/")), "~");
    assert_eq!(format_cwd_for_footer("/home/user2", Some("/home/u")), "/home/user2");
    assert_eq!(format_cwd_for_footer("/x", None), "/x");
}

/// chalk's nesting and line-feed handling, from chalk 6 `applyStyle` (literal expectations).
#[test]
fn interactive_chalk_styles_match_chalk() {
    use interactive::{bold, inverse, italic};
    assert_eq!(bold(""), "");
    assert_eq!(bold("a\x1b[22mb"), "\x1b[1ma\x1b[22m\x1b[1mb\x1b[22m");
    assert_eq!(
        italic("a\nb\r\nc"),
        "\x1b[3ma\x1b[23m\n\x1b[3mb\x1b[23m\r\n\x1b[3mc\x1b[23m"
    );
    assert_eq!(inverse("x"), "\x1b[7mx\x1b[27m");
}

/// The rlm cell renderer inside `ToolExecution`. The highlighter replays the TS `highlightCode` output recorded for
/// that case and theme, after checking it is asked to highlight the same text.
#[test]
fn interactive_rlm_cell_matches_ts() {
    let f = fixture();
    let ws = widths(&f);
    let themes = themes();
    let mut total = 0;
    for (ci, case) in f["rlm"].as_array().unwrap().iter().enumerate() {
        for (ti, (theme, per_width)) in themes.iter().zip(case["outputs"].as_array().unwrap()).enumerate() {
            let hl = case["hl"][ti].clone();
            for (w, want) in ws.iter().zip(per_width.as_array().unwrap()) {
                total += 1;
                let hl = hl.clone();
                let highlight: interactive::rlm_cell::Highlighter = Rc::new(move |code: &str| {
                    assert_eq!(code, s(&hl["input"]), "rlm[{ci}] highlighter input");
                    strings(&hl["lines"])
                });
                let result = tool_result(&case["result"]);
                let mut t = ToolExecution {
                    theme: Rc::clone(theme),
                    tool_name: "rlm".into(),
                    args: case["args"].clone(),
                    is_partial: result.is_none() || case["partial"].as_bool().unwrap_or(false),
                    result,
                    expanded: case["expanded"].as_bool().unwrap_or(false),
                    show_images: true,
                    execution_started: false,
                    args_complete: case["complete"].as_bool().unwrap_or(false),
                    renderers: Some(interactive::rlm_cell::rlm_renderers(theme, highlight, &expand_key())),
                    expand_key: expand_key(),
                };
                assert_eq!(t.render(*w), strings(want), "rlm[{ci}] theme {ti} width {w}");
            }
        }
    }
    assert!(total > 100);
}

/// `outputTail` keeps the last visual lines and counts the rest; literal expectations from the TS function.
#[test]
fn interactive_rlm_output_tail_counts_wrapped_rows() {
    use interactive::rlm_cell::output_tail;
    let id = |s: &str| s.to_string();
    let (tail, skipped) = output_tail("abcdefgh\nij\nk", &id, 3, 4);
    assert_eq!(tail, ["efgh", "ij  ", "k   "]);
    assert_eq!(skipped, 1);
    let (tail, skipped) = output_tail("a\tb", &id, 5, 10);
    assert_eq!(tail, ["a   b     "]);
    assert_eq!(skipped, 0);
    let (tail, skipped) = output_tail("x\ny", &id, 0, 4);
    assert!(tail.is_empty());
    assert_eq!(skipped, 2);
}
