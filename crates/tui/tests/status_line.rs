//! Ported from omp packages/tui/test/status-line-*.test.ts and the literal examples in pi-utils format.ts and
//! chrome/context-thresholds.ts; layout rows exercise omp's overflow order.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use tui::status_git::*;
use tui::status_line::*;

fn data() -> StatusData {
    StatusData {
        home: "/home/u".into(),
        cwd: "/home/u/x".into(),
        ..StatusData::default()
    }
}

fn seg(id: SegmentId, d: &StatusData) -> Option<String> {
    render_segment(id, d, &SegmentOptions::default(), &Symbols::unicode(), &Plain)
}

#[test]
fn tui_status_line_formats_numbers_and_durations() {
    for (n, s) in [
        (999, "999"),
        (1_000, "1K"),
        (1_500, "1.5K"),
        (9_999, "10K"),
        (25_000, "25K"),
        (1_000_000, "1M"),
        (1_500_000, "1.5M"),
        (25_000_000, "25M"),
        (1_500_000_000, "1.5B"),
        (25_000_000_000, "25B"),
    ] {
        assert_eq!(format_number(n), s, "{n}");
    }
    for (ms, s) in [
        (0, "0ms"),
        (123, "123ms"),
        (1_500, "1.5s"),
        (59_940, "59.9s"),
        (59_950, "1m"),
        (60_000, "1m"),
        (1_815_000, "30m15s"),
        (9_000_000, "2h30m"),
        (3_600_000, "1h"),
        (266_400_000, "3d2h"),
        (86_400_000, "1d"),
    ] {
        assert_eq!(format_duration(ms), s, "{ms}");
    }
}

#[test]
fn tui_status_line_context_thresholds_and_format() {
    assert_eq!(context_level(49.9, 200_000), ContextLevel::Normal);
    assert_eq!(context_level(50.0, 200_000), ContextLevel::Warning);
    assert_eq!(context_level(70.0, 200_000), ContextLevel::Purple);
    assert_eq!(context_level(90.0, 200_000), ContextLevel::Error);
    // 1M window: the token thresholds (150K / 270K / 500K) arrive first.
    assert_eq!(context_level(14.9, 1_000_000), ContextLevel::Normal);
    assert_eq!(context_level(15.0, 1_000_000), ContextLevel::Warning);
    assert_eq!(context_level(27.0, 1_000_000), ContextLevel::Purple);
    assert_eq!(context_level(50.0, 1_000_000), ContextLevel::Error);
    assert_eq!(context_level(95.0, 0), ContextLevel::Error);
    assert_eq!(context_level(0.0, 200_000), ContextLevel::Normal);
    assert_eq!(format_context_usage(Some(12.345), 200_000, 0), "12.3%/200K");
    assert_eq!(format_context_usage(None, 200_000, 0), "?/200K");
    assert_eq!(format_context_usage(Some(1.0), 0, 1_500), "1.5K/?");

    let mut d = data();
    d.context_tokens = 1_500;
    assert_eq!(
        seg(SegmentId::ContextPct, &d).as_deref(),
        Some("◫ 1.5K/?"),
        "unknown window shows tokens"
    );
    d.context_window = 200_000;
    assert_eq!(
        seg(SegmentId::ContextPct, &d).as_deref(),
        Some("◫ 200K"),
        "unknown usage shows the window"
    );
    d.context_percent = Some(55.0);
    d.auto_compact = true;
    assert_eq!(seg(SegmentId::ContextPct, &d).as_deref(), Some("◫ 55.0%/200K ⟲"));
    assert_eq!(seg(SegmentId::ContextTotal, &d).as_deref(), Some("◫ 200K"));
}

