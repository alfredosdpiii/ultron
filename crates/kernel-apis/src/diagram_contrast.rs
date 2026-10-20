//! Readable diagram text: every label of a flattened diagram SVG ([`crate::diagram_svg::flatten_svg`]) meets WCAG
//! contrast 4.5:1 against what is drawn under it.
//!
//! Pele picks one text colour per diagram kind (pie slice percentages in the page colour, node labels in the text
//! colour, ...), which suits its own palette; over an Ultron theme a dark page colour lands on dark slices. The host
//! draws the SVG once without its text, reads the colour under each label (the most common pixel colour in the
//! label's estimated box), and gives a label that does not reach 4.5:1 the theme's text colour, else its page
//! colour, else black or white (one of them always reaches it). Labels that already reach it are left alone. The
//! label box is estimated from its position, font size and anchor (no fonts are needed): `0.55 em` per character,
//! from `0.7 em` above the baseline to it.

use crate::diagram_svg::DiagramPalette;
use resvg::tiny_skia;
use resvg::usvg;
use resvg::usvg::fontdb;
use std::sync::Arc;

/// The WCAG AA contrast for normal text.
pub const MIN_CONTRAST: f64 = 4.5;

/// An sRGB colour.
pub type Rgb = (u8, u8, u8);

/// `#rgb` / `#rrggbb` (case-insensitive) as a colour.
pub fn parse_rgb(color: &str) -> Option<Rgb> {
    let hex = color.trim().strip_prefix('#')?;
    let digits: Vec<u8> = hex
        .chars()
        .map(|c| c.to_digit(16).map(|d| d as u8))
        .collect::<Option<_>>()?;
    match digits.as_slice() {
        [r, g, b] => Some((r * 17, g * 17, b * 17)),
        [r1, r2, g1, g2, b1, b2] => Some((r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2)),
        _ => None,
    }
}

/// WCAG relative luminance.
pub fn luminance((r, g, b): Rgb) -> f64 {
    let channel = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// WCAG contrast ratio of two colours (1 to 21).
pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// `color` drawn at `opacity` over `backdrop`.
fn blend(color: Rgb, opacity: f64, backdrop: Rgb) -> Rgb {
    let mix = |c: u8, b: u8| (f64::from(c) * opacity + f64::from(b) * (1.0 - opacity)).round() as u8;
    (
        mix(color.0, backdrop.0),
        mix(color.1, backdrop.1),
        mix(color.2, backdrop.2),
    )
}

/// The colour a label gets: `current` (drawn over each backdrop as `drawn`) when it reaches [`MIN_CONTRAST`] on every
/// backdrop, else the first of the palette's text and page colours that does, else black or white, whichever has the
/// better worst contrast.
pub fn readable_color(current: Rgb, drawn: &[(Rgb, Rgb)], palette: &DiagramPalette) -> Rgb {
    if drawn
        .iter()
        .all(|(color, under)| contrast(*color, *under) >= MIN_CONTRAST)
    {
        return current;
    }
    let worst = |color: Rgb| {
        drawn
            .iter()
            .map(|(_, under)| contrast(color, *under))
            .fold(f64::INFINITY, f64::min)
    };
    [palette.foreground(), palette.background()]
        .iter()
        .filter_map(|c| parse_rgb(c))
        .find(|c| worst(*c) >= MIN_CONTRAST)
        .unwrap_or_else(|| {
            let (black, white) = ((0, 0, 0), (255, 255, 255));
            if worst(black) >= worst(white) { black } else { white }
        })
}

/// A 2D affine transform `[a, b, c, d, e, f]`: `x' = a x + c y + e`, `y' = b x + d y + f`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Affine([f64; 6]);

impl Affine {
    const IDENTITY: Affine = Affine([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    /// `self` applied after `inner` (an inner element's transform inside this one's).
    fn then(self, inner: Affine) -> Affine {
        let [a, b, c, d, e, f] = self.0;
        let [a2, b2, c2, d2, e2, f2] = inner.0;
        Affine([
            a * a2 + c * b2,
            b * a2 + d * b2,
            a * c2 + c * d2,
            b * c2 + d * d2,
            a * e2 + c * f2 + e,
            b * e2 + d * f2 + f,
        ])
    }

    fn apply(self, x: f64, y: f64) -> (f64, f64) {
        let [a, b, c, d, e, f] = self.0;
        (a * x + c * y + e, b * x + d * y + f)
    }
}

/// An SVG `transform` list (`translate`, `scale`, `rotate`, `matrix`; anything else is ignored).
fn parse_transform(text: &str) -> Affine {
    let mut out = Affine::IDENTITY;
    let mut rest = text;
    while let Some(open) = rest.find('(') {
        let name = rest[..open].trim().trim_start_matches(',').trim();
        let Some(close) = rest[open..].find(')') else { break };
        let args: Vec<f64> = rest[open + 1..open + close]
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        let step = match (name, args.as_slice()) {
            ("translate", [tx]) => Affine([1.0, 0.0, 0.0, 1.0, *tx, 0.0]),
            ("translate", [tx, ty, ..]) => Affine([1.0, 0.0, 0.0, 1.0, *tx, *ty]),
            ("scale", [s]) => Affine([*s, 0.0, 0.0, *s, 0.0, 0.0]),
            ("scale", [sx, sy, ..]) => Affine([*sx, 0.0, 0.0, *sy, 0.0, 0.0]),
            ("rotate", [angle, rest @ ..]) => {
                let (sin, cos) = angle.to_radians().sin_cos();
                let rotation = Affine([cos, sin, -sin, cos, 0.0, 0.0]);
                match rest {
                    [cx, cy, ..] => Affine([1.0, 0.0, 0.0, 1.0, *cx, *cy])
                        .then(rotation)
                        .then(Affine([1.0, 0.0, 0.0, 1.0, -cx, -cy])),
                    _ => rotation,
                }
            }
            ("matrix", [a, b, c, d, e, f, ..]) => Affine([*a, *b, *c, *d, *e, *f]),
            _ => Affine::IDENTITY,
        };
        out = out.then(step);
        rest = &rest[open + close + 1..];
    }
    out
}

/// The value of attribute `name` in a start tag.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!(" {name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let len = tag[start..].find('"')?;
    Some(&tag[start..start + len])
}

/// The value of declaration `name` in a `style` attribute.
fn style_property<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    attribute(tag, "style")?.split(';').find_map(|declaration| {
        let (key, value) = declaration.split_once(':')?;
        (key.trim() == name).then(|| value.trim())
    })
}

/// A presentation property: the `style` declaration, else the attribute.
fn property<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    style_property(tag, name).or_else(|| attribute(tag, name))
}

