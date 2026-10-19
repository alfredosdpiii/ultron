//! Ported from omp packages/tui/test/editor-vim.test.ts (literal expectations): Vim mode driving the Editor.

use std::cell::RefCell;
use std::rc::Rc;

use tui::editor::Editor;
use tui::keybindings::{KeybindingsManager, tui_keybindings};
use tui::keys::KeyContext;
use tui::vim::VimMode;
use tui::vim_editor::VimEditor;

const ESC: &str = "\x1b";
const UP: &str = "\x1b[A";
const DOWN: &str = "\x1b[B";

struct T {
    ed: Editor,
    vim: Option<VimEditor>,
    submitted: Vec<String>,
}

impl T {
    fn plain() -> Self {
        let keys = KeybindingsManager::new(tui_keybindings(), Vec::new(), KeyContext::default());
        Self {
            ed: Editor::new(keys, KeyContext::default(), Box::new(str::to_string), 0),
            vim: None,
            submitted: Vec::new(),
        }
    }
    fn modal() -> Self {
        let mut t = Self::plain();
        t.vim = Some(VimEditor::new(KeyContext::default()));
        t
    }
    /// `vimEditor(text)`: Normal mode at the head of the buffer.
    fn new(text: &str) -> Self {
        let mut t = Self::modal();
        t.ed.set_text(text);
        t.keys(&[ESC, "g", "g"]);
        t
    }
    fn at_end(text: &str) -> Self {
        let mut t = Self::modal();
        t.ed.set_text(text);
        t
    }
    fn key(&mut self, data: &str) {
        let consumed = self.vim.as_mut().is_some_and(|v| v.handle_input(&mut self.ed, data));
        if !consumed && let Some(s) = self.ed.handle_input(data) {
            self.submitted.push(s);
        }
    }
    fn keys(&mut self, keys: &[&str]) {
        for k in keys {
            self.key(k);
        }
    }
    fn each(&mut self, s: &str) {
        for c in s.chars() {
            self.key(&c.to_string());
        }
    }
    fn text(&self) -> String {
        self.ed.text()
    }
    fn mode(&self) -> VimMode {
        self.vim.as_ref().map_or(VimMode::Insert, VimEditor::mode)
    }
    fn cursor(&self) -> (usize, usize) {
        self.ed.cursor()
    }
    fn pending(&self) -> String {
        self.vim.as_ref().unwrap().state.pending_text()
    }
    fn escape_claimed(&self) -> bool {
        self.vim.as_ref().is_some_and(VimEditor::consumes_escape)
    }
}

#[test]
fn tui_vim_editor_disabled_types_letters_and_never_claims_escape() {
    let mut t = T::plain();
    t.each("hjkldwvyGx");
    assert_eq!(t.text(), "hjkldwvyGx");
    assert_eq!(t.mode(), VimMode::Insert);
    let mut t = T::plain();
    t.ed.set_text("draft");
    t.key(ESC);
    assert!(!t.escape_claimed());
    assert_eq!(t.text(), "draft");
}

#[test]
fn tui_vim_editor_mode_switching() {
    let mut t = T::modal();
    t.key("hi");
    assert_eq!(t.text(), "hi");
    assert_eq!(t.mode(), VimMode::Insert);

    let mut t = T::modal();
    t.key("abc");
    t.key(ESC);
    assert_eq!(t.mode(), VimMode::Normal);
    t.key("z");
    assert_eq!(t.text(), "abc");

    let mut t = T::new("abc");
    assert_eq!(t.mode(), VimMode::Normal);
    assert!(!t.escape_claimed());
    t.key("2");
    assert!(t.escape_claimed());
    t.key(ESC);
    assert!(!t.escape_claimed());
    t.key("d");
    assert!(t.escape_claimed());

    let mut t = T::new("ab");
    t.keys(&["i", "X"]);
    assert_eq!(t.text(), "Xab");
    t.keys(&[ESC, "A", "Z"]);
    assert_eq!(t.text(), "XabZ");

    let mut t = T::new("one");
    t.keys(&["o", "two"]);
    assert_eq!(t.text(), "one\ntwo");
    t.keys(&[ESC, "O", "mid"]);
    assert_eq!(t.text(), "one\nmid\ntwo");
}

