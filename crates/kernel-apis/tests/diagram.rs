//! `diagram.mermaid` (kernel_apis::diagram and diagram_svg), new in Rust (no TypeScript reference): literal
//! expectations. Pele's CSS variables flattened to Ultron theme colours (nested definitions, missing fallbacks,
//! `color-mix`), the theme choice, the PNG drawn with those colours at 2x, and the module through `KernelApis` with a
//! fake `node` on the injected session PATH (it records its argument, stdin and environment and prints canned Pele
//! JSON). Fonts: an empty font database, so no test depends on the system's fonts. One end-to-end test runs the
//! real node with the vendored Pele, only with `ULTRON_MERMAID_TEST_NODE=1`.

use kernel::{HostHandler, Signal};
use kernel_apis::KernelApis;
use kernel_apis::diagram::{
    DiagramConfig, DiagramModule, NO_NODE, PELE_JS, RUNNER, ShowDiagram, ShownDiagram, ThemeInputs, find_node,
    install_pele, output_paths, parse_runner_output, pele_dir_name, slug,
};
use kernel_apis::diagram_svg::{
    DiagramPalette, VarScope, color_mix, custom_properties, flatten_svg, font_database, palette_from_theme,
    pick_family, rasterize, select_palette, select_palette_for_terminal,
};
use resvg::tiny_skia::Pixmap;
use resvg::usvg::fontdb;
use serde_json::{Map, Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn dark() -> DiagramPalette {
    select_palette("dark", None, None, Path::new("/nonexistent")).unwrap()
}

fn light() -> DiagramPalette {
    select_palette("light", None, None, Path::new("/nonexistent")).unwrap()
}

/// A Pele-shaped SVG: the root style defines the `--_*` properties over `--pele-*` with fallbacks, shapes use them.
const SVG: &str = concat!(
    r##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="60" viewBox="0 0 120 60" "##,
    r##"style="--_bg:var(--pele-bg,#fff);--_fg:var(--pele-fg,#1f1f1f);--_s:var(--pele-surface,#f3f3f3);"##,
    r##"--_a:var(--pele-surface-alt,#e6e6e6);--_b:var(--pele-border,#8a8a8a);color:var(--_fg)">"##,
    r##"<rect x="10" y="10" width="40" height="40" fill="var(--_s)" stroke="var(--_b)"/>"##,
    r##"<rect x="70" y="10" width="40" height="40" style="fill:color-mix(in srgb,var(--_a) 50%,var(--_bg))"/>"##,
    r##"<text x="30" y="30" fill="var(--_fg)">var(--pele-bg) label</text></svg>"##
);

#[test]
fn kernel_apis_diagram_palette_maps_the_ultron_theme_tokens() {
    let dark = dark();
    assert!(dark.dark);
    assert_eq!(
        dark.vars,
        [
            ("--pele-bg", "#18181e"),
            ("--pele-fg", "#d4d4d4"),
            ("--pele-muted", "#808080"),
            ("--pele-line", "#d4d4d4"),
            ("--pele-surface", "#282832"),
            ("--pele-surface-alt", "#3a3a4a"),
            ("--pele-border", "#5f87ff"),
            ("--pele-accent", "#8abeb7"),
            ("--pele-critical", "#cc6666"),
            ("--pele-error", "#cc6666"),
            ("--pele-series-1", "#8abeb7"),
            ("--pele-series-2", "#b5bd68"),
            ("--pele-series-3", "#ffff00"),
            ("--pele-series-4", "#cc6666"),
            ("--pele-series-5", "#5f87ff"),
            ("--pele-series-6", "#b294bb"),
            ("--pele-series-7", "#f0c674"),
            ("--pele-series-8", "#4EC9B0"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
    );
    let light = light();
    assert!(!light.dark);
    assert_eq!(light.background(), "#f8f8f8");
    assert_eq!(light.foreground(), "#1f2328");
    assert_eq!(light.get("--pele-surface"), Some("#e8e8f0"));
    assert_eq!(light.get("--pele-accent"), Some("#5a8080"));
    // A custom theme without export colours: the page falls back by its text's lightness.
    let mut custom = tui::theme::builtin_theme_json("light").unwrap();
    custom.export = Default::default();
    let palette = palette_from_theme(&custom, "paper").unwrap();
    assert!(!palette.dark);
    assert_eq!(palette.background(), "#ffffff");
}

#[test]
fn kernel_apis_diagram_theme_follows_the_setting_and_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let none = dir.path();
    // Unset: COLORFGBG's background (15 is white), else dark.
    assert_eq!(select_palette("auto", None, None, none).unwrap().theme, "dark");
    assert_eq!(select_palette("auto", None, Some("0;15"), none).unwrap().theme, "light");
    assert_eq!(
        select_palette("auto", Some("light"), None, none).unwrap().theme,
        "light"
    );
    // A "light/dark" pair picks the terminal's side.
    assert_eq!(
        select_palette("auto", Some("light/dark"), Some("0;15"), none)
            .unwrap()
            .theme,
        "light"
    );
    assert_eq!(
        select_palette("auto", Some("light/dark"), Some("15;0"), none)
            .unwrap()
            .theme,
        "dark"
    );
    // An explicit theme wins over the setting.
    assert_eq!(
        select_palette("dark", Some("light"), Some("0;15"), none).unwrap().theme,
        "dark"
    );
    // A custom theme from the themes directory; one that does not load falls back to the terminal's built-in.
    let mut json: Value = serde_json::from_str(include_str!("../../tui/src/theme/dark.json")).unwrap();
    json["name"] = json!("ember");
    json["vars"]["accent"] = json!("#ff8800");
    std::fs::write(none.join("ember.json"), json.to_string()).unwrap();
    let ember = select_palette("auto", Some("ember"), None, none).unwrap();
    assert_eq!(
        (ember.theme.as_str(), ember.get("--pele-accent")),
        ("ember", Some("#ff8800"))
    );
    assert_eq!(
        select_palette("auto", Some("missing"), Some("0;15"), none)
            .unwrap()
            .theme,
        "light"
    );
    assert_eq!(
        select_palette("sepia", None, None, none).unwrap_err(),
        "diagram.mermaid theme must be \"auto\", \"dark\" or \"light\", not \"sepia\""
    );
}

/// `auto` with the terminal's appearance known (what the attached TUI detected through OSC 11): it wins over
/// `COLORFGBG`, which wins over dark; an explicit theme or a named setting still wins over both.
#[test]
fn kernel_apis_diagram_auto_theme_follows_the_detected_terminal_before_colorfgbg() {
    let dir = tempfile::tempdir().unwrap();
    let none = dir.path();
    let theme = |requested: &str, setting: Option<&str>, terminal: Option<&str>, colorfgbg: Option<&str>| {
        select_palette_for_terminal(requested, setting, terminal, colorfgbg, none)
            .unwrap()
            .theme
    };
    assert_eq!(theme("auto", None, Some("light"), Some("15;0")), "light");
    assert_eq!(theme("auto", None, Some("dark"), Some("0;15")), "dark");
    assert_eq!(theme("auto", Some("light/dark"), Some("light"), Some("15;0")), "light");
    assert_eq!(theme("auto", Some("light/dark"), Some("dark"), Some("0;15")), "dark");
    // Nothing detected (or something unknown): COLORFGBG, else dark.
    assert_eq!(theme("auto", Some("light/dark"), None, Some("0;15")), "light");
    assert_eq!(theme("auto", Some("light/dark"), Some("sepia"), Some("0;15")), "light");
    assert_eq!(theme("auto", Some("light/dark"), None, None), "dark");
    // Explicit choices win.
    assert_eq!(theme("auto", Some("dark"), Some("light"), Some("0;15")), "dark");
    assert_eq!(theme("dark", None, Some("light"), None), "dark");
    assert_eq!(theme("light", Some("dark"), Some("dark"), None), "light");
}

#[test]
fn kernel_apis_diagram_var_resolution_handles_nesting_definitions_and_missing_fallbacks() {
    let palette = dark();
    let scope = VarScope {
        palette: &palette,
        defs: custom_properties(SVG),
    };
    assert_eq!(scope.defs["--_s"], "var(--pele-surface,#f3f3f3)");
    assert_eq!(scope.defs.len(), 5, "{:?}", scope.defs);
    // A theme colour beats the fallback; a definition is followed to it.
    assert_eq!(scope.resolve("var(--pele-surface,#f3f3f3)"), "#282832");
    assert_eq!(
        scope.resolve("fill:var(--_s);stroke:var(--_b)"),
        "fill:#282832;stroke:#5f87ff"
    );
    // An unknown name takes its fallback, itself resolved (nested var); a fallback may hold commas.
    assert_eq!(scope.resolve("var(--x,var(--y,#123456))"), "#123456");
    assert_eq!(scope.resolve("var(--x,var(--_a))"), "#3a3a4a");
    assert_eq!(scope.resolve("var(--font, Inter, sans-serif)"), "Inter, sans-serif");
    // Nothing to resolve to: the theme's foreground.
    assert_eq!(scope.resolve("var(--nothing)"), "#d4d4d4");
    // A definition naming itself is not followed forever.
    let cyclic = VarScope {
        palette: &palette,
        defs: [("--_x".to_string(), "var(--_x,#010203)".to_string())].into(),
    };
    assert_eq!(cyclic.resolve("var(--_x)"), "#010203");
    // Unbalanced text stays.
    assert_eq!(scope.resolve("var(--_s"), "var(--_s");
}

#[test]
fn kernel_apis_diagram_color_mix_follows_css_weights() {
    assert_eq!(color_mix("in srgb,#000000 50%,#ffffff").as_deref(), Some("#808080"));
    assert_eq!(color_mix("in srgb,#ff0000 25%,#0000ff").as_deref(), Some("#4000bf"));
    assert_eq!(color_mix("in srgb,#ff0000,#0000ff 25%").as_deref(), Some("#bf0040"));
    assert_eq!(color_mix("in srgb,#fff,#000").as_deref(), Some("#808080"));
    // Weights over 100% are normalized.
    assert_eq!(
        color_mix("in srgb,#ff0000 100%,#0000ff 100%").as_deref(),
        Some("#800080")
    );
    assert_eq!(color_mix("in oklab,#fff,#000"), None);
    assert_eq!(color_mix("in srgb,red,#000"), None);
}

#[test]
fn kernel_apis_diagram_flatten_leaves_no_variables_and_keeps_labels() {
    let flat = flatten_svg(SVG, &dark());
    assert!(!flat.contains("var(--_") && !flat.contains("--_"), "{flat}");
    assert!(!flat.contains("color-mix"), "{flat}");
    assert_eq!(
        flat,
        concat!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="60" viewBox="0 0 120 60" "##,
            r##"style="color:#d4d4d4">"##,
            r##"<rect x="10" y="10" width="40" height="40" fill="#282832" stroke="#5f87ff"/>"##,
            r##"<rect x="70" y="10" width="40" height="40" style="fill:#292934"/>"##,
            r##"<text x="30" y="30" fill="#d4d4d4">var(--pele-bg) label</text></svg>"##
        )
    );
    // The light theme gives other colours; a style of only definitions disappears.
    let light = flatten_svg(
        r#"<g style="--_s:var(--pele-surface,#f3f3f3)"><rect fill="var(--_s)"/></g>"#,
        &light(),
    );
    assert_eq!(light, r##"<g><rect fill="#e8e8f0"/></g>"##);
    // `>` inside a quoted attribute does not end the tag.
    let quoted = flatten_svg(r#"<a title="x > y" fill="var(--pele-accent,#000)">t</a>"#, &dark());
    assert_eq!(quoted, r##"<a title="x > y" fill="#8abeb7">t</a>"##);
}

fn pixel(png: &[u8], x: u32, y: u32) -> [u8; 3] {
    let map = Pixmap::decode_png(png).unwrap();
    let p = map.pixel(x, y).unwrap();
    [p.red(), p.green(), p.blue()]
}

fn no_fonts() -> Arc<fontdb::Database> {
    Arc::new(fontdb::Database::new())
}

#[test]
fn kernel_apis_diagram_png_is_drawn_at_twice_the_size_in_theme_colours() {
    let palette = dark();
    let png = rasterize(&flatten_svg(SVG, &palette), 2.0, palette.background(), no_fonts()).unwrap();
    assert_eq!((png.width, png.height), (240, 120));
    assert!(png.data.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(pixel(&png.data, 2, 2), [0x18, 0x18, 0x1e], "the page");
    assert_eq!(pixel(&png.data, 60, 60), [0x28, 0x28, 0x32], "the node's surface");
    assert_eq!(pixel(&png.data, 180, 60), [0x29, 0x29, 0x34], "the color-mix");
    // Unflattened, the same shapes are black: the variables must be resolved first.
    let raw = rasterize(SVG, 2.0, palette.background(), no_fonts()).unwrap();
    assert_eq!(pixel(&raw.data, 60, 60), [0, 0, 0]);
    // Huge diagrams are scaled to the longest side's limit.
    let wide = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10000" height="100"></svg>"#;
    let big = rasterize(wide, 2.0, "#000000", no_fonts()).unwrap();
    assert_eq!((big.width, big.height), (8192, 82));
    assert!(
        rasterize("<svg", 2.0, "#000", no_fonts())
            .unwrap_err()
            .starts_with("diagram.mermaid: the SVG does not parse:")
    );
}

#[test]
fn kernel_apis_diagram_font_fallback_prefers_common_families() {
    let have = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    assert_eq!(
        pick_family(
            &have(&["Zilla", "noto sans", "DejaVu Sans"]),
            &["DejaVu Sans", "Noto Sans"]
        )
        .as_deref(),
        Some("DejaVu Sans")
    );
    assert_eq!(
        pick_family(&have(&["Zilla", "noto sans"]), &["DejaVu Sans", "Noto Sans"]).as_deref(),
        Some("noto sans")
    );
    assert_eq!(
        pick_family(&have(&["Zilla"]), &["DejaVu Sans"]).as_deref(),
        Some("Zilla")
    );
    assert_eq!(pick_family(&[], &["DejaVu Sans"]), None);
    // An empty database keeps fontdb's defaults (nothing to point them at).
    let db = font_database(|_| {});
    assert_eq!(db.len(), 0);
}

#[test]
fn kernel_apis_diagram_slugs_paths_and_runner_answers() {
    assert_eq!(slug("flowchart LR\n  a --> b", "flowchart"), "flowchart");
    assert_eq!(
        slug("---\ntitle: Build & Deploy!\n---\nflowchart LR\n a-->b", "flowchart"),
        "build-deploy"
    );
    assert_eq!(slug("pie\n  title Pets adopted\n \"Dogs\" : 3", "pie"), "pets-adopted");
    assert_eq!(slug("x", "***"), "diagram");
    assert_eq!(slug(&format!("title {}", "a".repeat(80)), "t").len(), 40);
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (svg, png) = output_paths(None, d, "flow");
    assert_eq!((svg, png), (d.join("flow-1.svg"), d.join("flow-1.png")));
    std::fs::write(d.join("flow-1.png"), "taken").unwrap();
    assert_eq!(output_paths(Some(d), d, "flow").0, d.join("flow-2.svg"));
    assert_eq!(
        output_paths(Some(&d.join("out/arch.png")), d, "flow"),
        (d.join("out/arch.svg"), d.join("out/arch.png"))
    );
    assert_eq!(output_paths(Some(&d.join("arch")), d, "flow").1, d.join("arch.png"));
    assert_eq!(output_paths(Some(&d.join("v1.2")), d, "flow").1, d.join("v1.2.png"));
    // The runner's answers.
    let ok = parse_runner_output(&format!(
        "{}\n",
        json!({"ok": true, "type": "flowchart", "svg": "<svg/>", "width": 1, "height": 1})
    ))
    .unwrap();
    assert_eq!((ok.kind.as_str(), ok.svg.as_str()), ("flowchart", "<svg/>"));
    let error = json!({"ok": false, "error": "Unexpected token '-->'", "line": 2, "column": 5}).to_string();
    assert_eq!(
        parse_runner_output(&error).unwrap_err(),
        "Unexpected token '-->' (line 2, column 5)"
    );
    let named = json!({"ok": false, "error": "Parse error on line 3:\n^", "line": 3, "column": 1}).to_string();
    assert_eq!(parse_runner_output(&named).unwrap_err(), "Parse error on line 3:\n^");
    let no_line = json!({"ok": false, "error": "Unknown diagram type", "line": 0}).to_string();
    assert_eq!(parse_runner_output(&no_line).unwrap_err(), "Unknown diagram type");
    assert_eq!(
        parse_runner_output("garbage").unwrap_err(),
        "diagram.mermaid: the renderer answered no JSON"
    );
    assert_eq!(
        parse_runner_output(&json!({"ok": true, "svg": ""}).to_string()).unwrap_err(),
        "diagram.mermaid: the renderer answered no SVG"
    );
}

#[test]
fn kernel_apis_diagram_pele_is_unpacked_once_under_a_content_addressed_name() {
    let note = include_str!("../assets/pele/VERSION");
    let sha = kernel_apis::util::sha256_hex(PELE_JS);
    assert!(
        note.contains(&format!("sha256 {sha}  pele.js")),
        "the VERSION note names the embedded bytes"
    );
    assert!(note.starts_with("pele 1.7.0 (MIT"));
    let dir = tempfile::tempdir().unwrap();
    let runner = install_pele(dir.path()).unwrap();
    let mut both = PELE_JS.to_vec();
    both.extend_from_slice(RUNNER);
    let name = format!("pele-{}", &kernel_apis::util::sha256_hex(&both)[..16]);
    assert_eq!(pele_dir_name(), name);
    assert_eq!(runner, dir.path().join(&name).join("runner.mjs"));
    assert_eq!(std::fs::read(dir.path().join(&name).join("pele.mjs")).unwrap(), PELE_JS);
    assert!(
        std::fs::read_to_string(dir.path().join(&name).join("LICENSE"))
            .unwrap()
            .starts_with("MIT License")
    );
    assert!(String::from_utf8_lossy(RUNNER).contains(r#"import { render } from "./pele.mjs";"#));
    // A second call reuses it; a damaged one is rewritten.
    std::fs::write(dir.path().join(&name).join("marker"), "kept").unwrap();
    install_pele(dir.path()).unwrap();
    assert!(
        dir.path().join(&name).join("marker").exists(),
        "a complete directory is reused"
    );
    std::fs::write(dir.path().join(&name).join("pele.mjs"), "truncated").unwrap();
    install_pele(dir.path()).unwrap();
    assert_eq!(std::fs::read(dir.path().join(&name).join("pele.mjs")).unwrap(), PELE_JS);
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(
        entries,
        [std::ffi::OsString::from(&name)],
        "no staging directory is left"
    );
}

#[test]
fn kernel_apis_diagram_node_is_found_on_the_given_path_only() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("node"), "not executable").unwrap();
    script(&b.join("node"), "#!/bin/sh\n");
    let path = format!("relative:{}:{}", a.display(), b.display());
    assert_eq!(find_node(Some(&path)), Some(b.join("node")));
    assert_eq!(find_node(Some(&a.display().to_string())), None);
    assert_eq!(find_node(Some("")), None);
    assert_eq!(find_node(None), None);
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// (lane, to_model, line, png) of one shown diagram.
type Shown = (String, bool, String, Vec<u8>);

/// A temp root with `bin/node` (a fake) and what the module was asked to show.
struct Setup {
    _dir: tempfile::TempDir,
    root: PathBuf,
    shown: Arc<Mutex<Vec<Shown>>>,
}

/// A fake `node`: records its first argument, stdin and environment under `$HOME`, then runs `body`.
fn setup(body: &str) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for sub in ["bin", "home", "cwd", "agent"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    script(
        &root.join("bin/node"),
        &format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > \"$HOME/arg\"\n/usr/bin/cat > \"$HOME/stdin\"\n/usr/bin/env > \"$HOME/env\"\n{body}\n"
        ),
    );
    Setup {
        _dir: dir,
        root,
        shown: Arc::default(),
    }
}

impl Setup {
    fn config(&self, path_var: Option<String>) -> DiagramConfig {
        let shown = self.shown.clone();
        let show: ShowDiagram = Arc::new(move |d: &ShownDiagram<'_>| {
            assert_eq!(
                std::fs::read(d.png_path).unwrap(),
                d.png,
                "the shown PNG is the saved one"
            );
            shown
                .lock()
                .unwrap()
                .push((d.lane.to_string(), d.to_model, d.line.clone(), d.png.to_vec()));
            Ok(())
        });
        DiagramConfig {
            path_var,
            home: self.root.join("home"),
            cache_dir: self.root.join("agent/cache"),
            out_dir: self.root.join("agent/rlm-output/s1/diagrams"),
            cwd: self.root.join("cwd"),
            theme: Arc::new(|| ThemeInputs {
                setting: Some("dark".into()),
                colorfgbg: None,
                terminal: None,
                custom_dir: PathBuf::from("/nonexistent"),
            }),
            show,
            fonts: Some(no_fonts()),
            timeout: Duration::from_secs(120),
        }
    }

    fn session_path(&self) -> String {
        // Only the fake's directory: the real node (often /usr/bin/node) must never be found.
        self.root.join("bin").display().to_string()
    }

    fn call(&self, config: DiagramConfig, payload: Value) -> Result<Value, String> {
        let apis = KernelApis::new(Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| {
            Err(format!("unhandled {kind}"))
        }))
        .with(Arc::new(DiagramModule::new(config)));
        apis.handle("diagram.mermaid", payload.as_object().unwrap().clone(), &Signal::new())
    }

    fn answer(&self, answer: &Value) {
        std::fs::write(self.root.join("home/answer.json"), answer.to_string()).unwrap();
    }
}

#[test]
fn kernel_apis_diagram_mermaid_runs_node_from_the_session_path_and_saves_themed_files() {
    let s = setup("/usr/bin/cat \"$HOME/answer.json\"");
    s.answer(&json!({"ok": true, "type": "flowchart", "mermaidType": "flowchart-v2", "svg": SVG, "width": 120, "height": 60}));
    let text = "flowchart LR\n  a --> b\n";
    let result = s.call(s.config(Some(s.session_path())), json!({"text": text})).unwrap();
    let dir = s.root.join("agent/rlm-output/s1/diagrams");
    assert_eq!(
        result,
        json!({
            "svg_path": dir.join("flowchart-1.svg").display().to_string(),
            "png_path": dir.join("flowchart-1.png").display().to_string(),
            "width": 240, "height": 120, "type": "flowchart",
        })
    );
    // Node got the unpacked runner, the text on stdin, and only the session's PATH and HOME.
    let home = s.root.join("home");
    assert_eq!(
        std::fs::read_to_string(home.join("arg")).unwrap(),
        s.root
            .join("agent/cache")
            .join(pele_dir_name())
            .join("runner.mjs")
            .display()
            .to_string()
    );
    assert_eq!(std::fs::read_to_string(home.join("stdin")).unwrap(), text);
    let env = std::fs::read_to_string(home.join("env")).unwrap();
    assert!(env.contains(&format!("PATH={}\n", s.session_path())), "{env}");
    assert!(env.contains(&format!("HOME={}\n", home.display())), "{env}");
    assert!(
        !env.contains("CARGO") && !env.contains("RUST_TEST"),
        "the process env stays out: {env}"
    );
    // The saved SVG is the flattened one; the PNG is drawn in the dark theme (the setting).
    let svg = std::fs::read_to_string(dir.join("flowchart-1.svg")).unwrap();
    assert_eq!(svg, flatten_svg(SVG, &dark()));
    let png = std::fs::read(dir.join("flowchart-1.png")).unwrap();
    assert_eq!(pixel(&png, 60, 60), [0x28, 0x28, 0x32]);
    // Shown by default, for the user only, with the model's line.
    let shown = s.shown.lock().unwrap().clone();
    assert_eq!(shown.len(), 1);
    assert_eq!(
        (shown[0].0.as_str(), shown[0].1, shown[0].2.clone()),
        (
            "main",
            false,
            format!("diagram: {} (240x120)", dir.join("flowchart-1.png").display())
        )
    );
    assert_eq!(shown[0].3, png);
    // theme= overrides the setting; path= names the files; to_model shows it for the model; show=False shows nothing.
    s.call(
        s.config(Some(s.session_path())),
        json!({"text": text, "theme": "light", "path": "docs/arch.svg", "to_model": true}),
    )
    .unwrap();
    let png = std::fs::read(s.root.join("cwd/docs/arch.png")).unwrap();
    assert_eq!(pixel(&png, 60, 60), [0xe8, 0xe8, 0xf0]);
    assert!(s.root.join("cwd/docs/arch.svg").exists());
    assert!(s.shown.lock().unwrap()[1].1, "to_model");
    let quiet = s
        .call(s.config(Some(s.session_path())), json!({"text": text, "show": false}))
        .unwrap();
    assert_eq!(quiet["png_path"], dir.join("flowchart-2.png").display().to_string());
    assert_eq!(s.shown.lock().unwrap().len(), 2, "show=False shows nothing");
}

#[test]
fn kernel_apis_diagram_mermaid_errors_reach_the_cell() {
    let s = setup("/usr/bin/cat \"$HOME/answer.json\"");
    s.answer(&json!({"ok": false, "error": "Unexpected token '-->'", "line": 2, "column": 3}));
    let config = || s.config(Some(s.session_path()));
    assert_eq!(
        s.call(config(), json!({"text": "flowchart LR\n -->"})).unwrap_err(),
        "Unexpected token '-->' (line 2, column 3)"
    );
    // No node on the session PATH (the process PATH is never used).
    let empty = s.root.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert_eq!(
        s.call(s.config(Some(empty.display().to_string())), json!({"text": "graph TD"}))
            .unwrap_err(),
        NO_NODE
    );
    assert_eq!(
        s.call(s.config(None), json!({"text": "graph TD"})).unwrap_err(),
        NO_NODE
    );
    for (payload, error) in [
        (json!({"text": " "}), "diagram.mermaid text must be a non-empty string"),
        (
            json!({"text": "graph TD", "theme": 3}),
            "diagram.mermaid theme must be \"auto\", \"dark\" or \"light\"",
        ),
        (
            json!({"text": "graph TD", "theme": "neon"}),
            "diagram.mermaid theme must be \"auto\", \"dark\" or \"light\", not \"neon\"",
        ),
        (
            json!({"text": "graph TD", "show": "yes"}),
            "diagram.mermaid show must be True or False",
        ),
        (
            json!({"text": "graph TD", "to_model": 1}),
            "diagram.mermaid to_model must be True or False",
        ),
        (
            json!({"text": "graph TD", "path": ""}),
            "diagram.mermaid path must be a non-empty string",
        ),
        (json!({"text": "graph TD", "scale": 3}), "Unknown payload field: scale"),
    ] {
        assert_eq!(s.call(config(), payload).unwrap_err(), error);
    }
    // A node that dies without an answer: its exit status and the end of its stderr.
    let crash = setup("echo 'SyntaxError: bad runner' >&2\nexit 3");
    assert_eq!(
        crash
            .call(crash.config(Some(crash.session_path())), json!({"text": "graph TD"}))
            .unwrap_err(),
        "diagram.mermaid: node failed (exit status: 3): SyntaxError: bad runner"
    );
    assert!(s.shown.lock().unwrap().is_empty() && crash.shown.lock().unwrap().is_empty());
    assert!(
        !s.root.join("agent/rlm-output").exists(),
        "nothing is written on errors"
    );
}

#[test]
fn kernel_apis_diagram_mermaid_stops_a_node_past_its_timeout() {
    // The fake writes its pid, then waits on a fifo nobody opens: only the timeout ends it.
    let s = setup("echo $$ > \"$HOME/pid\"\n/usr/bin/mkfifo \"$HOME/never\"\nexec /usr/bin/cat \"$HOME/never\"");
    let mut config = s.config(Some(s.session_path()));
    config.timeout = Duration::from_secs(1);
    assert_eq!(
        s.call(config, json!({"text": "graph TD"})).unwrap_err(),
        "diagram.mermaid: node did not finish within 1 s"
    );
    // The child it started is gone (killed and reaped).
    let pid = std::fs::read_to_string(s.root.join("home/pid")).unwrap();
    assert!(
        !Path::new(&format!("/proc/{}", pid.trim())).exists(),
        "pid {pid} still runs"
    );
}

/// The real thing: node from this process's PATH with the vendored Pele (opt-in).
#[test]
fn kernel_apis_diagram_mermaid_renders_with_real_node_and_pele() {
    if std::env::var("ULTRON_MERMAID_TEST_NODE").as_deref() != Ok("1") {
        eprintln!("skipped: set ULTRON_MERMAID_TEST_NODE=1 to run node with the vendored Pele");
        return;
    }
    let s = setup("");
    let path = std::env::var("PATH").ok();
    assert!(
        find_node(path.as_deref()).is_some(),
        "ULTRON_MERMAID_TEST_NODE=1 needs node on PATH"
    );
    let mut config = s.config(path.clone());
    config.fonts = None;
    let result = s
        .call(
            config,
            json!({"text": "flowchart LR\n  A[Start] --> B{Ready?}\n  B -->|yes| C[Ship]\n  B -->|no| A"}),
        )
        .unwrap();
    assert_eq!(result["type"], "flowchart");
    let svg = std::fs::read_to_string(result["svg_path"].as_str().unwrap()).unwrap();
    assert!(
        svg.starts_with("<svg") && !svg.contains("var(") && svg.contains("#282832"),
        "{svg}"
    );
    let (width, height) = (result["width"].as_u64().unwrap(), result["height"].as_u64().unwrap());
    let png = std::fs::read(result["png_path"].as_str().unwrap()).unwrap();
    let map = Pixmap::decode_png(&png).unwrap();
    assert_eq!((u64::from(map.width()), u64::from(map.height())), (width, height));
    if let Ok(out) = std::env::var("ULTRON_MERMAID_TEST_OUT") {
        std::fs::write(out, &png).unwrap();
    }
    // Some pixel is the dark theme's node surface: the shapes were coloured, not black.
    let surface = map
        .pixels()
        .iter()
        .filter(|p| (p.red(), p.green(), p.blue()) == (0x28, 0x28, 0x32))
        .count();
    assert!(surface > 100, "{surface} surface pixels");
    // Other diagram types flatten fully too (series colours, color-mix clusters, sequence lifelines).
    for (text, kind) in [
        (
            "pie title Pets\n  \"Dogs\" : 386\n  \"Cats\" : 85\n  \"Rats\" : 15",
            "pie",
        ),
        ("sequenceDiagram\n  Alice->>Bob: Hello\n  Bob-->>Alice: Hi", "sequence"),
        (
            "flowchart TB\n  subgraph one\n    subgraph two\n      a1-->a2\n    end\n  end",
            "flowchart",
        ),
        ("classDiagram\n  Animal <|-- Duck\n  Animal : +int age", "class"),
        ("stateDiagram-v2\n  [*] --> Still\n  Still --> Moving", "state"),
    ] {
        let mut config = s.config(std::env::var("PATH").ok());
        config.fonts = None;
        let result = s.call(config, json!({"text": text})).unwrap();
        assert_eq!(result["type"], kind, "{text}");
        let svg = std::fs::read_to_string(result["svg_path"].as_str().unwrap()).unwrap();
        assert!(!svg.contains("var(") && !svg.contains("color-mix"), "{kind}: {svg}");
        // Every label reads against what is under it (kernel_apis::diagram_contrast).
        let palette = select_palette("dark", None, None, Path::new("/nonexistent")).unwrap();
        for label in kernel_apis::diagram_contrast::label_backdrops(&svg, &palette) {
            let ratio = kernel_apis::diagram_contrast::contrast(label.drawn, label.backdrop);
            assert!(ratio >= 4.5, "{kind}: {label:?} {ratio:.2}");
        }
        let png = std::fs::read(result["png_path"].as_str().unwrap()).unwrap();
        Pixmap::decode_png(&png).unwrap();
        if let Ok(out) = std::env::var("ULTRON_MERMAID_TEST_OUT") {
            std::fs::write(format!("{out}.{kind}.png"), &png).unwrap();
        }
    }
    // A syntax error is Pele's message.
    let mut config = s.config(path);
    config.fonts = None;
    let error = s.call(config, json!({"text": "flowchart LR\n  A -->"})).unwrap_err();
    assert!(
        error.starts_with("Parse error on line 3:") && error.contains("Expecting"),
        "{error}"
    );
    eprintln!("pele error: {error}");
}
