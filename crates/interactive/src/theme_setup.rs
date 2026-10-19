//! The TUI's theme state (`InteractiveThemeController` of modes/interactive/theme/theme-controller.ts): the `theme`
//! setting (a name, or an automatic `light/dark` pair following the terminal's appearance), applying it at startup,
//! when `/settings` changes it, and previewing themes while the user browses them. Themes come from a
//! [`ThemeRegistry`]: the built-ins, `<agent dir>/themes/*.json` and registered ones.
//!
//! The terminal's appearance starts from `COLORFGBG` (`detectTerminalBackgroundFromEnv`) and is then asked for, as
//! TypeScript's `detectTerminalBackgroundTheme` does: without a theme setting, and with an automatic pair, the app sends
//! the OSC 11 background query (`ESC ] 11 ; ? BEL`); the reply arrives as input and the reported background wins
//! (`getThemeForRgbColor`, luminance >= 0.5 is light) when it arrives. Without a reply the `COLORFGBG` result stands.
//! An automatic pair follows the terminal live, as TypeScript's auto-sync does: the app turns on the color palette
//! notifications (`CSI ? 2031 h`) while the setting is a pair and asks for the current scheme (`CSI ? 996 n`) each
//! time it applies one; the terminal's `CSI ? 997 ; 1|2 n` reports arrive as input and switch the theme when they
//! arrive, and a scheme report wins over the background reply (`detectTerminalThemeForAuto`). Nothing waits for an
//! answer (TypeScript waits up to 100 ms at startup), so the input pump and the paint path never block.

use tui::theme::registry::ThemeRegistry;
use tui::theme::{
    TerminalTheme, Theme, detect_terminal_background_from_env, detect_terminal_background_theme,
    parse_auto_theme_setting, resolve_theme_setting,
};

/// What applying a setting did.
#[derive(Debug)]
pub struct Applied {
    /// The theme to draw with now.
    pub theme: Theme,
    /// A detected appearance worth saving as the `theme` setting (`settingsManager.setTheme`).
    pub save: Option<String>,
    /// An automatic pair was applied: ask the terminal for its current scheme (`queryTerminalColorScheme`).
    pub query_scheme: bool,
    /// Ask the terminal for its background color (`queryTerminalBackgroundColor`, OSC 11): no setting, or a pair.
    pub query_background: bool,
    /// `Failed to load theme "<name>": <error>\nFell back to dark theme.` (`showError`).
    pub error: Option<String>,
}

pub struct ThemeController {
    registry: ThemeRegistry,
    /// `currentThemeSetting ?? settings.getThemeSetting()`.
    setting: Option<String>,
    terminal: TerminalTheme,
    detection_confident: bool,
    active: Option<String>,
    /// `autoSyncEnabled`: the setting is an automatic pair, so the terminal's scheme reports switch the theme.
    auto_sync: bool,
    /// OSC 11 queries sent and not yet answered (`pendingOsc11BackgroundReplies`).
    background_queries: u32,
    /// A scheme report arrived since the last background query: it wins over the background reply.
    scheme_reported: bool,
}

/// `parseTerminalColorSchemeReport`: input made only of `CSI ? 997 ; 1 n` (dark) / `CSI ? 997 ; 2 n` (light)
/// reports; the last one counts.
pub fn parse_color_scheme_report(data: &str) -> Option<TerminalTheme> {
    let mut rest = data;
    let mut last = None;
    while !rest.is_empty() {
        let after = rest.strip_prefix("\x1b[?997;")?;
        let (scheme, tail) = after.split_at_checked(1)?;
        rest = tail.strip_prefix('n')?;
        last = Some(match scheme {
            "1" => TerminalTheme::Dark,
            "2" => TerminalTheme::Light,
            _ => return None,
        });
    }
    last
}

impl ThemeController {
    /// `setting`: the `theme` setting; `colorfgbg`: the `COLORFGBG` variable.
    pub fn new(registry: ThemeRegistry, setting: Option<String>, colorfgbg: Option<&str>) -> Self {
        let detection = detect_terminal_background_from_env(colorfgbg);
        ThemeController {
            registry,
            setting: setting.filter(|s| !s.is_empty()),
            terminal: detection.theme,
            detection_confident: detection.confidence == "high",
            active: None,
            auto_sync: false,
            background_queries: 0,
            scheme_reported: false,
        }
    }

    /// Whether the terminal's scheme reports are wanted now (`setTerminalColorSchemeNotifications`).
    pub fn auto_sync(&self) -> bool {
        self.auto_sync
    }

    /// `applyTerminalTheme`: a scheme report while an automatic pair is set picks the pair's side; the theme to
    /// draw with when it changed. A report after the setting stopped being a pair turns the sync off.
    pub fn terminal_scheme_changed(&mut self, terminal: TerminalTheme) -> Option<Theme> {
        if !self.auto_sync {
            return None;
        }
        self.scheme_reported = true;
        self.terminal = terminal;
        let Some((light, dark)) = parse_auto_theme_setting(self.setting.as_deref()) else {
            self.auto_sync = false;
            return None;
        };
        let name = if terminal == TerminalTheme::Light { light } else { dark };
        if self.active.as_deref() == Some(name.as_str()) {
            return None;
        }
        Some(self.apply_name(&name, false).0)
    }

    /// An OSC 11 query is waiting for its reply.
    pub fn expects_background(&self) -> bool {
        self.background_queries > 0
    }