#[test]
fn tui_vim_editor_motions() {
    let mut t = T::new("alfa\nbeta");
    t.each("lljhk");
    assert_eq!(t.text(), "alfa\nbeta");

    let mut t = T::new("alfa beta");
    t.keys(&["$", "a", "!"]);
    assert_eq!(t.text(), "alfa beta!");
    t.keys(&[ESC, "0", "i", ">"]);
    assert_eq!(t.text(), ">alfa beta!");

    let mut t = T::new("alfa beta gamma");
    t.keys(&["w", "i", "<"]);
    assert_eq!(t.text(), "alfa <beta gamma");
    let mut t = T::new("alfa beta gamma");
    t.keys(&["w", "w", "b", "i", ">"]);
    assert_eq!(t.text(), "alfa >beta gamma");
    let mut t = T::new("alfa beta gamma delta");
    t.keys(&["3", "w", "i", "|"]);
    assert_eq!(t.text(), "alfa beta gamma |delta");

    let mut t = T::new("one\ntwo\nthree");
    t.keys(&["G", "A", "!"]);
    assert_eq!(t.text(), "one\ntwo\nthree!");
    t.keys(&[ESC, "g", "g", "I", ">"]);
    assert_eq!(t.text(), ">one\ntwo\nthree!");

    // k and arrows on the first line move the cursor instead of loading prompt history
    for keys in [["k", "k"], [UP, DOWN]] {
        let mut t = T::modal();
        t.ed.add_to_history("an older prompt");
        t.ed.set_text("current draft");
        t.key(ESC);
        t.keys(&keys);
        assert_eq!(t.text(), "current draft");
    }

    let mut t = T::at_end("alfa");
    t.keys(&[ESC, "\x7f"]);
    assert_eq!(t.text(), "alfa");
}

#[test]
fn tui_vim_editor_normal_mode_edits() {
    let mut t = T::new("abcd");
    t.key("x");
    assert_eq!(t.text(), "bcd");
    t.keys(&["2", "x"]);
    assert_eq!(t.text(), "d");

    let mut t = T::new("alfa beta");
    t.keys(&["w", "D"]);
    assert_eq!(t.text(), "alfa ");
    t.keys(&["0", "C"]);
    assert_eq!(t.mode(), VimMode::Insert);
    t.key("new");
    assert_eq!(t.text(), "new");

    let mut t = T::new("alfa beta\ngamma delta\nepsilon");
    t.keys(&["w", "2", "D"]);
    assert_eq!(t.text(), "alfa \nepsilon");
    let mut t = T::new("alfa beta\ngamma delta\nepsilon");
    t.keys(&["w", "2", "C"]);
    assert_eq!(t.mode(), VimMode::Insert);
    t.key("new");
    assert_eq!(t.text(), "alfa new\nepsilon");

    let mut t = T::new("alfa beta\nsecond line");
    t.keys(&["d", "w"]);
    assert_eq!(t.text(), "beta\nsecond line");
    t.keys(&["d", "d"]);
    assert_eq!(t.text(), "second line");

    let mut t = T::new("alfa beta");
    t.keys(&["c", "w"]);
    assert_eq!(t.mode(), VimMode::Insert);
    t.key("new");
    assert_eq!(t.text(), "new beta");

    let mut t = T::new("alfa beta\nsecond");
    t.keys(&["c", "c"]);
    assert_eq!(t.mode(), VimMode::Insert);
    t.key("fresh");
    assert_eq!(t.text(), "fresh\nsecond");

    let mut t = T::new("alfa beta");
    t.keys(&["d", "w"]);
    assert_eq!(t.text(), "beta");
    t.key("u");
    assert_eq!(t.text(), "alfa beta");

    let mut t = T::new("alfa\nbeta");
    t.keys(&["y", "y", "p"]);
    assert_eq!(t.text(), "alfa\nalfa\nbeta");
}