/// Roles flow to the style: the context color follows the level.
#[test]
fn tui_status_line_context_role_follows_level() {
    struct Tag;
    impl StatusStyle for Tag {
        fn fg(&self, role: Role, text: &str) -> String {
            format!("<{role:?}>{text}")
        }
    }
    let mut d = data();
    d.context_window = 200_000;
    let ascii = Symbols::ascii();
    let r = |d: &StatusData| render_segment(SegmentId::ContextPct, d, &SegmentOptions::default(), &ascii, &Tag);
    for (pct, role) in [(10.0, "Context"), (60.0, "Warning"), (75.0, "Purple"), (95.0, "Error")] {
        d.context_percent = Some(pct);
        assert_eq!(r(&d), Some(format!("ctx: <{role}>{pct:.1}%/200K")));
    }
}

#[test]
fn tui_status_line_cache_hit_rate_over_full_prompt() {
    let mut d = data();
    d.usage = Usage {
        cache_read: 800,
        cache_write: 0,
        input: 200,
        ..Usage::default()
    };
    assert_eq!(seg(SegmentId::CacheHit, &d).as_deref(), Some("💾 80.00%"));
    d.usage = Usage {
        cache_read: 600,
        cache_write: 300,
        input: 100,
        ..Usage::default()
    };
    assert_eq!(seg(SegmentId::CacheHit, &d).as_deref(), Some("💾 60.00%"));
    d.usage = Usage {
        input: 5_000,
        ..Usage::default()
    };
    assert_eq!(seg(SegmentId::CacheHit, &d), None);
}

#[test]
fn tui_status_line_cost_splits_subagent_spend() {
    let mut d = data();
    d.usage.cost = 0.5;
    d.usage.subagent_cost = 0.12;
    assert_eq!(seg(SegmentId::Cost, &d).as_deref(), Some("$0.38 (+0.12)"));
    d.usage = Usage {
        cost: 0.38,
        ..Usage::default()
    };
    assert_eq!(seg(SegmentId::Cost, &d).as_deref(), Some("$0.38"));
    d.usage.cost = 0.0;
    assert_eq!(seg(SegmentId::Cost, &d), None, "zero spend stays hidden");
    d.using_subscription = true;
    assert_eq!(seg(SegmentId::Cost, &d).as_deref(), Some("(sub)"));
    d.usage.cost = 1.25;
    assert_eq!(seg(SegmentId::Cost, &d).as_deref(), Some("S1.25"));
}

#[test]
fn tui_status_line_token_segments() {
    let mut d = data();
    assert_eq!(seg(SegmentId::TokenIn, &d), None);
    assert_eq!(seg(SegmentId::TokenRate, &d), None);
    d.usage = Usage {
        input: 12_345,
        output: 2_000,
        cache_read: 1_000_000,
        cache_write: 500,
        tokens_per_second: Some(42.06),
        ..Usage::default()
    };
    assert_eq!(seg(SegmentId::TokenIn, &d).as_deref(), Some("⤵ 12K"));
    assert_eq!(seg(SegmentId::TokenOut, &d).as_deref(), Some("⤴ 2K"));
    assert_eq!(seg(SegmentId::CacheRead, &d).as_deref(), Some("💾 1M"));
    assert_eq!(seg(SegmentId::CacheWrite, &d).as_deref(), Some("💾 500"));
    // total excludes cache reads (they re-read the whole context every turn)
    assert_eq!(seg(SegmentId::TokenTotal, &d).as_deref(), Some("🪙 15K"));
    assert_eq!(seg(SegmentId::TokenRate, &d).as_deref(), Some("⚡ 42.1 tok/s"));
}

