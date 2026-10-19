//! System prompt and resource assembly, ported from packages/coding-agent/src/core/{system-prompt,skills,
//! resource-loader}.ts and src/ultron/rlm/prompt.ts.

pub mod context_files;
pub mod frontmatter;
pub mod ignore;
pub mod paths;
pub mod persona;
pub mod prompt_files;
pub mod prompt_templates;
pub mod runtime_guide;
pub mod skills;
pub mod system_prompt;

/// A non-fatal problem found while loading resources (`ResourceDiagnostic`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// "warning" or "collision".
    pub kind: &'static str,
    pub message: String,
    pub path: Option<String>,
    /// For collisions: (winner path, loser path).
    pub collision: Option<(String, String)>,
}

impl Diagnostic {
    pub(crate) fn warning(message: impl Into<String>, path: Option<&str>) -> Self {
        Self {
            kind: "warning",
            message: message.into(),
            path: path.map(str::to_string),
            collision: None,
        }
    }
}

/// JS regex `\s`: Unicode White_Space without U+0085, plus U+FEFF.
pub fn js_space(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}
