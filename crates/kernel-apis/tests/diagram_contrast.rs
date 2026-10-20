//! Diagram labels stay readable on the Ultron theme (kernel_apis::diagram_contrast), through `diagram.mermaid`
//! itself: a fake `node` on the injected session PATH answers the SVG the vendored Pele rendered for one sample of
//! each diagram kind (fixtures/diagrams.json, recorded by fixtures/generate-diagrams.mjs), the module flattens and
//! fixes it, and the saved SVG is checked. The default suite never runs node; the opt-in real-node test in
//! tests/diagram.rs (`ULTRON_MERMAID_TEST_NODE=1`) runs the same check on Pele's live output.

use kernel::{HostHandler, Signal};
use kernel_apis::KernelApis;
use kernel_apis::diagram::{DiagramConfig, DiagramModule, ShowDiagram, ThemeInputs};
use kernel_apis::diagram_contrast::{
    LabelColor, contrast, ensure_contrast_report, label_backdrops, parse_rgb, readable_color,
};
use kernel_apis::diagram_svg::{DiagramPalette, flatten_svg, select_palette};
use resvg::usvg::fontdb;
use serde_json::{Map, Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn samples() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/diagrams.json")).unwrap();
    fixture["samples"].as_array().unwrap().clone()
}

fn palette(theme: &str) -> DiagramPalette {
    select_palette(theme, None, None, Path::new("/nonexistent")).unwrap()
}