#[test]
fn tui_status_line_vim_segment() {
    let mut d = data();
    assert_eq!(seg(SegmentId::Vim, &d), None);
    let vim = |mode, pending: &str, lines, display| VimStatus {
        mode,
        pending: pending.into(),
        selected_lines: lines,
        display,
    };
    for (mode, label) in [
        (VimMode::Normal, "NORMAL"),
        (VimMode::Insert, "INSERT"),
        (VimMode::Visual, "VISUAL"),
        (VimMode::VisualLine, "V-LINE"),
        (VimMode::Replace, "REPLACE"),
    ] {
        d.vim = Some(vim(mode, "", 0, VimDisplay::Text));
        assert_eq!(seg(SegmentId::Vim, &d).as_deref(), Some(label));
    }
    d.vim = Some(vim(VimMode::Normal, "2d", 0, VimDisplay::Text));
    assert_eq!(seg(SegmentId::Vim, &d).as_deref(), Some("NORMAL 2d"));
    d.vim = Some(vim(VimMode::VisualLine, "", 1, VimDisplay::Text));
    assert_eq!(seg(SegmentId::Vim, &d).as_deref(), Some("V-LINE"));
    d.vim = Some(vim(VimMode::VisualLine, "", 3, VimDisplay::Text));
    assert_eq!(seg(SegmentId::Vim, &d).as_deref(), Some("V-LINE 3L"));
    d.vim = Some(vim(VimMode::Normal, "", 0, VimDisplay::Icon));
    assert_eq!(seg(SegmentId::Vim, &d).as_deref(), Some("■"));
    d.vim = Some(vim(VimMode::Visual, "", 4, VimDisplay::Icon));
    assert_eq!(seg(SegmentId::Vim, &d).as_deref(), Some("◉ 4L"));
    d.vim = Some(vim(VimMode::Normal, "", 0, VimDisplay::None));
    assert_eq!(seg(SegmentId::Vim, &d), None);
}

#[test]
fn tui_status_line_model_and_thinking() {
    let mut d = data();
    assert_eq!(seg(SegmentId::Model, &d).as_deref(), Some("⬢ no-model"));
    d.model_name = Some("Claude Opus 4".into());
    assert_eq!(seg(SegmentId::Model, &d).as_deref(), Some("⬢ Opus 4"));
    d.thinking_level = Some("high".into());
    assert_eq!(seg(SegmentId::Model, &d).as_deref(), Some("⬢ Opus 4 · ◒ high"));
    d.thinking_level = Some("off".into());
    assert_eq!(seg(SegmentId::Model, &d).as_deref(), Some("⬢ Opus 4 · ⦸ off"));
    d.thinking_level = Some("auto".into());
    assert_eq!(seg(SegmentId::Model, &d).as_deref(), Some("⬢ Opus 4 · ⟳ auto"));
    let opts = SegmentOptions {
        show_thinking_level: false,
        ..SegmentOptions::default()
    };
    assert_eq!(
        render_segment(SegmentId::Model, &d, &opts, &Symbols::ascii(), &Plain).as_deref(),
        Some("[M] Opus 4")
    );
}

#[test]
fn tui_status_line_path_display() {
    let opts = PathOptions::default();
    assert_eq!(display_path("/home/u/Projects/ultron", "/home/u", &opts), "ultron");
    assert_eq!(display_path("/home/u/repos/a/b", "/home/u", &opts), "a/b");
    assert_eq!(display_path("/work/svc", "/home/u", &opts), "svc");
    assert_eq!(display_path("/home/u/other/x", "/home/u", &opts), "~/other/x");
    assert_eq!(display_path("/home/u", "/home/u", &opts), "~");
    assert_eq!(display_path("/tmp/scratch/a", "/home/u", &opts), "scratch/a");
    assert_eq!(display_path("/tmp", "/home/u", &opts), "/tmp");
    assert_eq!(display_path("/home/u/Projects", "/home/u", &opts), "~/Projects");
    assert_eq!(
        display_path("/home/user2/x", "/home/u", &opts),
        "/home/user2/x",
        "a sibling prefix is not home"
    );
    let no_strip = PathOptions {
        strip_work_prefix: false,
        ..PathOptions::default()
    };
    assert_eq!(
        display_path("/home/u/Projects/ultron", "/home/u", &no_strip),
        "~/Projects/ultron"
    );
    let raw = PathOptions {
        abbreviate: false,
        max_length: 10,
        strip_work_prefix: false,
    };
    assert_eq!(display_path("/home/u/Projects/ultron", "/home/u", &raw), "…ts/ultron");
}

