//! `diagram.mermaid` (`diagram` of diagram_api.py): Mermaid text rendered to a themed SVG and a PNG.
//!
//! The host runs `node` (found on the session environment's `PATH`, never this process's) with a small runner and
//! the vendored Pele 1.7.0 (MIT, `assets/pele/`), which turn the text into an SVG; Pele's CSS variables become the
//! current Ultron theme's colours ([`crate::diagram_svg`]), labels too dark or light for what is under them are
//! recoloured ([`crate::diagram_contrast`]), and resvg draws the PNG at twice the SVG's size. Both
//! files are saved (the session's `diagrams/` output directory, or `path`). Shown, the PNG reaches the cell's tool
//! result for the TUI to draw inline; only with `to_model` does the model get the image itself, otherwise one line
//! `diagram: <png path> (WxH)`.
//!
//! Pele is embedded in the binary and written to `<cache dir>/pele-<sha256 prefix>/` on first use; nothing is ever
//! installed from a registry at run time.

use crate::diagram_svg::{self, DiagramPalette};
use crate::util::{fields, sha256_hex};
use crate::{ApiModule, Request};
use resvg::usvg::fontdb;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Pele 1.7.0's `dist/pele.js` (see `assets/pele/VERSION`).
pub const PELE_JS: &[u8] = include_bytes!("../assets/pele/pele.js");
/// Pele's MIT license, written beside it.
pub const PELE_LICENSE: &[u8] = include_bytes!("../assets/pele/LICENSE");
/// The runner node runs: Mermaid text on stdin, one JSON line on stdout.
pub const RUNNER: &[u8] = include_bytes!("diagram_runner.mjs");
/// How long node may take.
pub const NODE_TIMEOUT: Duration = Duration::from_secs(20);
/// At most this much of node's output is read (Pele's own SVG limit is 4,000,000 characters).
pub const OUTPUT_CAP: usize = 16 * 1024 * 1024;
/// The PNG is drawn at this multiple of the SVG's size.
pub const PNG_SCALE: f32 = 2.0;
/// The error when the session's `PATH` has no node.
pub const NO_NODE: &str = "diagram.mermaid needs Node.js on PATH; the source stays in the transcript";

/// The theme inputs, read at each call (the `theme` setting can change while the session runs).
#[derive(Clone, Debug, Default)]
pub struct ThemeInputs {
    /// The `theme` setting.
    pub setting: Option<String>,
    /// The session environment's `COLORFGBG`.
    pub colorfgbg: Option<String>,
    /// The terminal's appearance (`"light"` / `"dark"`) as the attached TUI detected it (OSC 11, a scheme report,
    /// else its own `COLORFGBG`): it wins over `colorfgbg` for `auto`.
    pub terminal: Option<String>,
    /// `<agent dir>/themes`, where custom themes live.
    pub custom_dir: PathBuf,
}

/// A rendered diagram to show.
pub struct ShownDiagram<'a> {
    pub lane: &'a str,
    pub png_path: &'a Path,
    pub png: &'a [u8],
    pub width: u32,
    pub height: u32,
    /// The model sees the image itself; otherwise only [`ShownDiagram::line`] reaches it.
    pub to_model: bool,
    /// `diagram: <png path> (WxH)`.
    pub line: String,
}