/// The font size a tag sets (`font-size`, or the size in a `font` shorthand), in user units.
fn font_size(tag: &str) -> Option<f64> {
    let number = |s: &str| s.trim().trim_end_matches("px").parse::<f64>().ok();
    property(tag, "font-size").and_then(number).or_else(|| {
        style_property(tag, "font")?
            .split_whitespace()
            .find(|word| word.ends_with("px"))
            .and_then(number)
    })
}

/// What an element passes on to its children.
#[derive(Clone, Debug)]
struct Inherited {
    transform: Affine,
    fill: Option<String>,
    fill_opacity: f64,
    color: Option<String>,
    font_size: f64,
    anchor: String,
}

impl Inherited {
    fn child(&self, tag: &str) -> Inherited {
        let mut out = self.clone();
        if let Some(transform) = attribute(tag, "transform") {
            out.transform = self.transform.then(parse_transform(transform));
        }
        if let Some(color) = property(tag, "color") {
            out.color = Some(color.to_string());
        }
        if let Some(fill) = property(tag, "fill") {
            out.fill = Some(match fill.trim() {
                "currentColor" => out.color.clone().unwrap_or_default(),
                fill => fill.to_string(),
            });
        }
        if let Some(opacity) = property(tag, "fill-opacity").and_then(|o| o.trim().parse::<f64>().ok()) {
            out.fill_opacity = self.fill_opacity * opacity.clamp(0.0, 1.0);
        }
        if let Some(size) = font_size(tag) {
            out.font_size = size;
        }
        if let Some(anchor) = property(tag, "text-anchor") {
            out.anchor = anchor.trim().to_string();
        }
        out
    }
}

/// One line of a label: its anchor point (the element's `x`, `y`) and its characters.
#[derive(Clone, Debug)]
struct Line {
    x: f64,
    y: f64,
    chars: usize,
    style: Inherited,
}

/// A tag whose fill decides some label's colour, and the lines drawn in it.
#[derive(Debug)]
struct Run {
    /// The byte range of the start tag in the SVG.
    tag: (usize, usize),
    style: Inherited,
    lines: Vec<Line>,
}

/// A label's text with the five XML entities (and numeric ones) counted as one character.
fn char_count(text: &str) -> usize {
    let mut count = 0;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c == '&'
            && let Some(end) = rest.find(';').filter(|end| *end <= 10)
        {
            rest = &rest[end + 1..];
        } else {
            rest = &rest[c.len_utf8()..];
        }
        count += 1;
    }
    count
}