#[test]
fn tui_status_line_git_pr_time_and_misc_segments() {
    let mut d = data();
    assert_eq!(seg(SegmentId::Git, &d), None);
    d.git_branch = Some("main".into());
    assert_eq!(seg(SegmentId::Git, &d).as_deref(), Some("⑂ main"));
    d.git_status = Some(GitCounts {
        staged: 1,
        unstaged: 2,
        untracked: 3,
    });
    assert_eq!(seg(SegmentId::Git, &d).as_deref(), Some("⑂ main *2 +1 ?3"));
    let no_branch = SegmentOptions {
        git: GitOptions {
            show_branch: false,
            show_staged: false,
            show_unstaged: true,
            show_untracked: false,
        },
        ..SegmentOptions::default()
    };
    assert_eq!(
        render_segment(SegmentId::Git, &d, &no_branch, &Symbols::unicode(), &Plain).as_deref(),
        Some("⎇ *2")
    );
    d.pr = Some((42, "https://github.com/o/r/pull/42".into()));
    assert_eq!(seg(SegmentId::Pr, &d).as_deref(), Some("⤴ #42"));
    d.hyperlinks = true;
    assert_eq!(
        seg(SegmentId::Pr, &d).as_deref(),
        Some("\x1b]8;;https://github.com/o/r/pull/42\x07⤴ #42\x1b]8;;\x07")
    );
    d.clock = (13, 5, 9);
    assert_eq!(seg(SegmentId::Time, &d).as_deref(), Some("⏱ 13:05"));
    let twelve = SegmentOptions {
        time: TimeOptions {
            twelve_hour: true,
            show_seconds: true,
        },
        ..SegmentOptions::default()
    };
    let t = |d: &StatusData| render_segment(SegmentId::Time, d, &twelve, &Symbols::ascii(), &Plain);
    assert_eq!(t(&d).as_deref(), Some("t: 1:05:09pm"));
    d.clock = (0, 0, 0);
    assert_eq!(t(&d).as_deref(), Some("t: 12:00:00am"));
    assert_eq!(seg(SegmentId::Subagents, &d), None);
    d.subagent_count = 3;
    assert_eq!(seg(SegmentId::Subagents, &d).as_deref(), Some("👥 3"));
    assert_eq!(seg(SegmentId::TimeSpent, &d), None);
    d.active_ms = 1_815_000;
    assert_eq!(seg(SegmentId::TimeSpent, &d).as_deref(), Some("⏱ 30m15s"));
    assert_eq!(seg(SegmentId::Session, &d).as_deref(), Some("🆔 new"));
    d.session_id = Some("0123456789abcdef".into());
    assert_eq!(seg(SegmentId::Session, &d).as_deref(), Some("🆔 01234567"));
    d.hostname = "box.local".into();
    assert_eq!(seg(SegmentId::Hostname, &d).as_deref(), Some("🖥 box"));
    d.session_name = Some("  fix\nthe   bug ".into());
    assert_eq!(seg(SegmentId::SessionName, &d).as_deref(), Some("fix the bug"));
    d.hook_statuses = vec!["lint ok".into(), "".into(), "mcp\r\n2".into()];
    assert_eq!(seg(SegmentId::Status, &d).as_deref(), Some("lint ok · mcp 2"));
    // stream state and the working brand: spinner frame from the shared 80 ms clock
    assert_eq!(seg(SegmentId::State, &d), None);
    d.stream = StreamState::Streaming;
    d.now_ms = 170;
    assert_eq!(seg(SegmentId::State, &d).as_deref(), Some("⠹ streaming"));
    assert_eq!(seg(SegmentId::Pi, &d).as_deref(), Some("π"));
    d.turn_elapsed_ms = Some(75_000);
    assert_eq!(seg(SegmentId::Pi, &d).as_deref(), Some("⠹ 1m"));
    d.turn_elapsed_ms = Some(400 * 3_600_000);
    assert_eq!(seg(SegmentId::Pi, &d).as_deref(), Some("⠹ 99h"));
}

