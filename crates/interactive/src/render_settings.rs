//! The render settings every transcript markdown reads, as TypeScript's components read them from module state:
//! `getCapabilities().hyperlinks` (OSC 8 links instead of `text (url)`) and interactive-mode.ts
//! `getMarkdownThemeWithSettings()`'s `codeBlockIndent` (the `markdown.codeBlockIndent` setting). Set once by the
//! TUI's thread before it paints; a thread that never sets them renders with the defaults (no OSC 8 links, two-space
//! code blocks).

use std::cell::RefCell;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderSettings {
    pub hyperlinks: bool,
    /// `None`: the default two spaces.
    pub code_block_indent: Option<String>,
}

thread_local! {
    static CURRENT: RefCell<RenderSettings> = RefCell::new(RenderSettings::default());
}

/// Use `settings` for everything this thread renders from now on.
pub fn set(settings: RenderSettings) {
    CURRENT.with(|current| *current.borrow_mut() = settings);
}

pub fn hyperlinks() -> bool {
    CURRENT.with(|current| current.borrow().hyperlinks)
}

pub fn code_block_indent() -> Option<String> {
    CURRENT.with(|current| current.borrow().code_block_indent.clone())
}
