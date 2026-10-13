//! Ported from omp packages/tui/test/desktop-notify.test.ts, notifications.test.ts and terminal-capabilities.test.ts
//! (literal expectations), plus Ultron's OSC 777 and off-thread delivery.

use std::sync::{Arc, Mutex};

use tui::notify::*;

mod support;
use support::latch::Latch;

const UUID: &str = "123e4567-e89b-12d3-a456-426614174000";
const TMUX: &str = "/tmp/tmux-1000/default,1234,0";

#[derive(Default)]
struct FakeSink {
    writes: Vec<String>,
    spawns: Vec<Vec<String>>,
    spawn_fails: bool,
    desktop: Option<DesktopNotifier>,
}

impl NotifySink for FakeSink {
    fn write(&mut self, data: &str) {
        self.writes.push(data.to_string());
    }
    fn spawn(&mut self, argv: &[String]) -> bool {
        self.spawns.push(argv.to_vec());
        !self.spawn_fails
    }
    fn desktop_notifier(&mut self) -> Option<DesktopNotifier> {
        self.desktop.clone()
    }
}

fn omp(protocol: NotifyProtocol, env: &[(&str, &str)]) -> Notifier {
    let mut n = Notifier::new(TerminalId::Base, Env::from_pairs(env), false);
    n.protocol = protocol;
    n.app_name = "omp".into();
    n
}

fn text(s: &str) -> Message {
    Message::Text(s.into())
}

fn structured(title: &str, body: &str, kind: Option<&str>) -> Message {
    Message::Structured(Notification {
        title: Some(title.into()),
        body: Some(body.into()),
        kinds: kind.map(|k| vec![k.to_string()]).unwrap_or_default(),
        ..Notification::default()
    })
}