#[test]
fn tui_status_line_presets_and_settings() {
    let p = preset(Preset::Ascii);
    assert_eq!(
        p.left.iter().map(|s| s.name()).collect::<Vec<_>>(),
        ["vim", "model", "path", "git", "pr"]
    );
    assert_eq!(
        p.right.iter().map(|s| s.name()).collect::<Vec<_>>(),
        ["session_name", "token_total", "cost", "context_pct"]
    );
    assert_eq!(p.separator, SeparatorStyle::Ascii);
    assert_eq!(preset(Preset::Minimal).options.path.max_length, 30);
    assert!(!preset(Preset::Minimal).options.git.show_staged);
    assert!(preset(Preset::Nerd).options.time.show_seconds);
    assert_eq!(Preset::parse("nerd"), Some(Preset::Nerd));
    assert_eq!(Preset::parse("bogus"), None);
    assert_eq!(SeparatorStyle::parse("bogus"), SeparatorStyle::PowerlineThin);
    assert_eq!(SegmentId::parse("context_pct"), Some(SegmentId::ContextPct));
    assert_eq!(SegmentId::parse("nope"), None);
    let s = StatusLineSettings {
        preset: Some(Preset::Minimal),
        right: Some(vec![SegmentId::Time]),
        separator: Some(SeparatorStyle::Pipe),
        ..StatusLineSettings::default()
    };
    let e = s.effective();
    assert_eq!(e.left, preset(Preset::Minimal).left);
    assert_eq!(e.right, [SegmentId::Time]);
    assert_eq!(e.separator, SeparatorStyle::Pipe);
    assert_eq!(StatusLineSettings::default().effective(), preset(Preset::Default));
    let sym = Symbols::unicode();
    assert_eq!(sym.separator(SeparatorStyle::Slash), ("/", "/"));
    assert_eq!(sym.separator(SeparatorStyle::Pipe), ("│", "│"));
    assert_eq!(sym.separator(SeparatorStyle::Powerline), ("▶", "◀"));
    assert_eq!(
        Symbols::nerd().separator(SeparatorStyle::PowerlineThin),
        ("\u{e0b1}", "\u{e0b3}")
    );
}

fn layout() -> Layout {
    Layout {
        left: vec![SegmentId::Model, SegmentId::Path],
        right: vec![SegmentId::SessionName, SegmentId::Cost],
        separator: SeparatorStyle::Pipe,
        options: SegmentOptions::default(),
    }
}

fn line_data() -> StatusData {
    let mut d = data();
    d.model_name = Some("gpt".into());
    d.session_name = Some("fix bug".into());
    d.usage.cost = 0.5;
    d
}