#[test]
fn tui_vim_editor_text_objects() {
    let mut t = T::new("alfa beta gamma");
    t.keys(&["w", "d", "i"]);
    assert_eq!(t.mode(), VimMode::Normal);
    t.key("w");
    assert_eq!(t.text(), "alfa  gamma");

    let cases: [(&str, &str, &str, &str); 9] = [
        ("alfa beta gamma", "w", "daw", "alfa gamma"),
        ("alfa beta", "w", "daw", "alfa"),
        ("alfa beta gamma", "", "d2aw", "gamma"),
        ("say \"hello there\" now", "", "di\"", "say \"\" now"),
        ("say \"hello there\" now", "", "da\"", "say now"),
        ("fn {\n  body\n}\ntail", "j", "da{", "fn \ntail"),
        ("one\ntwo\n\nthree", "", "dip", "\nthree"),
        ("one\ntwo\n\nthree", "", "dap", "three"),
        ("one\ntwo\n\nthree", "G", "dap", "one\ntwo"),
    ];
    for (text, pre, keys, want) in cases {
        let mut t = T::new(text);
        t.each(pre);
        t.each(keys);
        assert_eq!(t.text(), want, "{text:?} {pre}{keys}");
    }

    let mut t = T::new("alfa beta");
    t.each("ciw");
    assert_eq!(t.mode(), VimMode::Insert);
    t.key("x");
    assert_eq!(t.text(), "x beta");

    let mut t = T::new("say \"\" now");
    t.each("ci\"");
    assert_eq!(t.mode(), VimMode::Insert);
    t.key("hi");
    assert_eq!(t.text(), "say \"hi\" now");

    let mut t = T::new("call(one, nest(two), three)");
    t.each(&"l".repeat(15));
    t.each("di(");
    assert_eq!(t.text(), "call(one, nest(), three)");

    let mut t = T::new("alfa beta");
    t.each("viw");
    assert_eq!(t.mode(), VimMode::Visual);
    t.key("d");
    assert_eq!(t.text(), " beta");

    let mut t = T::new("alfa beta");
    t.keys(&["d", "i"]);
    assert_eq!(t.pending(), "di");
    assert!(t.escape_claimed());
    t.key(ESC);
    assert_eq!(t.pending(), "");
    t.key("w");
    assert_eq!(t.text(), "alfa beta");
}

#[test]
fn tui_vim_editor_visual_mode_and_clipboard_yank() {
    let mut t = T::new("alfa beta");
    t.key("v");
    assert_eq!(t.mode(), VimMode::Visual);
    t.each("llll");
    t.key("d");
    assert_eq!(t.text(), "beta");
    assert_eq!(t.mode(), VimMode::Normal);

    let yanked = Rc::new(RefCell::new(Vec::new()));
    let mut t = T::new("alfa beta");
    let sink = yanked.clone();
    t.vim.as_mut().unwrap().on_yank = Some(Box::new(move |s: &str| sink.borrow_mut().push(s.to_string())));
    t.each("vllly");
    assert_eq!(t.text(), "alfa beta");
    assert_eq!(*yanked.borrow(), ["alfa"]);
    assert_eq!(t.mode(), VimMode::Normal);
    t.each("$p");
    assert_eq!(t.text(), "alfa betaalfa");

    let mut t = T::new("alfa\nbeta\ngamma");
    t.each("vjd");
    assert_eq!(t.text(), "eta\ngamma");

    let mut t = T::new("alfa\nbeta\ngamma");
    t.each("lV");
    assert_eq!(t.mode(), VimMode::VisualLine);
    t.each("jd");
    assert_eq!(t.text(), "gamma");

    let mut t = T::new("alfa beta");
    t.each("vl");
    assert!(t.escape_claimed());
    t.key(ESC);
    assert_eq!(t.mode(), VimMode::Normal);
    assert_eq!(t.text(), "alfa beta");
}

/// A yank over a paste marker hands the clipboard the pasted content, not the marker.
#[test]
fn tui_vim_editor_yank_expands_paste_markers() {
    let yanked = Rc::new(RefCell::new(Vec::new()));
    let mut t = T::modal();
    let big = (1..=12).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    t.key(&format!("\x1b[200~{big}\x1b[201~"));
    let marker = t.text();
    assert!(marker.starts_with("[paste #1"), "{marker}");
    let sink = yanked.clone();
    t.vim.as_mut().unwrap().on_yank = Some(Box::new(move |s: &str| sink.borrow_mut().push(s.to_string())));
    t.keys(&[ESC, "y", "y"]);
    assert_eq!(*yanked.borrow(), [format!("{big}\n")]);
    assert_eq!(t.vim.as_ref().unwrap().register(), Some(format!("{marker}\n").as_str()));
    // x on the marker removes it whole
    t.keys(&["0", "x"]);
    assert_eq!(t.text(), "");
}

#[test]
fn tui_vim_editor_batched_input() {
    let mut t = T::new("alfa beta gamma");
    t.key("wwx");
    assert_eq!(t.text(), "alfa beta amma");
    let mut t = T::new("xyz");
    t.key("iabc");
    assert_eq!(t.text(), "abcxyz");
    assert_eq!(t.mode(), VimMode::Insert);
}