/// Shows a diagram in the requesting cell's result.
pub type ShowDiagram = Arc<dyn Fn(&ShownDiagram<'_>) -> Result<(), String> + Send + Sync>;

/// What the module needs from the session.
#[derive(Clone)]
pub struct DiagramConfig {
    /// The session environment's `PATH` (node is looked up there only) and `HOME`.
    pub path_var: Option<String>,
    pub home: PathBuf,
    /// Where Pele is unpacked (`<agent dir>/cache`).
    pub cache_dir: PathBuf,
    /// Where diagrams go without `path`.
    pub out_dir: PathBuf,
    /// What a relative `path` is relative to.
    pub cwd: PathBuf,
    pub theme: Arc<dyn Fn() -> ThemeInputs + Send + Sync>,
    pub show: ShowDiagram,
    /// The fonts text is drawn with (`None`: the system's).
    pub fonts: Option<Arc<fontdb::Database>>,
    pub timeout: Duration,
}

pub struct DiagramModule {
    config: DiagramConfig,
}

impl DiagramModule {
    pub fn new(config: DiagramConfig) -> Self {
        Self { config }
    }
}

/// The first executable `node` in the directories of `path` (absolute entries only).
pub fn find_node(path: Option<&str>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    path?
        .split(':')
        .filter(|dir| dir.starts_with('/'))
        .map(|dir| Path::new(dir).join("node"))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// The name of the directory Pele and the runner are unpacked into: `pele-<first 16 hex digits of their sha256>`.
pub fn pele_dir_name() -> String {
    let mut bytes = PELE_JS.to_vec();
    bytes.extend_from_slice(RUNNER);
    format!("pele-{}", &sha256_hex(&bytes)[..16])
}

/// Pele, its license and the runner under `cache_dir` (written once; a complete directory is reused): the runner's
/// path.
pub fn install_pele(cache_dir: &Path) -> Result<PathBuf, String> {
    let dir = cache_dir.join(pele_dir_name());
    let runner = dir.join("runner.mjs");
    let complete = |dir: &Path| {
        std::fs::metadata(dir.join("pele.mjs")).is_ok_and(|m| m.len() == PELE_JS.len() as u64)
            && std::fs::read(dir.join("runner.mjs")).is_ok_and(|bytes| bytes == RUNNER)
    };
    if complete(&dir) {
        return Ok(runner);
    }
    let failed = |e: std::io::Error| format!("diagram.mermaid: cannot unpack Pele into {}: {e}", cache_dir.display());
    std::fs::create_dir_all(cache_dir).map_err(failed)?;
    let staging = cache_dir.join(format!(".{}-{}", pele_dir_name(), crate::util::uuid()));
    std::fs::create_dir_all(&staging).map_err(failed)?;
    let written = [("pele.mjs", PELE_JS), ("LICENSE", PELE_LICENSE), ("runner.mjs", RUNNER)]
        .iter()
        .try_for_each(|(name, bytes)| std::fs::write(staging.join(name), bytes));
    if let Err(error) = written {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(failed(error));
    }
    if dir.exists() && !complete(&dir) {
        // A damaged directory (an interrupted older write) is replaced.
        let _ = std::fs::remove_dir_all(&dir);
    }
    if let Err(error) = std::fs::rename(&staging, &dir) {
        let _ = std::fs::remove_dir_all(&staging);
        if !complete(&dir) {
            return Err(failed(error));
        }
    }
    Ok(runner)
}

/// What Pele made of the text.
#[derive(Clone, Debug, PartialEq)]
pub struct PeleOutput {
    /// Pele's diagram type (`flowchart`, `sequence`, ...).
    pub kind: String,
    pub svg: String,
}

/// The runner's JSON answer: the SVG, or Pele's error as the call's error.
pub fn parse_runner_output(stdout: &str) -> Result<PeleOutput, String> {
    let line = stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    let answer: Value =
        serde_json::from_str(line).map_err(|_| "diagram.mermaid: the renderer answered no JSON".to_string())?;
    if answer["ok"] != true {
        let message = answer["error"].as_str().unwrap_or("Pele failed").to_string();
        // Pele's message usually names the line itself ("Parse error on line 3: ...").
        let line = answer["line"]
            .as_u64()
            .filter(|l| *l > 0 && !message.contains(&format!("line {l}")));
        return Err(match line {
            Some(line) => format!(
                "{message} (line {line}, column {})",
                answer["column"].as_u64().unwrap_or(0)
            ),
            None => message,
        });
    }
    let svg = answer["svg"]
        .as_str()
        .filter(|svg| svg.contains("<svg"))
        .ok_or("diagram.mermaid: the renderer answered no SVG")?;
    Ok(PeleOutput {
        kind: answer["type"].as_str().unwrap_or("diagram").to_string(),
        svg: svg.to_string(),
    })
}

/// A file-name slug for a diagram: its `title` (front matter `title:` or a `title ...` line), else its type;
/// lowercase letters, digits and single dashes, at most 40 characters.
pub fn slug(text: &str, kind: &str) -> String {
    let title = text.lines().map(str::trim).find_map(|line| {
        line.strip_prefix("title:")
            .or_else(|| line.strip_prefix("title "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
    });
    let mut out = String::new();
    for c in title.unwrap_or(kind).chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "diagram".into()
    } else {
        out.to_string()
    }
}

/// Where the SVG and PNG go: without `path`, or with `path` an existing directory, `<slug>-<n>.svg` / `.png` with the
/// first `n` from 1 that neither file uses; otherwise `path` itself (an `.svg` or `.png` extension dropped) with
/// both extensions.
pub fn output_paths(path: Option<&Path>, default_dir: &Path, slug: &str) -> (PathBuf, PathBuf) {
    let directory = match path {
        None => Some(default_dir),
        Some(path) if path.is_dir() => Some(path),
        Some(_) => None,
    };
    if let Some(directory) = directory {
        let mut n = 1;
        loop {
            let (svg, png) = (
                directory.join(format!("{slug}-{n}.svg")),
                directory.join(format!("{slug}-{n}.png")),
            );
            if !svg.exists() && !png.exists() {
                return (svg, png);
            }
            n += 1;
        }
    }
    let path = path.unwrap_or(default_dir);
    let stem = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("svg") || ext.eq_ignore_ascii_case("png") => path.with_extension(""),
        _ => path.to_path_buf(),
    };
    let with = |ext: &str| PathBuf::from(format!("{}.{ext}", stem.display()));
    (with("svg"), with("png"))
}

fn flag(request: &Request<'_>, key: &str, default: bool) -> Result<bool, String> {
    match request.payload.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("diagram.mermaid {key} must be True or False")),
    }
}