#[test]
fn tui_status_line_layout_fits_width_in_omp_order() {
    let sym = Symbols::ascii();
    let r = |d: &StatusData, w| render_status_line(&layout(), d, &sym, &Plain, w);
    let d = line_data();
    // groups: " [M] gpt | [D] ~/x " (19) and " fix bug | $0.50 " (17), gap fills the width
    assert_eq!(
        r(&d, 50),
        format!(" [M] gpt | [D] ~/x {} fix bug | $0.50 ", " ".repeat(14))
    );
    assert_eq!(r(&d, 0), " [M] gpt | [D] ~/x   fix bug | $0.50 ");
    // right segments drop from the end first
    assert_eq!(r(&d, 30), " [M] gpt | [D] ~/x    fix bug ");
    // then the whole right group
    assert_eq!(r(&d, 20), " [M] gpt | [D] ~/x ");
    // a long session name shrinks (to at least 8 columns) before right segments drop
    let mut long = line_data();
    long.session_name = Some("a very long session title".into());
    assert_eq!(r(&long, 40), " [M] gpt | [D] ~/x   a very lo\x1b[0m…\x1b[0m | $0.50 ");
    // at the 8-column floor the bar still overflows, so the cost drops
    assert_eq!(r(&long, 37), " [M] gpt | [D] ~/x          a very \x1b[0m…\x1b[0m ");
    assert_eq!(r(&long, 30), " [M] gpt | [D] ~/x   a very \x1b[0m…\x1b[0m ");
    // the path shrinks before left segments drop
    let mut deep = line_data();
    deep.cwd = "/home/u/aaaa/bbbb/cccc".into();
    deep.session_name = None;
    deep.usage.cost = 0.0;
    assert_eq!(r(&deep, 100).trim_end(), " [M] gpt | [D] ~/aaaa/bbbb/cccc");
    assert_eq!(r(&deep, 25), " [M] gpt | [D] …bbb/cccc ");
    // left segments drop right to left, but the path survives longest
    let wide = Layout {
        left: vec![SegmentId::Model, SegmentId::Path, SegmentId::Time],
        right: vec![],
        separator: SeparatorStyle::Pipe,
        options: SegmentOptions::default(),
    };
    let mut d = line_data();
    d.clock = (9, 3, 0);
    assert_eq!(
        render_status_line(&wide, &d, &sym, &Plain, 40),
        " [M] gpt | [D] ~/x | t: 9:03 "
    );
    assert_eq!(render_status_line(&wide, &d, &sym, &Plain, 20), " [M] gpt | [D] ~/x ");
    assert_eq!(render_status_line(&wide, &d, &sym, &Plain, 10), " [D] ~/x ");
    // only a right group: right-aligned
    let right_only = Layout {
        left: vec![],
        right: vec![SegmentId::Cost],
        separator: SeparatorStyle::Pipe,
        options: SegmentOptions::default(),
    };
    assert_eq!(
        render_status_line(&right_only, &line_data(), &sym, &Plain, 12),
        "      $0.50 "
    );
    assert_eq!(render_status_line(&right_only, &data(), &sym, &Plain, 12), "");
}

#[test]
fn tui_status_line_theme_style_colors_segments() {
    let json = tui::theme::builtin_theme_json("dark").unwrap();
    let theme = tui::theme::create_theme(&json, tui::theme::ColorMode::Truecolor, None).unwrap();
    let mut d = data();
    d.git_branch = Some("main".into());
    let out = render_segment(
        SegmentId::Git,
        &d,
        &SegmentOptions::default(),
        &Symbols::unicode(),
        &ThemeStyle(&theme),
    )
    .unwrap();
    assert_eq!(out, theme.fg("success", "⑂ main").unwrap());
}