#[test]
fn tui_vim_editor_atomic_placeholders() {
    let mut t = T::new("see [Image #1, 800x600] here");
    t.key("v");
    t.each("lllllll");
    t.key("d");
    assert_eq!(t.text(), " here");

    let mut t = T::new("[Image #1, 800x600]!");
    t.key("x");
    assert_eq!(t.text(), "!");

    let mut t = T::new("ab[Image #1] tail");
    t.each("5rx");
    assert_eq!(t.text(), "ab[Image #1] tail");

    let mut t = T::new("[Image #1]x");
    t.each("Rz");
    assert_eq!(
        t.text(),
        "[Image #1]x",
        "replace mode never overwrites part of a placeholder"
    );
}

#[test]
fn tui_vim_editor_mode_chrome() {
    let mut t = T::new("alfa bravo charlie");
    assert_eq!(t.pending(), "");
    t.key("2");
    assert_eq!(t.pending(), "2");
    t.key("d");
    assert_eq!(t.pending(), "2d");
    t.key(ESC);
    assert_eq!(t.pending(), "");

    let mut t = T::new("one\ntwo\nthree");
    let sel = |t: &T| t.vim.as_ref().unwrap().indicator(&t.ed).selected_lines;
    assert_eq!(sel(&t), 0);
    t.key("V");
    assert_eq!(sel(&t), 1);
    t.key("j");
    assert_eq!(sel(&t), 2);
    let ind = t.vim.as_ref().unwrap().indicator(&t.ed);
    assert_eq!(ind.mode, VimMode::VisualLine);
    t.key(ESC);
    assert_eq!(sel(&t), 0);
}

#[test]
fn tui_vim_editor_desired_column() {
    let buf = "alfa bravo charlie\nxy\ndelta echo foxtrot";
    let mut t = T::new(buf);
    t.each(&"l".repeat(12));
    assert_eq!(t.cursor(), (0, 12));
    t.key("j");
    assert_eq!(t.cursor(), (1, 1));
    t.key("j");
    assert_eq!(t.cursor(), (2, 12));

    let mut t = T::new(buf);
    t.each(&"l".repeat(12));
    t.each("j0j");
    assert_eq!(t.cursor(), (2, 0));

    let mut t = T::new(buf);
    t.each(&"l".repeat(12));
    t.each("j1j");
    assert_eq!(t.cursor(), (2, 12));

    let mut t = T::new(buf);
    t.key("$");
    assert_eq!(t.cursor(), (0, 17));
    t.key("j");
    assert_eq!(t.cursor(), (1, 1));
    t.key("j");
    assert_eq!(t.cursor(), (2, 17));
}

#[test]
fn tui_vim_editor_find_and_till() {
    let mut t = T::new("abcde");
    t.each("fc");
    assert_eq!(t.cursor(), (0, 2));
    t.each("Fa");
    assert_eq!(t.cursor(), (0, 0));
    t.each("tc");
    assert_eq!(t.cursor(), (0, 1));
    t.each("$Tc");
    assert_eq!(t.cursor(), (0, 3));

    let mut t = T::new("abaca\nzzz");
    t.each("2fa");
    assert_eq!(t.cursor(), (0, 4));
    t.each("fz");
    assert_eq!(t.cursor(), (0, 4));

    let mut t = T::new("abcde");
    t.each("dfc");
    assert_eq!(t.text(), "de");
    let mut t = T::new("abcde");
    t.each("dtc");
    assert_eq!(t.text(), "cde");

    let mut t = T::new("a-b-c");
    t.each("f-");
    assert_eq!(t.cursor(), (0, 1));
    t.key(";");
    assert_eq!(t.cursor(), (0, 3));
    t.key(",");
    assert_eq!(t.cursor(), (0, 1));

    let mut t = T::new("a,b,c,d");
    t.each("t,");
    assert_eq!(t.cursor(), (0, 0));
    t.key(";");
    assert_eq!(t.cursor(), (0, 2));

    let mut t = T::new("a1b");
    t.key("f");
    assert!(t.escape_claimed());
    t.key(ESC);
    assert_eq!(t.cursor(), (0, 0));
    assert!(!t.escape_claimed());
    t.each("f1");
    assert_eq!(t.cursor(), (0, 1));

    let mut t = T::new("a😀b");
    t.keys(&["f", "😀"]);
    assert_eq!(t.cursor(), (0, 1));
}