/// A temp root with a fake `node` that prints `$HOME/answer.json`.
struct Setup {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for sub in ["bin", "home", "cwd", "agent"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    let node = root.join("bin/node");
    std::fs::write(
        &node,
        "#!/bin/sh\n/usr/bin/cat > /dev/null\n/usr/bin/cat \"$HOME/answer.json\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    Setup { _dir: dir, root }
}

impl Setup {
    /// `diagram.mermaid` with the `theme` setting, node answering `sample`'s SVG: the saved SVG.
    fn render(&self, sample: &Value, setting: &str) -> String {
        self.render_with(sample, setting, Path::new("/nonexistent"))
    }

    fn render_with(&self, sample: &Value, setting: &str, custom_dir: &Path) -> String {
        let custom_dir = custom_dir.to_path_buf();
        let answer = json!({"ok": true, "type": sample["type"], "svg": sample["svg"]});
        std::fs::write(self.root.join("home/answer.json"), answer.to_string()).unwrap();
        let setting = setting.to_string();
        let show: ShowDiagram = Arc::new(|_| Ok(()));
        let config = DiagramConfig {
            path_var: Some(self.root.join("bin").display().to_string()),
            home: self.root.join("home"),
            cache_dir: self.root.join("agent/cache"),
            out_dir: self.root.join("agent/out"),
            cwd: self.root.join("cwd"),
            theme: Arc::new(move || ThemeInputs {
                setting: Some(setting.clone()),
                colorfgbg: None,
                terminal: None,
                custom_dir: custom_dir.clone(),
            }),
            show,
            fonts: Some(Arc::new(fontdb::Database::new())),
            timeout: Duration::from_secs(120),
        };
        let apis = KernelApis::new(Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| {
            Err(format!("unhandled {kind}"))
        }))
        .with(Arc::new(DiagramModule::new(config)));
        let payload = json!({"text": sample["text"], "show": false});
        let result = apis
            .handle("diagram.mermaid", payload.as_object().unwrap().clone(), &Signal::new())
            .unwrap();
        std::fs::read_to_string(result["svg_path"].as_str().unwrap()).unwrap()
    }
}

fn failing(labels: &[LabelColor]) -> Vec<String> {
    labels
        .iter()
        .filter(|l| contrast(l.drawn, l.backdrop) < 4.5)
        .map(|l| {
            format!(
                "{} drawn {:?} on {:?} ({:.2})",
                l.tag,
                l.drawn,
                l.backdrop,
                contrast(l.drawn, l.backdrop)
            )
        })
        .collect()
}

/// Every label of every diagram kind, in both built-in themes, reaches 4.5:1 against what is under it in the SVG
/// `diagram.mermaid` saves; Pele's own colours did not (the dark pie's percentages among them).
#[test]
fn kernel_apis_diagram_contrast_every_label_of_every_kind_is_readable_in_both_themes() {
    let s = setup();
    let mut before = Vec::new();
    for theme in ["dark", "light"] {
        let palette = palette(theme);
        for sample in samples() {
            let kind = sample["type"].as_str().unwrap().to_string();
            let saved = s.render(&sample, theme);
            let labels = label_backdrops(&saved, &palette);
            assert!(!labels.is_empty(), "{theme} {kind}: no labels found");
            assert_eq!(failing(&labels), Vec::<String>::new(), "{theme} {kind}");
            let unfixed = label_backdrops(&flatten_svg(sample["svg"].as_str().unwrap(), &palette), &palette);
            for label in failing(&unfixed) {
                before.push(format!("{theme} {kind}: {label}"));
            }
        }
    }
    // Without the fix these failed: the light pie's page-coloured percentages on its mid slices (4.1:1), muted labels
    // on node fills in both themes (3.6:1 to 4.3:1), the dark muted axis labels on the page (4.47:1).
    for expected in [
        "light pie: <text",
        "dark kanban:",
        "light sankey:",
        "dark requirement:",
        "dark gantt:",
    ] {
        assert!(
            before.iter().any(|f| f.starts_with(expected)),
            "{expected}: {before:#?}"
        );
    }
    eprintln!("{} labels fixed:\n{}", before.len(), before.join("\n"));
}

/// A dark theme with dark series colours (the reported case): Pele draws the percentages in the page colour, dark
/// on dark slices; they get the theme's text colour.
#[test]
fn kernel_apis_diagram_contrast_dark_percentages_on_dark_slices_get_the_text_colour() {
    let s = setup();
    let custom = s.root.join("themes");
    std::fs::create_dir_all(&custom).unwrap();
    let mut json: Value = serde_json::from_str(include_str!("../../tui/src/theme/dark.json")).unwrap();
    json["name"] = json!("midnight");
    for (token, color) in [
        ("accent", "#1f3a68"),
        ("success", "#1d4d2b"),
        ("warning", "#5a4a12"),
        ("error", "#5c1f1f"),
        ("border", "#2b2b55"),
        ("thinkingHigh", "#3d2350"),
        ("mdHeading", "#4a3a10"),
        ("syntaxType", "#174848"),
    ] {
        json["colors"][token] = json!(color);
    }
    std::fs::write(custom.join("midnight.json"), json.to_string()).unwrap();
    let palette = select_palette("auto", Some("midnight"), None, &custom).unwrap();
    assert_eq!(palette.get("--pele-series-1"), Some("#1f3a68"));
    let pie = samples().into_iter().find(|s| s["type"] == "pie").unwrap();
    let unfixed = label_backdrops(&flatten_svg(pie["svg"].as_str().unwrap(), &palette), &palette);
    let dark_on_dark = unfixed
        .iter()
        .filter(|l| l.drawn == (0x18, 0x18, 0x1e) && contrast(l.drawn, l.backdrop) < 2.0)
        .count();
    assert!(dark_on_dark >= 3, "{unfixed:#?}");
    let saved = s.render_with(&pie, "midnight", &custom);
    let labels = label_backdrops(&saved, &palette);
    assert_eq!(failing(&labels), Vec::<String>::new());
    let inside = saved.split("<g class=\"pele-slice-labels\"").nth(1).unwrap();
    let inside = &inside[..inside.find("</g>").unwrap()];
    assert!(inside.matches("<text fill=\"#d4d4d4\"").count() >= 3, "{inside}");
}

/// The pie's percentages, checked by geometry rather than by the module's own sampling: each label inside a slice
/// (the angle of its position from the centre) against that slice's fill.
#[test]
fn kernel_apis_diagram_contrast_pie_percentages_contrast_with_their_own_slice() {
    let s = setup();
    let pie = samples().into_iter().find(|s| s["type"] == "pie").unwrap();
    for theme in ["dark", "light"] {
        let saved = s.render(&pie, theme);
        // The slices: (start angle, end angle, fill) from `M0,0Lx1,y1A... x2,y2Z`, clockwise from 12 o'clock.
        let angle = |x: f64, y: f64| (x.atan2(-y).to_degrees() + 360.0) % 360.0;
        let mut slices = Vec::new();
        for path in saved.split("<path class=\"pele-slice\"").skip(1) {
            let d = attr(path, "d");
            let numbers: Vec<f64> = d
                .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
                .filter_map(|n| n.parse().ok())
                .collect();
            let (x1, y1) = (numbers[2], numbers[3]);
            let (x2, y2) = (numbers[numbers.len() - 2], numbers[numbers.len() - 1]);
            let (a1, mut a2) = (angle(x1, y1), angle(x2, y2));
            if a2 <= a1 {
                a2 += 360.0;
            }
            slices.push((a1, a2, parse_rgb(&attr(path, "fill")).unwrap()));
        }
        assert_eq!(slices.len(), 8, "{theme}");
        let inside = saved.split("<g class=\"pele-slice-labels\"").nth(1).unwrap();
        let inside = &inside[..inside.find("</g>").unwrap()];
        let mut checked = 0;
        for text in inside.split("<text").skip(1) {
            let fill = parse_rgb(&attr(text, "fill")).unwrap_or_else(|| {
                // No own fill: the group's.
                parse_rgb(&attr(inside, "fill")).unwrap()
            });
            let (x, y) = (attr(text, "x").parse().unwrap(), attr(text, "y").parse().unwrap());
            let a = angle(x, y);
            let (_, _, slice) = slices
                .iter()
                .find(|(a1, a2, _)| (a >= *a1 && a < *a2) || (a + 360.0 >= *a1 && a + 360.0 < *a2))
                .unwrap();
            let ratio = contrast(fill, *slice);
            assert!(
                ratio >= 4.5,
                "{theme}: label at {x},{y} {fill:?} on {slice:?}: {ratio:.2}"
            );
            checked += 1;
        }
        assert!(checked >= 3, "{theme}: {checked} labels inside slices");
    }
}

fn attr(tag: &str, name: &str) -> String {
    let needle = format!(" {name}=\"");
    let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
    match tag.find(&needle) {
        Some(at) => {
            let start = at + needle.len();
            tag[start..start + tag[start..].find('"').unwrap()].to_string()
        }
        None => String::new(),
    }
}

/// The colour choice: a readable colour is kept; otherwise the theme's text colour, else its page colour, else black
/// or white; and a translucent fill is judged as drawn.
#[test]
fn kernel_apis_diagram_contrast_picks_the_text_then_page_colour_then_black_or_white() {
    // WCAG's own reference values: black on white 21:1, #767676 on white 4.54:1.
    assert!((contrast((0, 0, 0), (255, 255, 255)) - 21.0).abs() < 1e-9);
    assert!((contrast((0x76, 0x76, 0x76), (255, 255, 255)) - 4.54).abs() < 0.005);
    let dark = palette("dark");
    let (fg, bg) = (parse_rgb("#d4d4d4").unwrap(), parse_rgb("#18181e").unwrap());
    // Already readable: unchanged.
    assert_eq!(readable_color(bg, &[(bg, fg)], &dark), bg);
    // The page colour on a dark fill fails: the text colour reaches 4.5:1 on it.
    let navy = parse_rgb("#202a60").unwrap();
    assert!(contrast(bg, navy) < 4.5);
    assert_eq!(readable_color(bg, &[(bg, navy)], &dark), fg);
    // On a light fill the text colour fails and the page colour wins.
    let pale = parse_rgb("#f0f0f0").unwrap();
    assert_eq!(readable_color(fg, &[(fg, pale)], &dark), bg);
    // A mid grey where neither theme colour reaches 4.5: black or white, whichever is better.
    let grey = parse_rgb("#7a7a7a").unwrap();
    let pick = readable_color(grey, &[(grey, grey)], &dark);
    assert!(pick == (0, 0, 0) || pick == (255, 255, 255));
    assert!(contrast(pick, grey) >= 4.5);
    // Two backdrops: the colour must hold on both.
    assert_eq!(readable_color(fg, &[(fg, pale), (fg, navy)], &dark), (0, 0, 0));
    // Through the SVG: a label in a translucent group is judged as drawn (blended), and fixed opaque.
    let svg = concat!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="40" viewBox="0 0 100 40">"##,
        r##"<rect x="0" y="0" width="100" height="40" fill="#f0f0f0"/>"##,
        r##"<g fill="#000000" fill-opacity="0.2"><text x="50" y="25" font-size="14" text-anchor="middle">faint</text></g>"##,
        r##"<g transform="translate(10,0)"><text x="0" y="25" font-size="14" fill="#1f2328">dark</text></g></svg>"##
    );
    let (fixed, labels) = ensure_contrast_report(svg, &dark);
    assert_eq!(labels.len(), 2, "{labels:?}");
    assert_eq!(labels[0].backdrop, pale);
    assert!(contrast(labels[0].drawn, pale) < 2.0, "{labels:?}");
    assert!(
        fixed.contains(r##"<text fill-opacity="1" fill="#18181e" x="50""##),
        "{fixed}"
    );
    assert!(
        fixed.contains(r##"<text x="0" y="25" font-size="14" fill="#1f2328">dark</text>"##),
        "{fixed}"
    );
}

/// A label's box follows its anchor: centred (`middle`) or ending (`end`) on its `x`, so the backdrop is the shape it
/// sits on, not the page beside it.
#[test]
fn kernel_apis_diagram_contrast_label_boxes_follow_the_text_anchor() {
    let dark = palette("dark");
    let navy = parse_rgb("#202a60").unwrap();
    let svg = concat!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="240" height="60" viewBox="0 0 240 60">"##,
        r##"<rect x="0" y="0" width="240" height="60" fill="#f0f0f0"/>"##,
        r##"<rect x="0" y="0" width="60" height="30" fill="#202a60"/>"##,
        r##"<rect x="180" y="30" width="60" height="30" fill="#202a60"/>"##,
        r##"<text x="30" y="22" font-size="14" text-anchor="middle" fill="#d4d4d4">abcdefgh</text>"##,
        r##"<text x="240" y="52" font-size="14" text-anchor="end" fill="#d4d4d4">abcdefgh</text></svg>"##
    );
    let (fixed, labels) = ensure_contrast_report(svg, &dark);
    assert_eq!(
        labels.iter().map(|l| l.backdrop).collect::<Vec<_>>(),
        [navy, navy],
        "{labels:?}"
    );
    assert_eq!(fixed, svg, "both labels already read on navy");
}
