//! Right-click paste in fullscreen (`handleRightClickPaste` of tui-alt-screen.ts): literal expectations of its gate
//! (`process.platform === "win32"`, `TERM_PROGRAM` not `vscode` in any case, a press of exactly button 2) with an
//! injected platform and environment, and of the viewport routing the press to the application instead of text
//! selection.

use std::time::Instant;

use tui::fullscreen::{FullscreenViewport, ViewportInput};
use tui::keybindings::{KeybindingsManager, tui_keybindings};
use tui::keys::KeyContext;
use tui::right_click_paste::{is_right_click_press, right_click_paste_enabled};

#[test]
fn tui_right_click_paste_is_on_only_on_windows_outside_vscode() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| pairs.iter().find(|(name, _)| *name == k).map(|(_, v)| v.to_string())
    };
    assert!(right_click_paste_enabled("win32", &env(&[])));
    assert!(right_click_paste_enabled("win32", &env(&[("TERM_PROGRAM", "WezTerm")])));
    assert!(right_click_paste_enabled("win32", &env(&[("TERM_PROGRAM", "")])));
    assert!(!right_click_paste_enabled("win32", &env(&[("TERM_PROGRAM", "vscode")])));
    assert!(!right_click_paste_enabled("win32", &env(&[("TERM_PROGRAM", "VSCode")])));
    assert!(!right_click_paste_enabled("linux", &env(&[])));
    assert!(!right_click_paste_enabled("darwin", &env(&[])));
}

#[test]
fn tui_right_click_paste_takes_a_bare_right_button_press() {
    assert!(is_right_click_press(2, false));
    assert!(!is_right_click_press(2, true));
    for button in [0, 1, 3, 2 | 4, 2 | 8, 2 | 16, 2 | 32, 2 | 64] {
        assert!(!is_right_click_press(button, false), "{button}");
    }
}

#[test]
fn tui_fullscreen_viewport_routes_a_right_click_to_paste_when_enabled() {
    let keys = KeybindingsManager::new(tui_keybindings(), Vec::new(), KeyContext::default());
    let now = Instant::now();
    let mut view = FullscreenViewport::default();
    assert!(!view.right_click_paste);
    // Off (every platform but Windows): the press is the selection's, which ignores other buttons.
    assert_eq!(view.handle_input("\x1b[<2;5;3M", &keys, now), ViewportInput::Consumed);
    view.right_click_paste = true;
    assert_eq!(
        view.handle_input("\x1b[<2;5;3M", &keys, now),
        ViewportInput::RightClickPaste
    );
    // The release, a right drag, a modified right click and a left press do not paste.
    for data in [
        "\x1b[<2;5;3m",
        "\x1b[<34;6;3M",
        "\x1b[<18;5;3M",
        "\x1b[<0;5;3M",
        "\x1b[<0;5;3m",
    ] {
        assert_eq!(view.handle_input(data, &keys, now), ViewportInput::Consumed, "{data:?}");
    }
}