/// The index just past the `>` closing the tag that starts at `open`.
fn tag_end(text: &str, open: usize) -> usize {
    let mut quote: Option<char> = None;
    for (i, c) in text[open..].char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return open + i + 1,
            _ => {}
        }
    }
    text.len()
}

fn element_name(tag: &str) -> &str {
    let body = tag.trim_start_matches('<').trim_start_matches('/');
    let end = body
        .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .unwrap_or(body.len());
    &body[..end]
}

/// The labels of `svg` (each `<text>`, and each `<tspan>` with its own fill), and the SVG without its `<text>`
/// elements.
fn scan(svg: &str, root: Inherited) -> (Vec<Run>, String) {
    let mut runs = Vec::new();
    let mut textless = String::with_capacity(svg.len());
    let mut stack = vec![root];
    let mut at = 0;
    while let Some(offset) = svg[at..].find('<') {
        let open = at + offset;
        textless.push_str(&svg[at..open]);
        let end = tag_end(svg, open);
        let tag = &svg[open..end];
        at = end;
        if tag.starts_with("<!") || tag.starts_with("<?") {
            textless.push_str(tag);
            continue;
        }
        if tag.starts_with("</") {
            if stack.len() > 1 {
                stack.pop();
            }
            textless.push_str(tag);
            continue;
        }
        let parent = stack.last().cloned().unwrap_or_else(default_root);
        let self_closing = tag.ends_with("/>");
        if element_name(tag) == "text" && !self_closing {
            let close = svg[end..].find("</text>").map_or(svg.len(), |i| end + i);
            runs.extend(text_runs(svg, (open, end), close, &parent));
            at = (close + "</text>".len()).min(svg.len());
            continue;
        }
        textless.push_str(tag);
        if !self_closing {
            stack.push(parent.child(tag));
        }
    }
    textless.push_str(&svg[at..]);
    (runs, textless)
}

fn default_root() -> Inherited {
    Inherited {
        transform: Affine::IDENTITY,
        fill: None,
        fill_opacity: 1.0,
        color: None,
        font_size: 16.0,
        anchor: "start".into(),
    }
}

/// The runs of one `<text>` element (start tag at `tag`, content up to `close`).
fn text_runs(svg: &str, tag: (usize, usize), close: usize, parent: &Inherited) -> Vec<Run> {
    let text_tag = &svg[tag.0..tag.1];
    let text_style = parent.child(text_tag);
    let number = |tag: &str, name: &str| attribute(tag, name).and_then(|v| v.trim().parse::<f64>().ok());
    let mut text_run = Run {
        tag,
        style: text_style.clone(),
        lines: Vec::new(),
    };
    let mut own_runs: Vec<Run> = Vec::new();
    // The current line, and which run it belongs to (None: the text element's).
    let mut line = Line {
        x: number(text_tag, "x").unwrap_or(0.0),
        y: number(text_tag, "y").unwrap_or(0.0),
        chars: 0,
        style: text_style.clone(),
    };
    let mut owner: Option<usize> = None;
    let mut stack: Vec<(Inherited, Option<usize>)> = vec![(text_style, None)];
    let mut at = tag.1;
    let flush = |line: &mut Line, owner: Option<usize>, text_run: &mut Run, own_runs: &mut Vec<Run>| {
        if line.chars > 0 {
            let run = match owner {
                Some(i) => &mut own_runs[i],
                None => &mut *text_run,
            };
            run.lines.push(line.clone());
        }
        line.chars = 0;
    };
    while at < close {
        let next = svg[at..close].find('<').map_or(close, |i| at + i);
        line.chars += char_count(svg[at..next].trim_matches(['\n', '\r', '\t']));
        if next >= close {
            break;
        }
        let end = tag_end(svg, next).min(close);
        let inner = &svg[next..end];
        at = end;
        if inner.starts_with("</") {
            if stack.len() > 1 {
                flush(&mut line, owner, &mut text_run, &mut own_runs);
                stack.pop();
                if let Some((style, run)) = stack.last() {
                    line.style = style.clone();
                    owner = *run;
                }
            }
            continue;
        }
        if inner.starts_with("<!") || inner.starts_with("<?") {
            continue;
        }
        let (style, run) = stack.last().cloned().unwrap_or((parent.clone(), None));
        let style = style.child(inner);
        let run = if property(inner, "fill").is_some() || property(inner, "fill-opacity").is_some() {
            own_runs.push(Run {
                tag: (next, end),
                style: style.clone(),
                lines: Vec::new(),
            });
            Some(own_runs.len() - 1)
        } else {
            run
        };
        flush(&mut line, owner, &mut text_run, &mut own_runs);
        if let Some(x) = number(inner, "x") {
            line.x = x;
        }
        if let Some(y) = number(inner, "y") {
            line.y = y;
        }
        line.style = style.clone();
        owner = run;
        if !inner.ends_with("/>") {
            stack.push((style, run));
        }
    }
    flush(&mut line, owner, &mut text_run, &mut own_runs);
    let mut runs = vec![text_run];
    runs.extend(own_runs);
    runs
}