fn deliver(n: &mut Notifier, m: &Message) -> FakeSink {
    let mut sink = FakeSink::default();
    n.deliver(m, &mut sink);
    sink
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn tui_notify_formats_osc99_string_and_unconfirmed_structured() {
    let mut n = omp(NotifyProtocol::Osc99, &[]);
    assert_eq!(n.format(&text("hello")), "\x1b]99;;hello\x1b\\");
    assert_eq!(
        n.format(&structured("Session", "Complete", None)),
        "\x1b]99;;Session: Complete\x1b\\"
    );
}

#[test]
fn tui_notify_formats_confirmed_osc99_title_and_body_chunks() {
    let mut n = omp(NotifyProtocol::Osc99, &[]);
    n.osc99_confirmed = true;
    let m = Message::Structured(Notification {
        title: Some("Session".into()),
        body: Some("Complete".into()),
        id: Some("complete-1".into()),
        kinds: vec!["completion".into()],
        urgency: Some(Urgency::Normal),
        icon_name: Some("info".into()),
        sound: Some("info".into()),
        actions: Some("focus".into()),
        expires_ms: Some(5000),
    });
    assert_eq!(
        n.format(&m),
        "\x1b]99;i=complete-1:f=b21w:a=focus:u=1:t=Y29tcGxldGlvbg==:n=aW5mbw==:s=aW5mbw==:w=5000:d=0;Session\x1b\\\
         \x1b]99;i=complete-1:p=body;Complete\x1b\\"
    );
    let unsafe_title = Message::Structured(Notification {
        title: Some("Line 1\nLine 2".into()),
        id: Some("unsafe".into()),
        ..Notification::default()
    });
    assert_eq!(
        n.format(&unsafe_title),
        "\x1b]99;i=unsafe:f=b21w:e=1;TGluZSAxCkxpbmUgMg==\x1b\\"
    );
    // No id: a generated `<app>-<n>` id, incrementing per notification.
    let anon = structured("T", "", None);
    assert_eq!(n.format(&anon), "\x1b]99;i=omp-1:f=b21w;T\x1b\\");
    assert_eq!(n.format(&anon), "\x1b]99;i=omp-2:f=b21w;T\x1b\\");
    // An id that sanitizes to nothing, or to "0" (kitty's "no id"), is replaced too.
    for id in ["0", "!!"] {
        let m = Message::Structured(Notification {
            title: Some("T".into()),
            id: Some(id.into()),
            ..Notification::default()
        });
        assert!(n.format(&m).starts_with("\x1b]99;i=omp-"), "{id}");
    }
}

#[test]
fn tui_notify_osc99_chunks_long_payloads_at_2048_bytes() {
    let mut n = omp(NotifyProtocol::Osc99, &[]);
    n.osc99_confirmed = true;
    let long = "é".repeat(1025); // 2050 bytes: the 1025th char starts a second chunk
    let m = Message::Structured(Notification {
        title: Some(long),
        id: Some("x".into()),
        ..Notification::default()
    });
    let out = n.format(&m);
    let expected = format!(
        "\x1b]99;i=x:f=b21w:d=0;{}\x1b\\\x1b]99;i=x:f=b21w;é\x1b\\",
        "é".repeat(1024)
    );
    assert_eq!(out, expected);
}

#[test]
fn tui_notify_formats_osc9_bell_and_osc777() {
    let mut n = omp(NotifyProtocol::Osc9, &[]);
    assert_eq!(n.format(&text("ping")), "\x1b]9;ping\x1b\\");
    assert_eq!(
        n.format(&structured("Session", "Complete", None)),
        "\x1b]9;Session: Complete\x1b\\"
    );
    // Control characters can never end the OSC early.
    assert_eq!(n.format(&text("a\x1b]0;x\x07b")), "\x1b]9;a ]0;x b\x1b\\");
    let mut bell = omp(NotifyProtocol::Bell, &[]);
    assert_eq!(bell.format(&text("ping")), "\x07");
    let mut foot = omp(NotifyProtocol::Osc777, &[]);
    assert_eq!(foot.format(&text("ping")), "\x1b]777;notify;omp;ping\x1b\\");
    assert_eq!(
        foot.format(&structured("a;b", "c;d", None)),
        "\x1b]777;notify;a,b;c;d\x1b\\"
    );
}

#[test]
fn tui_notify_wraps_tmux_passthrough() {
    assert_eq!(
        wrap_tmux_passthrough("\x1b]99;;Hello\x1b\\"),
        "\x1bPtmux;\x1b\x1b]99;;Hello\x1b\x1b\\\x1b\\"
    );
}

#[test]
fn tui_notify_detects_terminal_ids() {
    let id = |pairs: &[(&str, &str)]| detect_terminal_id(&Env::from_pairs(pairs), None);
    assert_eq!(
        id(&[("TERM_PROGRAM", "rio"), ("COLORTERM", "truecolor")]),
        TerminalId::Rio
    );
    assert_eq!(TerminalId::Rio.notify_protocol(), NotifyProtocol::Bell);
    assert_eq!(
        id(&[("TERM_PROGRAM", "WarpTerminal"), ("COLORTERM", "truecolor")]),
        TerminalId::Warp
    );
    let monstar = id(&[("TERM", "monstar"), ("COLORTERM", "truecolor")]);
    assert_eq!(monstar, TerminalId::Monstar);
    assert_eq!(monstar.notify_protocol(), NotifyProtocol::Osc9);
    assert_eq!(
        id(&[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", ""),
            ("COLORTERM", "truecolor"),
            ("VTE_VERSION", "8400")
        ]),
        TerminalId::TrueColor
    );
    assert_eq!(
        id(&[("KITTY_WINDOW_ID", "1"), ("TERM_PROGRAM", "ghostty")]),
        TerminalId::Kitty
    );
    assert_eq!(TerminalId::Kitty.notify_protocol(), NotifyProtocol::Osc99);
    assert_eq!(id(&[("TERM", "xterm-ghostty")]), TerminalId::Ghostty);
    assert_eq!(id(&[("TERM", "foot")]), TerminalId::Foot);
    assert_eq!(TerminalId::Foot.notify_protocol(), NotifyProtocol::Osc777);
    assert_eq!(id(&[]), TerminalId::Base);
    // tmux replaces TERM_PROGRAM; the client terminal type recovers the identity.
    let tmux = Env::from_pairs(&[("TERM_PROGRAM", "tmux"), ("TMUX", TMUX)]);
    assert_eq!(detect_terminal_id(&tmux, Some("ghostty")), TerminalId::Ghostty);
    assert_eq!(
        parse_client_termtype("xterm-kitty(0.35.2)\n").as_deref(),
        Some("xterm-kitty")
    );
    assert_eq!(parse_client_termtype("WezTerm 20240203").as_deref(), Some("WezTerm"));
    assert_eq!(parse_client_termtype("ghostty"), Some("ghostty".into()));
    assert_eq!(parse_client_termtype("1bad"), None);
    assert_eq!(parse_client_termtype("a/b"), None);
}

#[test]
fn tui_notify_multiplexer_sessions_and_classification() {
    let inside = |m: Multiplexer, pairs: &[(&str, &str)]| m.is_inside(&Env::from_pairs(pairs));
    assert!(inside(Multiplexer::Herdr, &[("HERDR_ENV", "1")]));
    for k in ["HERDR_PANE_ID", "HERDR_TAB_ID", "HERDR_WORKSPACE_ID"] {
        assert!(inside(Multiplexer::Herdr, &[(k, "x")]));
        assert!(!inside(Multiplexer::Herdr, &[(k, "")]));
    }
    for (k, v) in [
        ("HERDR_ENV", "0"),
        ("HERDR_SOCKET_PATH", "/tmp/x"),
        ("HERDR_BIN_PATH", "/usr/bin/herdr"),
        ("HERDR_SESSION", "s1"),
    ] {
        assert!(!inside(Multiplexer::Herdr, &[(k, v)]));
    }
    assert!(inside(Multiplexer::Tmux, &[("TMUX", "/tmp/tmux,1,0")]));
    assert!(inside(Multiplexer::Screen, &[("STY", "1234.pts-0.host")]));
    assert!(inside(Multiplexer::Zellij, &[("ZELLIJ", "0")]));
    assert!(inside(Multiplexer::Cmux, &[("CMUX_WORKSPACE_ID", "workspace")]));
    assert!(inside(Multiplexer::Cmux, &[("CMUX_SURFACE_ID", "surface")]));
    assert!(inside(Multiplexer::Cmux, &[("CMUX_REMOTE_TRANSPORT", "ssh")]));
    assert!(!inside(Multiplexer::Cmux, &[("CMUX_SOCKET_PATH", "/tmp/cmux.sock")]));
    assert!(!inside(Multiplexer::Tmux, &[("TERM", "tmux-256color")]));
    assert!(inside(Multiplexer::Wmux, &[("WMUX", "1")]));
    assert!(!inside(Multiplexer::Wmux, &[("WMUX", "0")]));
    let orca = [("ORCA_PANE_KEY", "tab-1"), ("ORCA_WORKTREE_ID", "worktree-1")];
    assert!(inside(Multiplexer::Orca, &orca));
    assert!(!inside(
        Multiplexer::Orca,
        &[("ORCA_PANE_KEY", " "), ("ORCA_WORKTREE_ID", "worktree-1")]
    ));
    assert!(!inside(Multiplexer::Orca, &[("ORCA_PANE_KEY", "tab-1")]));

    let classify = |pairs: &[(&str, &str)]| classify_multiplexer(&Env::from_pairs(pairs));
    assert_eq!(classify(&[("TERM", "tmux-256color")]), Some(Multiplexer::Tmux));
    assert_eq!(
        classify(&[("HERDR_PANE_ID", "p1"), ("TMUX", "session")]),
        Some(Multiplexer::Herdr)
    );
    assert_eq!(
        classify(&[("TMUX", "session"), ("STY", "screen")]),
        Some(Multiplexer::Tmux)
    );
    assert_eq!(
        classify(&[("STY", "screen"), ("ZELLIJ", "session")]),
        Some(Multiplexer::Screen)
    );
    assert_eq!(classify(&[("TERM", "screen-256color")]), Some(Multiplexer::Screen));
    assert_eq!(classify(&orca), Some(Multiplexer::Orca));
    assert!(!is_inside_multiplexer(&Env::from_pairs(&orca)));
    let mut orca_tmux = orca.to_vec();
    orca_tmux.push(("TMUX", "session"));
    assert_eq!(classify(&orca_tmux), Some(Multiplexer::Tmux));
    assert!(is_inside_multiplexer(&Env::from_pairs(&orca_tmux)));
    let mut orca_term = orca.to_vec();
    orca_term.push(("TERM", "screen-256color"));
    assert_eq!(classify(&orca_term), Some(Multiplexer::Screen));
    assert_eq!(classify(&[("TERM_PROGRAM", "orca")]), None);
    assert_eq!(
        multiplexer_sessions(&Env::from_pairs(&[("ZELLIJ", "0"), ("HERDR_ENV", "1")])),
        vec![Multiplexer::Herdr, Multiplexer::Zellij]
    );
}

#[test]
fn tui_notify_routes_cmux_surface_with_explicit_argv() {
    let mut n = omp(NotifyProtocol::Osc99, &[("CMUX_SURFACE_ID", UUID)]);
    let sink = deliver(&mut n, &structured("--title=spoof", "--surface other", None));
    assert_eq!(
        sink.spawns,
        vec![s(&[
            "cmux",
            "notify",
            "--surface",
            UUID,
            "--title",
            "--title=spoof",
            "--body",
            "--surface other"
        ])]
    );
    assert!(sink.writes.is_empty());
}

#[test]
fn tui_notify_routes_herdr_pane_with_sounds_and_safe_titles() {
    let env = [("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w6:p1")];
    let mut n = omp(NotifyProtocol::Osc99, &env);
    let sink = deliver(&mut n, &structured("session", "Waiting for input", Some("ask")));
    assert_eq!(
        sink.spawns,
        vec![s(&[
            "herdr",
            "notification",
            "show",
            "session",
            "--body",
            "Waiting for input",
            "--sound",
            "request"
        ])]
    );
    assert!(sink.writes.is_empty());

    let sound = |m: &Message, n: &mut Notifier| {
        let sp = deliver(n, m).spawns.remove(0);
        sp[sp.iter().position(|a| a == "--sound").unwrap() + 1].clone()
    };
    assert_eq!(
        sound(&structured("session", "Complete", Some("completion")), &mut n),
        "done"
    );
    assert_eq!(
        sound(&structured("session", "Stopped with error", Some("error")), &mut n),
        "request"
    );
    assert_eq!(sound(&structured("session", "Reminder", Some("info")), &mut n), "none");
    let title = |m: &Message, n: &mut Notifier| deliver(n, m).spawns.remove(0)[3].clone();
    assert_eq!(
        title(&structured("--help", "Complete", Some("completion")), &mut n),
        "omp"
    );
    assert_eq!(
        title(&structured("-x session", "Complete", Some("completion")), &mut n),
        "-x session"
    );

    // HERDR_ENV dropped by a launcher: the pane id alone still routes.
    let mut n = omp(NotifyProtocol::Osc99, &[("HERDR_PANE_ID", "w6:p1")]);
    assert_eq!(deliver(&mut n, &text("x")).spawns.len(), 1);
}

#[test]
fn tui_notify_falls_back_to_terminal_sequence() {
    // Herdr without a pane id
    let mut n = omp(NotifyProtocol::Osc99, &[("HERDR_ENV", "1")]);
    let sink = deliver(&mut n, &text("no pane"));
    assert!(sink.spawns.is_empty());
    assert_eq!(sink.writes, s(&["\x1b]99;;no pane\x1b\\"]));
    // Herdr binary missing
    let mut n = omp(NotifyProtocol::Osc99, &[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w6:p1")]);
    let mut sink = FakeSink {
        spawn_fails: true,
        ..FakeSink::default()
    };
    n.deliver(&text("no binary"), &mut sink);
    assert_eq!(sink.spawns.len(), 1);
    assert_eq!(sink.writes, s(&["\x1b]99;;no binary\x1b\\"]));
    // cmux workspace or socket without a surface; option-like surface
    for env in [
        ("CMUX_WORKSPACE_ID", "workspace:1"),
        ("CMUX_SOCKET_PATH", "/tmp/cmux.sock"),
        ("CMUX_SURFACE_ID", "--help"),
    ] {
        let mut n = omp(NotifyProtocol::Osc99, &[env]);
        let sink = deliver(&mut n, &text("ping"));
        assert!(sink.spawns.is_empty(), "{env:?}");
        assert_eq!(sink.writes, s(&["\x1b]99;;ping\x1b\\"]));
    }
    // incomplete Herdr falls through to a concrete cmux surface
    let mut n = omp(NotifyProtocol::Osc99, &[("HERDR_ENV", "1"), ("CMUX_SURFACE_ID", UUID)]);
    let sink = deliver(&mut n, &text("ping"));
    assert_eq!(sink.spawns[0][0], "cmux");
    assert!(sink.writes.is_empty());
}

#[test]
fn tui_notify_routes_to_innermost_notifier() {
    type Case<'a> = (&'a [(&'a str, &'a str)], Option<&'a str>, &'a [&'a str]);
    let cases: [Case; 6] = [
        (&[("HERDR_PANE_ID", "w6:p1"), ("TMUX", TMUX)], Some("herdr"), &[]),
        (&[("CMUX_SURFACE_ID", UUID), ("TMUX", TMUX)], Some("cmux"), &[]),
        (&[("CMUX_SURFACE_ID", UUID), ("ZELLIJ", "0")], Some("cmux"), &[]),
        (
            &[("HERDR_ENV", "1"), ("TMUX", TMUX)],
            None,
            &["\x1bPtmux;\x1b\x1b]99;;ping\x1b\x1b\\\x1b\\\x07"],
        ),
        (
            &[("TMUX", TMUX), ("ZELLIJ", "0")],
            None,
            &["\x1bPtmux;\x1b\x1b]99;;ping\x1b\x1b\\\x1b\\\x07"],
        ),
        (
            &[
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "w6:p1"),
                ("CMUX_SURFACE_ID", UUID),
            ],
            Some("herdr"),
            &[],
        ),
    ];
    for (env, binary, writes) in cases {
        let mut n = omp(NotifyProtocol::Osc99, env);
        let sink = deliver(&mut n, &text("ping"));
        let bins: Vec<&str> = sink.spawns.iter().map(|a| a[0].as_str()).collect();
        assert_eq!(bins, binary.into_iter().collect::<Vec<_>>(), "{env:?}");
        assert_eq!(sink.writes, s(writes), "{env:?}");
    }
}

#[test]
fn tui_notify_tmux_and_zellij_in_band_rewrites() {
    let w = |p: NotifyProtocol, env: &[(&str, &str)]| deliver(&mut omp(p, env), &text("ping")).writes;
    assert_eq!(
        w(NotifyProtocol::Osc99, &[("TMUX", TMUX)]),
        s(&["\x1bPtmux;\x1b\x1b]99;;ping\x1b\x1b\\\x1b\\\x07"])
    );
    assert_eq!(w(NotifyProtocol::Bell, &[("TMUX", TMUX)]), s(&["\x07"]));
    assert_eq!(w(NotifyProtocol::Osc99, &[]), s(&["\x1b]99;;ping\x1b\\"]));
    assert_eq!(
        w(NotifyProtocol::Osc99, &[("ZELLIJ", "0")]),
        s(&["\x1b]99;;ping\x1b\\\x07"])
    );
    assert_eq!(w(NotifyProtocol::Bell, &[("ZELLIJ", "0")]), s(&["\x07"]));
    assert_eq!(w(NotifyProtocol::Osc9, &[]), s(&["\x1b]9;ping\x1b\\"]));
    assert_eq!(
        w(NotifyProtocol::Osc9, &[("TMUX", TMUX)]),
        s(&["\x1bPtmux;\x1b\x1b]9;ping\x1b\x1b\\\x1b\\\x07"])
    );
    assert_eq!(
        w(NotifyProtocol::Osc9, &[("ZELLIJ", "0")]),
        s(&["\x1b]9;ping\x1b\\\x07"])
    );
}

#[test]
fn tui_notify_suppressed_by_env() {
    for v in ["off", "0", "false"] {
        let mut n = omp(NotifyProtocol::Osc9, &[(NOTIFICATIONS_ENV, v)]);
        let sink = deliver(&mut n, &text("ping"));
        assert!(sink.writes.is_empty() && sink.spawns.is_empty());
    }
    let mut n = omp(NotifyProtocol::Osc9, &[(NOTIFICATIONS_ENV, "on")]);
    assert_eq!(deliver(&mut n, &text("ping")).writes.len(), 1);
}

#[test]
fn tui_notify_linux_desktop_session_gate() {
    let linux = Env::from_pairs(&[("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus")]);
    let never = |_: &str| false;
    assert!(has_linux_desktop_session("linux", &linux, &never));
    assert!(!has_linux_desktop_session("linux", &Env::default(), &never));
    assert!(!has_linux_desktop_session("macos", &linux, &never));
    assert!(!has_linux_desktop_session("windows", &linux, &never));
    let rt = Env::from_pairs(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
    assert!(has_linux_desktop_session("linux", &rt, &|p| p == "/run/user/1000/bus"));
    assert!(!has_linux_desktop_session("linux", &rt, &never));
}

#[test]
fn tui_notify_resolves_desktop_notifier() {
    let both = |n: &str| Some(format!("/usr/bin/{n}"));
    assert_eq!(
        resolve_desktop_notifier(&both),
        Some(DesktopNotifier {
            kind: DesktopNotifierKind::NotifySend,
            path: "/usr/bin/notify-send".into()
        })
    );
    let gdbus = |n: &str| (n == "gdbus").then(|| "/usr/bin/gdbus".to_string());
    assert_eq!(
        resolve_desktop_notifier(&gdbus).map(|d| d.kind),
        Some(DesktopNotifierKind::Gdbus)
    );
    assert_eq!(resolve_desktop_notifier(&|_| None), None);
    // `which` honors PATH order and executability.
    let dir = std::env::temp_dir().join(format!("tui-notify-which-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("notify-send");
    std::fs::write(&file, "#!/bin/sh\n").unwrap();
    assert_eq!(
        which("notify-send", Some(dir.to_str().unwrap())),
        None,
        "not executable"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            which("notify-send", Some(dir.to_str().unwrap())),
            Some(file.to_string_lossy().into_owned())
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn tui_notify_builds_desktop_commands() {
    let ns = DesktopNotifier {
        kind: DesktopNotifierKind::NotifySend,
        path: "/usr/bin/notify-send".into(),
    };
    let gd = DesktopNotifier {
        kind: DesktopNotifierKind::Gdbus,
        path: "/usr/bin/gdbus".into(),
    };
    assert_eq!(
        build_desktop_notify_command(&ns, &text("ping"), "omp"),
        s(&[
            "/usr/bin/notify-send",
            "--app-name",
            "omp",
            "--urgency=normal",
            "--expire-time=5000",
            "omp",
            "ping"
        ])
    );
    let crit = Message::Structured(Notification {
        title: Some("Session 12".into()),
        body: Some("Complete".into()),
        urgency: Some(Urgency::Critical),
        ..Notification::default()
    });
    assert_eq!(
        build_desktop_notify_command(&ns, &crit, "omp"),
        s(&[
            "/usr/bin/notify-send",
            "--app-name",
            "omp",
            "--urgency=critical",
            "--expire-time=5000",
            "Session 12",
            "Complete"
        ])
    );
    assert_eq!(
        build_desktop_notify_command(&ns, &structured("   ", "Waiting for input", None), "omp"),
        s(&[
            "/usr/bin/notify-send",
            "--app-name",
            "omp",
            "--urgency=normal",
            "--expire-time=5000",
            "omp",
            "Waiting for input"
        ])
    );
    let low = Message::Structured(Notification {
        title: Some("omp".into()),
        body: Some("ping".into()),
        urgency: Some(Urgency::Low),
        ..Notification::default()
    });
    assert_eq!(
        build_desktop_notify_command(&gd, &low, "omp"),
        s(&[
            "/usr/bin/gdbus",
            "call",
            "--session",
            "--dest",
            "org.freedesktop.Notifications",
            "--object-path",
            "/org/freedesktop/Notifications",
            "--method",
            "org.freedesktop.Notifications.Notify",
            "omp",
            "0",
            "",
            "omp",
            "ping",
            "[]",
            "{\"urgency\": <byte 0>}",
            "5000",
        ])
    );
}

#[test]
fn tui_notify_bell_terminal_fans_out_to_dbus_only_when_gated() {
    let ns = DesktopNotifier {
        kind: DesktopNotifierKind::NotifySend,
        path: "/usr/bin/notify-send".into(),
    };
    let run = |protocol, session: bool, env: &[(&str, &str)]| {
        let mut n = omp(protocol, env);
        n.desktop_session = session;
        let mut sink = FakeSink {
            desktop: Some(ns.clone()),
            ..FakeSink::default()
        };
        n.deliver(&structured("Session", "Complete", None), &mut sink);
        sink
    };
    let sink = run(NotifyProtocol::Bell, true, &[]);
    assert_eq!(sink.writes, s(&["\x07"]));
    assert_eq!(
        sink.spawns,
        vec![s(&[
            "/usr/bin/notify-send",
            "--app-name",
            "omp",
            "--urgency=normal",
            "--expire-time=5000",
            "Session",
            "Complete"
        ])]
    );
    assert!(run(NotifyProtocol::Bell, false, &[]).spawns.is_empty());
    assert!(
        run(NotifyProtocol::Bell, true, &[(NO_DESKTOP_NOTIFY_ENV, "1")])
            .spawns
            .is_empty()
    );
    assert!(run(NotifyProtocol::Osc99, true, &[]).spawns.is_empty());
    // no notifier binary: BEL only
    let mut n = omp(NotifyProtocol::Bell, &[]);
    n.desktop_session = true;
    let sink = deliver(&mut n, &text("ping"));
    assert_eq!(sink.writes, s(&["\x07"]));
    assert!(sink.spawns.is_empty());
}

#[test]
fn tui_notify_turn_messages() {
    assert_eq!(
        Message::turn(Some("fix tests"), TurnOutcome::Complete),
        Message::Structured(Notification {
            title: Some("fix tests".into()),
            body: Some("Complete".into()),
            kinds: vec!["completion".into()],
            urgency: Some(Urgency::Normal),
            actions: Some("focus".into()),
            ..Notification::default()
        })
    );
    let mut n = omp(NotifyProtocol::Osc9, &[]);
    assert_eq!(
        n.format(&Message::turn(None, TurnOutcome::Ask)),
        "\x1b]9;Waiting for input\x1b\\"
    );
    assert_eq!(
        n.format(&Message::turn(Some(" "), TurnOutcome::Error)),
        "\x1b]9;Stopped with error\x1b\\"
    );
    let mut n = omp(NotifyProtocol::Osc99, &[("HERDR_PANE_ID", "w6:p1")]);
    let sp = deliver(&mut n, &Message::turn(Some("s"), TurnOutcome::Complete)).spawns;
    assert_eq!(sp[0].last().map(String::as_str), Some("done"));
}

/// The sink blocks (a slow D-Bus activation); `notify` must still return at once and deliver in order.
#[test]
fn tui_notify_service_never_blocks_the_caller() {
    // The sink blocks until the test releases it: a `notify` that waited on it would hang, not merely be slow.
    struct SlowSink(Arc<Mutex<Vec<String>>>, Latch);
    impl NotifySink for SlowSink {
        fn write(&mut self, data: &str) {
            self.1.wait();
            self.0.lock().unwrap().push(data.to_string());
        }
        fn spawn(&mut self, _: &[String]) -> bool {
            true
        }
        fn desktop_notifier(&mut self) -> Option<DesktopNotifier> {
            None
        }
    }
    let written = Arc::new(Mutex::new(Vec::new()));
    let blocked = Latch::default();
    let svc = NotificationService::spawn(
        omp(NotifyProtocol::Osc99, &[]),
        SlowSink(written.clone(), blocked.clone()),
    );
    svc.notify(text("one"));
    svc.set_osc99_supported(true);
    svc.notify(Message::Structured(Notification {
        title: Some("two".into()),
        id: Some("t".into()),
        ..Notification::default()
    }));
    assert!(written.lock().unwrap().is_empty());
    blocked.open();
    svc.shutdown();
    assert_eq!(
        *written.lock().unwrap(),
        s(&["\x1b]99;;one\x1b\\", "\x1b]99;i=t:f=b21w;two\x1b\\"])
    );
}

/// The real sink spawns detached children; a missing binary is a silent false.
#[test]
fn tui_notify_process_sink_spawns_and_reports_missing_binaries() {
    let mut writes = Vec::new();
    let mut sink = ProcessSink::new(|d: &str| writes.push(d.to_string()), Some(String::new()));
    assert!(sink.spawn(&s(&["/bin/sh", "-c", "exit 0"])));
    assert!(!sink.spawn(&s(&["/nonexistent/ultron-notify-binary"])));
    assert!(!sink.spawn(&[]));
    assert_eq!(sink.desktop_notifier(), None);
    sink.write("x");
    drop(sink);
    assert_eq!(writes, s(&["x"]));
}