#[test]
fn tui_vim_editor_replace() {
    let mut t = T::new("abcd");
    t.each("lr");
    assert!(t.escape_claimed());
    t.key("X");
    assert_eq!(t.text(), "aXcd");
    assert_eq!(t.mode(), VimMode::Normal);
    assert_eq!(t.cursor(), (0, 1));

    let mut t = T::new("abcd");
    t.each("3rz");
    assert_eq!(t.text(), "zzzd");
    assert_eq!(t.cursor(), (0, 2));
    t.each("$2rQ");
    assert_eq!(t.text(), "zzzd");

    let mut t = T::new("abcd");
    t.key("R");
    assert_eq!(t.mode(), VimMode::Replace);
    t.key("XY");
    assert_eq!(t.text(), "XYcd");
    t.key("\x7f");
    assert_eq!(t.text(), "Xbcd");
    t.key("Z");
    assert_eq!(t.text(), "XZcd");
    t.key(ESC);
    assert_eq!(t.mode(), VimMode::Normal);
    assert_eq!(t.cursor(), (0, 1));
    t.key("q");
    assert_eq!(t.text(), "XZcd");
    // one undo step covers the whole replace run
    t.key("u");
    assert_eq!(t.text(), "abcd");

    let mut t = T::new("ab");
    t.each("$R");
    t.key("YZ");
    assert_eq!(t.text(), "aYZ");

    let mut t = T::new("abc");
    t.key("R");
    t.key("xyz");
    t.key("\x1b[C");
    t.key("w");
    assert_eq!(t.text(), "xyzw");

    let mut t = T::new("abcd");
    t.key("RXY");
    assert_eq!(t.text(), "XYcd");
    assert_eq!(t.mode(), VimMode::Replace);

    let mut t = T::new("abc");
    t.keys(&["R", "X"]);
    assert_eq!(t.text(), "Xbc");
    t.keys(&["\x1b[H", "\x1b[3~", "\x1b[C", "\x7f"]);
    assert_eq!(t.text(), "bc");
    assert_eq!(t.mode(), VimMode::Replace);

    for keys in ["jrXj", "jRZ\x1bj"] {
        let mut t = T::new("abcdefghij\nab\nabcdefghij");
        t.each(&"l".repeat(8));
        t.each(keys);
        assert_eq!(t.cursor(), (2, 1), "{keys:?}");
    }
}

#[test]
fn tui_vim_editor_bracket_match_and_paragraphs() {
    let mut t = T::new("(a(b)c)");
    t.key("%");
    assert_eq!(t.cursor(), (0, 6));
    t.key("%");
    assert_eq!(t.cursor(), (0, 0));
    t.each("ll%");
    assert_eq!(t.cursor(), (0, 4));

    let mut t = T::new("(\n)");
    t.key("%");
    assert_eq!(t.cursor(), (1, 0));
    t.each("%d%");
    assert_eq!(t.text(), "");

    let mut t = T::new("ab(c");
    t.key("%");
    assert_eq!(t.cursor(), (0, 0));

    let mut t = T::new("say \"hi\"");
    t.each("f\"%");
    assert_eq!(t.cursor(), (0, 7));
    t.key("%");
    assert_eq!(t.cursor(), (0, 4));
    let mut t = T::new("say 'hi'");
    t.key("%");
    assert_eq!(t.cursor(), (0, 7));
    let mut t = T::new("say \\\"hi\\\"");
    t.key("%");
    assert_eq!(t.cursor(), (0, 0));

    let mut t = T::new("(a)\nb\n  c\nd");
    t.each("50%");
    assert_eq!(t.cursor(), (1, 0));
    t.each("75%");
    assert_eq!(t.cursor(), (2, 2));

    let mut t = T::new("aaa\n\nbbb\n\nccc");
    t.key("}");
    assert_eq!(t.cursor(), (1, 0));
    t.key("}");
    assert_eq!(t.cursor(), (3, 0));
    t.key("{");
    assert_eq!(t.cursor(), (1, 0));

    let mut t = T::new("aaa\n\nbbb");
    t.each("d}");
    assert_eq!(t.text(), "\nbbb");
    let mut t = T::new("hello world");
    t.each("d}");
    assert_eq!(t.text(), "");
    let mut t = T::new("one\ntwo");
    t.key("}");
    assert_eq!(t.cursor(), (1, 2));
    t.each("ggd}");
    assert_eq!(t.text(), "");

    let mut t = T::new("abcdefghij\nab");
    t.each(&"l".repeat(8));
    t.each("jkFaj");
    assert_eq!(t.cursor(), (1, 0));
}

