//! The modes `/mode` switches between, and `/engineering`. Engineering mode puts the always-on skills
//! (`alwaysSkills`) in the system prompt; Default lists them like any other skill. A mode is the settings it writes;
//! the session's next request uses it.

pub struct Mode {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// The `engineering` setting this mode writes.
    pub engineering: bool,
}

pub const MODES: &[Mode] = &[
    Mode {
        id: "default",
        label: "Default",
        description: "Skills listed, none always on",
        engineering: false,
    },
    Mode {
        id: "engineering",
        label: "Engineering",
        description: "alwaysSkills in the system prompt",
        engineering: true,
    },
];

/// The mode the settings are in.
pub fn current(engineering: bool) -> &'static Mode {
    MODES.iter().find(|m| m.engineering == engineering).unwrap_or(&MODES[0])
}

/// A mode by id or label, ignoring case.
pub fn find(name: &str) -> Option<&'static Mode> {
    let name = name.trim();
    MODES
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(name) || m.label.eq_ignore_ascii_case(name))
}

/// What the app does once the client reports the settings' mode ([`crate::lane::LaneEvent::Mode`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModeRequest {
    /// `/mode`: the picker.
    Picker,
    /// `/engineering`: report the mode.
    Status,
    /// `/engineering true|false`, `/mode <name>`: write the mode.
    Set(bool),
}

/// What `/persona <arg>` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersonaArg {
    /// No argument: report the settings' persona.
    Report,
    /// `ultron` / `none`: write the global setting.
    Global(bool),
    /// `on` / `off` / `auto`: the session's own toggle.
    Session(&'static str),
}

/// `/persona`'s usage line.
pub const PERSONA_USAGE: &str = "Usage: /persona on|off|auto (this session) or ultron|none (the global setting)";

/// `/persona`'s argument: `None` for a usage error.
pub fn persona_arg(args: &str) -> Option<PersonaArg> {
    match args.trim().to_lowercase().as_str() {
        "" => Some(PersonaArg::Report),
        "ultron" => Some(PersonaArg::Global(true)),
        "none" => Some(PersonaArg::Global(false)),
        "on" => Some(PersonaArg::Session("on")),
        "off" => Some(PersonaArg::Session("off")),
        "auto" => Some(PersonaArg::Session("auto")),
        _ => None,
    }
}

/// `/persona` without an argument: the settings' persona.
pub fn persona_status(persona: Option<&str>) -> String {
    match persona {
        Some("ultron") => "Persona is ultron: personality.md is in the system prompt".into(),
        Some("none") | None => "Persona is off (/persona ultron turns it on)".into(),
        Some(other) => format!("Persona \"{other}\" is not known, so it is off: /persona ultron|none"),
    }
}

/// What the status line says after `/persona ultron|off` wrote the setting.
pub fn persona_changed(on: bool) -> String {
    if on {
        "Persona ultron on from the next message".into()
    } else {
        "Persona off from the next message".into()
    }
}

/// `/engineering`'s argument: `None` for a usage error, `Some(None)` to report.
pub fn engineering_arg(args: &str) -> Option<Option<bool>> {
    match args.trim().to_lowercase().as_str() {
        "" => Some(None),
        "true" | "on" | "1" => Some(Some(true)),
        "false" | "off" | "0" => Some(Some(false)),
        _ => None,
    }
}

fn named(always_skills: &[String]) -> String {
    if always_skills.is_empty() {
        "none: set alwaysSkills in settings.json".into()
    } else {
        always_skills.join(", ")
    }
}

/// `/engineering` without an argument (client-tui-pi-commands.ts `setEngineeringMode`).
pub fn status(engineering: bool, always_skills: &[String]) -> String {
    format!(
        "Engineering mode is {} (always-on skills: {})",
        if engineering { "on" } else { "off" },
        named(always_skills)
    )
}

/// What the status line says after the mode was written.
pub fn changed(engineering: bool, always_skills: &[String]) -> String {
    if engineering {
        format!(
            "Engineering mode on: always-on skills ({}) are in the system prompt from the next message",
            named(always_skills)
        )
    } else {
        "Engineering mode off: always-on skills are listed like other skills".into()
    }
}