#[test]
fn tui_status_git_parses_remotes_heads_and_porcelain() {
    assert_eq!(
        parse_github_repo("https://github.com/can1357/oh-my-pi.git").as_deref(),
        Some("can1357/oh-my-pi")
    );
    assert_eq!(
        parse_github_repo("https://github.com/can1357/oh-my-pi").as_deref(),
        Some("can1357/oh-my-pi")
    );
    assert_eq!(
        parse_github_repo("git@github.com:loftiskg/oh-my-pi.git").as_deref(),
        Some("loftiskg/oh-my-pi")
    );
    assert_eq!(
        parse_github_repo("ssh://git@github.com/user/repo.git").as_deref(),
        Some("user/repo")
    );
    assert_eq!(parse_github_repo("https://gitlab.com/user/repo.git"), None);
    assert_eq!(parse_github_repo("https://github.corp.com/org/repo.git"), None);
    assert_eq!(
        parse_github_repo("https://github.com/org/my.repo.name.git").as_deref(),
        Some("org/my.repo.name")
    );
    assert_eq!(
        parse_github_repo("https://github.com/org/my.repo").as_deref(),
        Some("org/my.repo")
    );
    assert_eq!(parse_default_branch("origin/main"), "main");
    assert_eq!(parse_default_branch("upstream/main"), "main");
    assert_eq!(parse_default_branch("main"), "main");
    assert_eq!(parse_default_branch(""), "");

    let ctx = |b: &str, r: &str| PrCacheContext {
        branch: b.into(),
        repo_id: Some(r.into()),
    };
    let one = ctx("feature/one", "/repo/.git/HEAD");
    assert!(
        can_reuse_cached_pr(Some(&None), Some(&one), Some(&one)),
        "negative cache reuse"
    );
    let pr = Some((12, "https://example.test/pr/12".to_string()));
    assert!(!can_reuse_cached_pr(
        Some(&pr),
        Some(&one),
        Some(&ctx("feature/two", "/repo/.git/HEAD"))
    ));
    assert!(!can_reuse_cached_pr(
        Some(&pr),
        Some(&one),
        Some(&ctx("feature/one", "/repo-b/.git/HEAD"))
    ));
    assert!(!can_reuse_cached_pr(Some(&pr), Some(&one), None));
    assert!(!can_reuse_cached_pr(None, Some(&one), Some(&one)));

    assert_eq!(parse_head("ref: refs/heads/feature/x\n").as_deref(), Some("feature/x"));
    assert_eq!(parse_head("0123456789abcdef0123\n").as_deref(), Some("0123456"));
    assert_eq!(parse_head("garbage"), None);
    assert_eq!(
        parse_porcelain("M  a\n M b\nMM c\n?? d\n?? e\nA  f\n!! ignored\n"),
        GitCounts {
            staged: 3,
            unstaged: 2,
            untracked: 2
        }
    );
}

struct FakePr(mpsc::Sender<String>);

impl PrLookup for FakePr {
    fn lookup(&mut self, _cwd: &Path, branch: &str) -> Option<(u64, String)> {
        let _ = self.0.send(branch.to_string());
        (branch == "feature").then(|| (7, "https://github.com/o/r/pull/7".to_string()))
    }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn tui_status_git_probe_reads_a_real_repository_and_caches_pr_lookups() {
    let dir: PathBuf = std::env::temp_dir().join(format!("tui-status-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a"), "1").unwrap();
    git(&dir, &["add", "a"]);
    std::fs::write(dir.join("b"), "1").unwrap();
    let (tx, rx) = mpsc::channel();
    let mut probe = GitProbe::new(dir.join("sub"), FakePr(tx));
    let snap = probe.probe();
    assert_eq!(snap.branch.as_deref(), Some("main"));
    assert_eq!(
        snap.status,
        Some(GitCounts {
            staged: 1,
            unstaged: 0,
            untracked: 1
        })
    );
    assert_eq!(snap.pr, None);
    probe.probe();
    assert_eq!(
        probe.lookups, 1,
        "the negative answer is cached for an unchanged branch"
    );
    git(&dir, &["checkout", "-q", "-b", "feature"]);
    let snap = probe.probe();
    assert_eq!(snap.pr, Some((7, "https://github.com/o/r/pull/7".into())));
    assert_eq!(probe.lookups, 2);
    assert_eq!(rx.try_iter().collect::<Vec<_>>(), ["main", "feature"]);

    // the watcher: refresh never blocks, snapshots arrive off-thread
    let (utx, urx) = mpsc::channel();
    let (ptx, _prx) = mpsc::channel();
    let watcher = GitWatcher::spawn(dir.clone(), FakePr(ptx), move || {
        let _ = utx.send(());
    });
    urx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(watcher.snapshot().branch.as_deref(), Some("feature"));
    assert_eq!(watcher.snapshot().pr.map(|p| p.0), Some(7));
    watcher.shutdown();

    let outside = GitProbe::new(PathBuf::from("/"), FakePr(mpsc::channel().0)).probe();
    assert_eq!(outside, GitSnapshot::default());
    std::fs::remove_dir_all(&dir).unwrap();
}
