//! Transcript rendering of an `rlm` cell (packages/coding-agent/src/experimental/rlm-tool-renderer.ts): the cell's
//! Python with a line-number gutter and its output. Collapsed, the code shows its first lines and the output its last
//! visual lines, each with an expand-key hint; expanded, both are shown in full. Syntax highlighting is injected
//! (`highlightCode(code, "python")` in TS); the product's is [`crate::rlm::highlight::rlm_highlighter`].

use std::rc::Rc;

use serde_json::Value;
use tui::components::{Component, Container, Text};
use tui::theme::Theme;
use tui::utils::{js_trim_end, truncate_to_width, visible_width, wrap_text_with_ansi};

use crate::components::{
    ToolRenderState, ToolRenderers, ToolResultView, bold, fg, key_hint, tool_text_output_with, truncate_to_visual_lines,
};

/// Code lines shown while collapsed.
pub const RLM_CODE_PREVIEW_LINES: usize = 12;
/// Output visual lines shown while collapsed (the tail, where results and tracebacks end).
pub const RLM_OUTPUT_PREVIEW_LINES: usize = 10;

/// `highlightCode(code, "python")`: one styled string per line of `code`.
pub type Highlighter = Rc<dyn Fn(&str) -> Vec<String>>;

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `outputTail`: the last `max` visual lines of `output` (each styled by `color`) at `width`, and how many came before.
pub fn output_tail(output: &str, color: &dyn Fn(&str) -> String, max: usize, width: usize) -> (Vec<String>, usize) {
    if output.contains('\x1b') || output.contains('\r') {
        let styled = output.split('\n').map(color).collect::<Vec<_>>().join("\n");
        return truncate_to_visual_lines(&styled, max, width, 0);
    }
    let mut tail = Vec::new();
    let mut total = 0;
    for line in output.split('\n').rev() {
        let plain = line.replace('\t', "   ");
        let line_width = visible_width(&plain);
        if line_width <= width {
            total += 1;
            if tail.len() < max {
                tail.push(format!("{}{}", color(&plain), " ".repeat(width - line_width)));
            }
            continue;
        }
        let wrapped = wrap_text_with_ansi(&color(&plain), width);
        total += wrapped.len();
        for row in wrapped.iter().rev() {
            if tail.len() >= max {
                break;
            }
            tail.push(format!("{row}{}", " ".repeat(width.saturating_sub(visible_width(row)))));
        }
    }
    tail.reverse();
    let skipped = total - tail.len();
    (tail, skipped)
}

fn render_call(
    args: &Value,
    theme: &Theme,
    state: &ToolRenderState,
    highlight: &Highlighter,
    expand_key: &str,
) -> Box<dyn Component> {
    let code = args.get("code").and_then(Value::as_str);
    let meta = code.map_or(String::new(), |c| {
        let n = c.split('\n').count();
        fg(theme, "muted", &format!(" python · {n} line{}", plural(n)))
    });
    let mut children: Vec<Box<dyn Component>> = vec![Box::new(Text::new(
        &format!("{}{meta}", fg(theme, "toolTitle", &bold("rlm"))),
        0,
        0,
        None,
    ))];
    let Some(code) = code.filter(|c| !c.trim_matches(tui::utils::is_whitespace_char).is_empty()) else {
        if !state.args_complete {
            children.push(Box::new(Text::new(&fg(theme, "toolOutput", "..."), 0, 0, None)));
        }
        return Box::new(Container { children });
    };
    let lines = highlight(&js_trim_end(code).replace('\t', "    "));
    let shown = if state.expanded {
        lines.len()
    } else {
        lines.len().min(RLM_CODE_PREVIEW_LINES)
    };
    let gutter = lines.len().to_string().len();
    let body = lines[..shown]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{} {line}", fg(theme, "dim", &format!("{:>gutter$} │", i + 1))))
        .collect::<Vec<_>>()
        .join("\n");
    let mut text = format!("\n{body}");
    let hidden = lines.len() - shown;
    if hidden > 0 {
        text.push_str(&format!(
            "\n{} {}{}",
            fg(theme, "muted", &format!("... ({hidden} more line{},", plural(hidden))),
            key_hint(theme, expand_key, "to expand"),
            fg(theme, "muted", ")")
        ));
    }
    children.push(Box::new(Text::new(&text, 0, 0, None)));
    Box::new(Container { children })
}

/// The collapsed output preview, laid out per width.
struct OutputPreview {
    theme: Rc<Theme>,
    output: String,
    color: &'static str,
    label: String,
    expand_key: String,
}

impl Component for OutputPreview {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = &self.theme;
        let color = |s: &str| fg(t, self.color, s);
        let (visual, skipped) = output_tail(&self.output, &color, RLM_OUTPUT_PREVIEW_LINES, width);
        let mut lines = vec![String::new(), self.label.clone()];
        if skipped > 0 {
            let hint = format!(
                "{} {}{}",
                fg(t, "muted", &format!("... ({skipped} earlier line{},", plural(skipped))),
                key_hint(t, &self.expand_key, "to expand"),
                fg(t, "muted", ")")
            );
            lines.push(truncate_to_width(&hint, width, "...", false));
        }
        lines.extend(visual);
        lines
    }
}

fn render_result(
    result: &ToolResultView,
    theme: &Rc<Theme>,
    state: &ToolRenderState,
    expand_key: &str,
) -> Box<dyn Component> {
    let output = js_trim_end(&tool_text_output_with(Some(result), state.inline_images)).to_string();
    let mut children: Vec<Box<dyn Component>> = Vec::new();
    if output.is_empty() {
        if state.is_partial {
            children.push(Box::new(Text::new(
                &format!("\n{}", fg(theme, "muted", "running…")),
                0,
                0,
                None,
            )));
        }
        return Box::new(Container { children });
    }
    let color = if state.is_error { "error" } else { "toolOutput" };
    let label = fg(
        theme,
        "muted",
        if state.is_error {
            "error"
        } else if state.is_partial {
            "output (running)"
        } else {
            "output"
        },
    );
    if state.expanded {
        let styled = output
            .split('\n')
            .map(|l| fg(theme, color, l))
            .collect::<Vec<_>>()
            .join("\n");
        children.push(Box::new(Text::new(&format!("\n{label}\n{styled}"), 0, 0, None)));
    } else {
        children.push(Box::new(OutputPreview {
            theme: Rc::clone(theme),
            output,
            color,
            label,
            expand_key: expand_key.to_string(),
        }));
    }
    Box::new(Container { children })
}

/// `rlmToolRenderers` for a `ToolExecution` of the `rlm` tool, drawing with `theme`.
pub fn rlm_renderers(theme: &Rc<Theme>, highlight: Highlighter, expand_key: &str) -> ToolRenderers {
    let call_key = expand_key.to_string();
    let result_key = expand_key.to_string();
    let result_theme = Rc::clone(theme);
    ToolRenderers {
        self_shell: false,
        call: Some(Box::new(move |args, theme, state| {
            Some(render_call(args, theme, state, &highlight, &call_key))
        })),
        result: Some(Box::new(move |result, _theme, state| {
            Some(render_result(result, &result_theme, state, &result_key))
        })),
    }
}