    /// An OSC 11 reply (`consumeOsc11BackgroundResponse` resolving `detectTerminalBackgroundTheme`): the reported
    /// background is the terminal's appearance. Without a setting its theme applies and is saved (the detection is
    /// confident); with an automatic pair its side applies unless a scheme report came first. A reply nobody asked
    /// for, an unparseable one, or one after the setting became a name changes nothing. The theme to draw with and
    /// the setting to save, when something changed.
    pub fn background_reported(&mut self, rgb: Option<(u8, u8, u8)>) -> Option<Applied> {
        if self.background_queries == 0 {
            return None;
        }
        self.background_queries -= 1;
        let detection = detect_terminal_background_theme(Some(rgb?), None);
        if let Some((light, dark)) = parse_auto_theme_setting(self.setting.as_deref()) {
            if self.scheme_reported {
                return None;
            }
            self.terminal = detection.theme;
            let name = if detection.theme == TerminalTheme::Light {
                light
            } else {
                dark
            };
            if self.active.as_deref() == Some(name.as_str()) {
                return None;
            }
            return Some(Applied {
                theme: self.apply_name(&name, false).0,
                save: None,
                query_scheme: false,
                query_background: false,
                error: None,
            });
        }
        if self.setting.is_some() {
            return None;
        }
        self.terminal = detection.theme;
        let detected = self.terminal_theme();
        if self.detection_confident && self.active.as_deref() == Some(detected) {
            // `COLORFGBG` already chose and saved this theme.
            return None;
        }
        self.detection_confident = true;
        let (theme, _, ok) = self.apply_name(detected, false);
        Some(Applied {
            theme,
            save: ok.then(|| detected.to_string()),
            query_scheme: false,
            query_background: false,
            error: None,
        })
    }

    /// The name of the theme applied last (`theme.name`): the theme document's own name, `dark` after a fallback.
    pub fn active_theme_name(&self) -> Option<String> {
        self.registry.current().and_then(|t| t.name.clone())
    }

    /// `getTerminalTheme`.
    pub fn terminal_theme(&self) -> &'static str {
        match self.terminal {
            TerminalTheme::Light => "light",
            TerminalTheme::Dark => "dark",
        }
    }

    /// `getThemeSelection`.
    pub fn selection(&self) -> Option<String> {
        self.setting.clone().or_else(|| self.active.clone())
    }

    /// The registry's theme names (`getAvailableThemes`).
    pub fn available(&self) -> Vec<String> {
        self.registry.available_themes()
    }

    /// `applyThemeName`: the theme, falling back to dark on failure (with the error when `show_error`).
    fn apply_name(&mut self, name: &str, show_error: bool) -> (Theme, Option<String>, bool) {
        let result = self.registry.set_theme(name);
        let ok = result.is_ok();
        self.active = Some(if ok { name.to_string() } else { "dark".into() });
        let error = match result {
            Err(e) if show_error => Some(format!(
                "Failed to load theme \"{name}\": {e}\nFell back to dark theme."
            )),
            _ => None,
        };
        let theme = self.registry.current().cloned().unwrap_or_else(|| {
            let dark = tui::theme::builtin_theme_json("dark").expect("the dark theme");
            tui::theme::create_theme(&dark, self.registry.mode, None).expect("the dark theme resolves")
        });
        (theme, error, ok)
    }

    /// `applyFromSettings`: an automatic pair picks the terminal's side; a name applies as is; without a setting the
    /// detected appearance applies, and is saved when the detection was confident.
    pub fn apply_from_settings(&mut self) -> Applied {
        let setting = self.setting.clone();
        if let Some((light, dark)) = parse_auto_theme_setting(setting.as_deref()) {
            let name = if self.terminal == TerminalTheme::Light {
                light
            } else {
                dark
            };
            self.auto_sync = true;
            self.query_background();
            let (theme, error, _) = self.apply_name(&name, true);
            return Applied {
                theme,
                save: None,
                query_scheme: true,
                query_background: true,
                error,
            };
        }
        self.auto_sync = false;
        if let Some(setting) = setting {
            let (theme, error, _) = self.apply_name(&setting, true);
            return Applied {
                theme,
                save: None,
                query_scheme: false,
                query_background: false,
                error,
            };
        }
        self.query_background();
        let detected = self.terminal_theme();
        let (theme, _, ok) = self.apply_name(detected, false);
        Applied {
            theme,
            save: (ok && self.detection_confident).then(|| detected.to_string()),
            query_scheme: false,
            query_background: true,
            error: None,
        }
    }

    /// An OSC 11 query goes out: its reply is awaited, and scheme reports count again from here.
    fn query_background(&mut self) {
        self.background_queries += 1;
        self.scheme_reported = false;
    }

    /// `setThemeSetting` (a `/settings` change).
    pub fn set_setting(&mut self, setting: &str) -> Applied {
        self.setting = Some(setting.to_string());
        self.apply_from_settings()
    }

    /// `preview`: the theme a setting or name would show, without changing the setting; `None` when nothing
    /// resolves.
    pub fn preview(&mut self, setting: &str) -> Option<Theme> {
        let name = resolve_theme_setting(Some(setting), self.terminal).or_else(|| self.active.clone())?;
        let _ = self.registry.set_theme(&name);
        self.registry.current().cloned()
    }
}