/// Reads all of `reader`, keeping at most `cap` bytes (and whether more came).
fn read_capped(mut reader: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut over = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => return (kept, over),
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..n.min(room)]);
                over |= n > room;
            }
        }
    }
}

impl DiagramModule {
    /// Pele's SVG for `text`, from node on the session's PATH.
    fn render(&self, text: &str, signal: &kernel::Signal) -> Result<PeleOutput, String> {
        let node = find_node(self.config.path_var.as_deref()).ok_or(NO_NODE)?;
        let runner = install_pele(&self.config.cache_dir)?;
        let mut command = Command::new(&node);
        command
            .arg(&runner)
            .current_dir(runner.parent().unwrap_or(Path::new("/")))
            .env_clear()
            .env("HOME", &self.config.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(path) = &self.config.path_var {
            command.env("PATH", path);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("diagram.mermaid: cannot run {}: {e}", node.display()))?;
        let mut stdin = child.stdin.take();
        let input = text.to_string();
        let writer = std::thread::spawn(move || {
            if let Some(stdin) = stdin.as_mut() {
                let _ = stdin.write_all(input.as_bytes());
            }
        });
        let stdout = child.stdout.take();
        let reader = std::thread::spawn(move || stdout.map(|out| read_capped(out, OUTPUT_CAP)).unwrap_or_default());
        let stderr = child.stderr.take();
        let errors = std::thread::spawn(move || stderr.map(|err| read_capped(err, 8 * 1024)).unwrap_or_default());
        let deadline = Instant::now() + self.config.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(e) => break Err(format!("diagram.mermaid: {e}")),
            }
            if Instant::now() >= deadline {
                break Err(format!(
                    "diagram.mermaid: node did not finish within {} s",
                    self.config.timeout.as_secs()
                ));
            }
            if signal.wait(Duration::from_millis(10)) {
                break Err("diagram.mermaid was cancelled".to_string());
            }
        };
        if status.is_err() {
            // Only the node this call started is stopped.
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = writer.join();
        let (out, over) = reader.join().unwrap_or_default();
        let (err, _) = errors.join().unwrap_or_default();
        let status = status?;
        if over {
            return Err(format!(
                "diagram.mermaid: the renderer's output is over {} MiB",
                OUTPUT_CAP / 1024 / 1024
            ));
        }
        let out = String::from_utf8_lossy(&out);
        match parse_runner_output(&out) {
            Ok(output) => Ok(output),
            Err(error) if status.success() || out.contains("\"ok\"") => Err(error),
            Err(_) => {
                let err = String::from_utf8_lossy(&err);
                let tail: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
                let tail = tail[tail.len().saturating_sub(3)..].join("\n");
                Err(format!("diagram.mermaid: node failed ({status}): {tail}"))
            }
        }
    }