/// The estimated box of a line, in SVG user units: (min x, min y, max x, max y).
fn line_box(line: &Line) -> (f64, f64, f64, f64) {
    let size = line.style.font_size;
    let width = line.chars as f64 * size * 0.55;
    let left = match line.style.anchor.as_str() {
        "middle" => line.x - width / 2.0,
        "end" => line.x - width,
        _ => line.x,
    };
    // The middle of the glyphs: a little in from each side, from the x-height to the baseline.
    let (x0, x1) = (left + width * 0.1, left + width * 0.9);
    let (y0, y1) = (line.y - size * 0.7, line.y - size * 0.1);
    let corners = [(x0, y0), (x1, y0), (x0, y1), (x1, y1)].map(|(x, y)| line.style.transform.apply(x, y));
    let xs = corners.map(|c| c.0);
    let ys = corners.map(|c| c.1);
    let min = |v: [f64; 4]| v.iter().copied().fold(f64::INFINITY, f64::min);
    let max = |v: [f64; 4]| v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (min(xs), min(ys), max(xs), max(ys))
}

/// The most common colour of `pixmap` inside `area` (pixel units), if the area covers any pixel.
fn backdrop(pixmap: &tiny_skia::Pixmap, area: (f64, f64, f64, f64)) -> Option<Rgb> {
    let (width, height) = (pixmap.width() as i64, pixmap.height() as i64);
    let x0 = (area.0.floor() as i64).max(0);
    let y0 = (area.1.floor() as i64).max(0);
    let x1 = (area.2.ceil() as i64).min(width);
    let y1 = (area.3.ceil() as i64).min(height);
    let mut counts: Vec<(Rgb, usize)> = Vec::new();
    for y in y0..y1 {
        for x in x0..x1 {
            let pixel = pixmap.pixel(x as u32, y as u32)?.demultiply();
            let rgb = (pixel.red(), pixel.green(), pixel.blue());
            match counts.iter_mut().find(|(c, _)| *c == rgb) {
                Some((_, n)) => *n += 1,
                None => counts.push((rgb, 1)),
            }
        }
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(c, _)| c)
}

/// The SVG's root `viewBox` → pixel transform at `scale` (identity when it has none).
fn view_transform(svg: &str, scale: f64) -> Affine {
    let root = svg.find("<svg").map(|open| &svg[open..tag_end(svg, open)]);
    let numbers = |name: &str| -> Vec<f64> {
        root.and_then(|tag| attribute(tag, name))
            .map(|v| v.split([' ', ',']).filter_map(|s| s.trim().parse().ok()).collect())
            .unwrap_or_default()
    };
    let (width, height) = (numbers("width"), numbers("height"));
    match numbers("viewBox").as_slice() {
        [x, y, w, h] if *w > 0.0 && *h > 0.0 => {
            let sx = width.first().map_or(1.0, |v| v / w);
            let sy = height.first().map_or(1.0, |v| v / h);
            Affine([sx * scale, 0.0, 0.0, sy * scale, -x * sx * scale, -y * sy * scale])
        }
        _ => Affine([scale, 0.0, 0.0, scale, 0.0, 0.0]),
    }
}

/// `tag` with attribute `name` set to `value` (replaced, or added after the element name).
fn set_attribute(tag: &str, name: &str, value: &str) -> String {
    let needle = format!(" {name}=\"");
    if let Some(at) = tag.find(&needle) {
        let start = at + needle.len();
        if let Some(len) = tag[start..].find('"') {
            return format!("{}{value}{}", &tag[..start], &tag[start + len..]);
        }
    }
    let name_end = 1 + element_name(tag).len();
    format!("{} {name}=\"{value}\"{}", &tag[..name_end], &tag[name_end..])
}

/// `tag` without the `name` declaration of its `style` attribute (it would win over the attribute).
fn drop_style_property(tag: &str, name: &str) -> String {
    let Some(style) = attribute(tag, "style") else {
        return tag.to_string();
    };
    let kept: Vec<&str> = style
        .split(';')
        .filter(|d| d.split_once(':').is_none_or(|(k, _)| k.trim() != name))
        .collect();
    tag.replacen(
        &format!(" style=\"{style}\""),
        &format!(" style=\"{}\"", kept.join(";")),
        1,
    )
}