#[test]
fn tui_vim_editor_join_and_indent() {
    let mut t = T::new("foo\n  bar");
    t.key("J");
    assert_eq!(t.text(), "foo bar");
    assert_eq!(t.cursor(), (0, 3));
    for (text, want) in [
        ("end.\nnext", "end.  next"),
        ("wow!\nnext", "wow!  next"),
        ("why?\nnext", "why?  next"),
        ("call(\n  )", "call()"),
        ("foo\n\nbar", "foo\nbar"),
        ("foo\n   \nbar", "foo\nbar"),
    ] {
        let mut t = T::new(text);
        t.key("J");
        assert_eq!(t.text(), want);
    }
    let mut t = T::new("foo \nbar\nbaz");
    t.key("J");
    assert_eq!(t.text(), "foo bar\nbaz");
    t.each("gg3J");
    assert_eq!(t.text(), "foo bar baz");
    let mut t = T::new("only");
    t.key("J");
    assert_eq!(t.text(), "only");

    let mut t = T::new("a\nb");
    t.each(">>");
    assert_eq!(t.text(), "  a\nb");
    assert_eq!(t.cursor(), (0, 2));
    t.each("2>>");
    assert_eq!(t.text(), "    a\n  b");
    t.each("gg<<");
    assert_eq!(t.text(), "  a\n  b");
    t.each("<<");
    assert_eq!(t.text(), "a\n  b");
    t.each("<<");
    assert_eq!(t.text(), "a\n  b");

    let mut t = T::new("a\nb\nc");
    t.each(">j");
    assert_eq!(t.text(), "  a\n  b\nc");
    let mut t = T::new("a\nb");
    t.each("Vj>");
    assert_eq!(t.text(), "  a\n  b");
    assert_eq!(t.mode(), VimMode::Normal);
    let mut t = T::new("one\ntwo\n\nthree");
    t.each(">ip");
    assert_eq!(t.text(), "  one\n  two\n\nthree");
    let mut t = T::new("one\n\ntwo");
    t.each(">G");
    assert_eq!(t.text(), "  one\n\n  two");
    // set_text expands tabs, so place a tab-indented line directly
    let mut t = T::new("");
    t.ed.apply_edit(vec!["\tx".into()], (0, 0), false);
    t.each(">>");
    assert_eq!(t.text(), "\t\tx");
}

#[test]
fn tui_vim_editor_pending_targets_and_submit() {
    let mut t = T::new("abc");
    t.keys(&["r", "\x1b[D"]);
    assert_eq!(t.text(), "abc");
    assert_eq!(t.pending(), "");
    let mut t = T::new("a b");
    t.keys(&["f", " "]);
    assert_eq!(t.cursor(), (0, 1));
    let mut t = T::new("abc");
    t.keys(&["r", "\r"]);
    assert_eq!(t.submitted, ["abc"]);
    assert_eq!(t.pending(), "");
}

#[test]
fn tui_vim_editor_clamps_normal_cursor_and_expands_token_starts() {
    let mut t = T::new("abc");
    t.each("$x");
    assert_eq!(t.text(), "ab");
    assert_eq!(t.cursor(), (0, 1), "a Normal cursor rests on the last grapheme");
    // x from inside a placeholder takes the whole placeholder
    let mut t = T::new("see [Image #1, 800x600] here");
    t.each("llllll");
    t.key("x");
    assert_eq!(t.text(), "see  here");
}

struct Slash;

impl tui::autocomplete::AutocompleteProvider for Slash {
    fn trigger_characters(&self) -> Vec<String> {
        vec!["/".into()]
    }
    fn get_suggestions(
        &self,
        lines: &[String],
        _line: usize,
        _col: usize,
        _force: bool,
    ) -> Option<tui::autocomplete::AutocompleteSuggestions> {
        lines[0]
            .starts_with('/')
            .then(|| tui::autocomplete::AutocompleteSuggestions {
                items: vec![tui::autocomplete::AutocompleteItem::new("/help")],
                prefix: lines[0].clone(),
            })
    }
}

#[test]
fn tui_vim_editor_first_escape_dismisses_autocomplete() {
    let mut t = T::modal();
    t.ed.set_autocomplete_provider(Box::new(Slash));
    t.key("/");
    t.ed.fire_autocomplete_debounce();
    assert!(t.ed.is_showing_autocomplete());
    t.key(ESC);
    assert!(!t.ed.is_showing_autocomplete());
    assert_eq!(t.mode(), VimMode::Insert);
    t.key(ESC);
    assert_eq!(t.mode(), VimMode::Normal);
}