    fn palette(&self, requested: &str) -> Result<DiagramPalette, String> {
        let inputs = (self.config.theme)();
        diagram_svg::select_palette_for_terminal(
            requested,
            inputs.setting.as_deref(),
            inputs.terminal.as_deref(),
            inputs.colorfgbg.as_deref(),
            &inputs.custom_dir,
        )
    }

    fn mermaid(&self, request: &Request<'_>) -> Result<Value, String> {
        fields(request.payload, &["text", "theme", "show", "to_model", "path"])?;
        let text = match request.payload.get("text") {
            Some(Value::String(text)) if !text.trim().is_empty() => text.as_str(),
            _ => return Err("diagram.mermaid text must be a non-empty string".into()),
        };
        let theme = match request.payload.get("theme") {
            None | Some(Value::Null) => "auto",
            Some(Value::String(theme)) => theme.as_str(),
            Some(_) => return Err("diagram.mermaid theme must be \"auto\", \"dark\" or \"light\"".into()),
        };
        let (show, to_model) = (flag(request, "show", true)?, flag(request, "to_model", false)?);
        let path = match request.payload.get("path") {
            None | Some(Value::Null) => None,
            Some(Value::String(path)) if !path.trim().is_empty() => Some(self.config.cwd.join(path)),
            Some(_) => return Err("diagram.mermaid path must be a non-empty string".into()),
        };
        let palette = self.palette(theme)?;
        let output = self.render(text, request.signal)?;
        let svg = diagram_svg::flatten_svg(&output.svg, &palette);
        // Labels Pele coloured for its own palette stay readable on the Ultron theme's fills.
        let svg = crate::diagram_contrast::ensure_contrast(&svg, &palette);
        let fonts = self.config.fonts.clone().unwrap_or_else(diagram_svg::system_fonts);
        let png = diagram_svg::rasterize(&svg, PNG_SCALE, palette.background(), fonts)?;
        let (svg_path, png_path) = output_paths(path.as_deref(), &self.config.out_dir, &slug(text, &output.kind));
        for (file, bytes) in [(&svg_path, svg.as_bytes()), (&png_path, png.data.as_slice())] {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("diagram.mermaid: {}: {e}", parent.display()))?;
            }
            std::fs::write(file, bytes).map_err(|e| format!("diagram.mermaid: {}: {e}", file.display()))?;
        }
        if show || to_model {
            (self.config.show)(&ShownDiagram {
                lane: request.lane,
                png_path: &png_path,
                png: &png.data,
                width: png.width,
                height: png.height,
                to_model,
                line: format!("diagram: {} ({}x{})", png_path.display(), png.width, png.height),
            })?;
        }
        Ok(json!({
            "svg_path": svg_path.display().to_string(),
            "png_path": png_path.display().to_string(),
            "width": png.width,
            "height": png.height,
            "type": output.kind,
        }))
    }
}

impl ApiModule for DiagramModule {
    fn prefixes(&self) -> &[&str] {
        &["diagram."]
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        match request.kind {
            "diagram.mermaid" => self.mermaid(request),
            other => Err(format!("Unknown diagram request: {other}")),
        }
    }
}