/// What [`ensure_contrast`] found for one label run.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelColor {
    /// The run's start tag, as it was.
    pub tag: String,
    /// The colour it was drawn in (its fill at its opacity over the backdrop) and the backdrop.
    pub drawn: Rgb,
    pub backdrop: Rgb,
    /// The colour it is drawn in now.
    pub color: Rgb,
}

/// The labels of a flattened diagram SVG and the colour under each one: Pele's text drawn on `palette`'s page,
/// without fonts.
pub fn label_backdrops(svg: &str, palette: &DiagramPalette) -> Vec<LabelColor> {
    ensure_contrast_report(svg, palette).1
}

/// `svg` with every label that is not readable against its backdrop recoloured ([`readable_color`]).
pub fn ensure_contrast(svg: &str, palette: &DiagramPalette) -> String {
    ensure_contrast_report(svg, palette).0
}

/// [`ensure_contrast`], and what it found for each label run.
pub fn ensure_contrast_report(svg: &str, palette: &DiagramPalette) -> (String, Vec<LabelColor>) {
    let root = Inherited {
        fill: Some("#000000".into()),
        ..default_root()
    };
    let (runs, textless) = scan(svg, root);
    if runs.iter().all(|r| r.lines.is_empty()) {
        return (svg.to_string(), Vec::new());
    }
    const SCALE: f64 = 2.0;
    let Some(pixmap) = draw(&textless, SCALE as f32, palette.background()) else {
        return (svg.to_string(), Vec::new());
    };
    let view = view_transform(svg, SCALE);
    let page = parse_rgb(palette.background()).unwrap_or((0, 0, 0));
    let mut edits: Vec<((usize, usize), String)> = Vec::new();
    let mut report = Vec::new();
    for run in &runs {
        let Some(fill) = run.style.fill.as_deref().and_then(parse_rgb) else {
            continue;
        };
        let opacity = run.style.fill_opacity;
        let drawn: Vec<(Rgb, Rgb)> = run
            .lines
            .iter()
            .map(|line| {
                let (x0, y0, x1, y1) = line_box(line);
                let (px0, py0) = view.apply(x0, y0);
                let (px1, py1) = view.apply(x1, y1);
                let area = (px0.min(px1), py0.min(py1), px0.max(px1), py0.max(py1));
                let under = backdrop(&pixmap, area).unwrap_or(page);
                (blend(fill, opacity, under), under)
            })
            .collect();
        if drawn.is_empty() {
            continue;
        }
        let color = readable_color(fill, &drawn, palette);
        let changed = color != fill || (opacity < 1.0 && drawn.iter().any(|(d, u)| contrast(*d, *u) < MIN_CONTRAST));
        let tag = &svg[run.tag.0..run.tag.1];
        for (drawn, under) in &drawn {
            report.push(LabelColor {
                tag: tag.to_string(),
                drawn: *drawn,
                backdrop: *under,
                color: if changed { color } else { *drawn },
            });
        }
        if changed {
            let (r, g, b) = color;
            let mut tag = set_attribute(
                &drop_style_property(tag, "fill"),
                "fill",
                &format!("#{r:02x}{g:02x}{b:02x}"),
            );
            if opacity < 1.0 {
                tag = set_attribute(&drop_style_property(&tag, "fill-opacity"), "fill-opacity", "1");
            }
            edits.push((run.tag, tag));
        }
    }
    edits.sort_by_key(|(range, _)| range.0);
    let mut out = String::with_capacity(svg.len() + edits.len() * 20);
    let mut at = 0;
    for ((start, end), tag) in edits {
        out.push_str(&svg[at..start]);
        out.push_str(&tag);
        at = end;
    }
    out.push_str(&svg[at..]);
    (out, report)
}

/// The SVG drawn at `scale` on `background`, without fonts.
fn draw(svg: &str, scale: f32, background: &str) -> Option<tiny_skia::Pixmap> {
    let options = usvg::Options {
        fontdb: Arc::new(fontdb::Database::new()),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_str(svg, &options).ok()?;
    let size = tree.size();
    let width = ((size.width() * scale).ceil() as u32).clamp(1, 16384);
    let height = ((size.height() * scale).ceil() as u32).clamp(1, 16384);
    let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
    if let Some((r, g, b)) = parse_rgb(background) {
        pixmap.fill(tiny_skia::Color::from_rgba8(r, g, b, 255));
    }
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some(pixmap)
}
