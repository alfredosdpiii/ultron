//! Checks `inventory.tsv` against the sources it was generated from: every row parses, ids are unique and
//! derived from the row, and every `source_path:line` still exists and still names the surface.
//! Regenerate with `node crates/inventory/generate.mjs [<pi-reference-checkout>]`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

const PACKAGES: [&str; 16] = [
    "agent",
    "ai",
    "chord",
    "client",
    "coding-agent",
    "durable",
    "evals",
    "mcp",
    "protocol",
    "server",
    "session-backends-sqlite-node",
    "telemetry",
    "tui",
    "upstream-durable",
    "upstream-chord",
    "upstream-ai",
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The pinned upstream Pi checkout: `ULTRON_RUST_REFERENCE`, else a sibling of the repository or of its worktree directory.
fn reference() -> PathBuf {
    if let Some(dir) = std::env::var_os("ULTRON_RUST_REFERENCE") {
        return PathBuf::from(dir);
    }
    ["../ultron-rust-reference", "../../ultron-rust-reference"]
        .iter()
        .map(|p| repo().join(p))
        .find(|p| p.join("packages").is_dir())
        .expect("upstream reference checkout not found; set ULTRON_RUST_REFERENCE")
}

/// The id the generator derives for a row; also what makes a renamed or re-kinded row stale.
fn expected_id(package: &str, kind: &str, name: &str) -> String {
    format!("INV-{package}-{kind}-{}", name.replace(' ', "_"))
}

/// The part of a name that must appear on its source line: the text after the last `:`, `.`, `/` or space.
fn token(name: &str) -> &str {
    name.rsplit([':', '.', '/', ' ']).next().unwrap_or(name)
}

/// Checks one row against the sources under `repo` (and `reference` for upstream rows).
fn check_row(
    row: &str,
    files: &mut HashMap<PathBuf, Vec<String>>,
    repo: &Path,
    reference: &Path,
) -> Result<(), String> {
    let cols: Vec<&str> = row.split('\t').collect();
    let [id, package, kind, name, source] = cols[..] else {
        return Err(format!("expected 5 columns, got {}", cols.len()));
    };
    if !PACKAGES.contains(&package) {
        return Err(format!("unknown package {package}"));
    }
    if [kind, name].iter().any(|v| v.is_empty()) {
        return Err("empty kind or name".into());
    }
    if id != expected_id(package, kind, name) {
        return Err(format!("id does not match package/kind/name: {id}"));
    }
    let (path, line) = source
        .rsplit_once(':')
        .ok_or_else(|| format!("source without line: {source}"))?;
    let line: usize = line.parse().map_err(|_| format!("bad line number: {source}"))?;
    let file = match path.strip_prefix("upstream:") {
        Some(rest) => reference.join(rest),
        None => repo.join(path),
    };
    if package.starts_with("upstream-") != path.starts_with("upstream:") {
        return Err(format!("{package} row points at {path}"));
    }
    if !files.contains_key(&file) {
        let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        files.insert(file.clone(), text.lines().map(str::to_owned).collect());
    }
    let text = line
        .checked_sub(1)
        .and_then(|i| files[&file].get(i))
        .ok_or_else(|| format!("{source}: line does not exist"))?;
    if !text.contains(token(name)) && !text.starts_with("export * from ") {
        return Err(format!("{source} no longer names {name:?}: {text:?}"));
    }
    Ok(())
}

#[test]
fn inventory_matches_the_pinned_sources() {
    let tsv = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("inventory.tsv"))
        .expect("read inventory.tsv");
    let mut lines = tsv.lines();
    assert_eq!(lines.next(), Some("id\tpackage\tkind\tname\tsource"));
    let reference = reference();
    let mut files = HashMap::new();
    let mut ids = HashSet::new();
    let mut packages = BTreeSet::new();
    let mut failures = Vec::new();
    let mut rows = 0;
    for (i, row) in lines.enumerate() {
        rows += 1;
        if let Err(e) = check_row(row, &mut files, &repo(), &reference) {
            failures.push(format!("row {}: {e}", i + 2));
            continue;
        }
        let id = row.split('\t').next().unwrap_or_default();
        if !ids.insert(id.to_owned()) {
            failures.push(format!("row {}: duplicate id {id}", i + 2));
        }
        packages.insert(row.split('\t').nth(1).unwrap_or_default().to_owned());
    }
    assert!(
        failures.is_empty(),
        "{} stale or malformed rows:\n{}",
        failures.len(),
        failures.join("\n")
    );
    let missing: Vec<_> = PACKAGES.iter().filter(|p| !packages.contains(**p)).collect();
    assert!(missing.is_empty(), "packages without rows: {missing:?}");
    assert!(rows > 5000, "inventory shrank to {rows} rows");
}

#[test]
fn inventory_row_check_rejects_stale_rows() {
    let root = std::env::temp_dir().join(format!("inventory-test-{}", std::process::id()));
    let (repo, reference) = (root.join("repo"), root.join("reference"));
    std::fs::create_dir_all(repo.join("packages/coding-agent")).unwrap();
    std::fs::create_dir_all(reference.join("packages/ai")).unwrap();
    std::fs::write(
        repo.join("packages/coding-agent/args.ts"),
        "// flags\nif (arg === \"--model\") {\n",
    )
    .unwrap();
    std::fs::write(reference.join("packages/ai/index.ts"), "export * from \"./x.ts\";\n").unwrap();
    let row =
        |id: &str, package: &str, name: &str, source: &str| format!("{id}\t{package}\tcli-flag\t{name}\t{source}");
    let id = "INV-coding-agent-cli-flag---model";
    let cases = [
        (
            row(id, "coding-agent", "--model", "packages/coding-agent/args.ts:2"),
            None,
        ),
        (
            row(
                "INV-upstream-ai-cli-flag-x",
                "upstream-ai",
                "x",
                "upstream:packages/ai/index.ts:1",
            ),
            None,
        ),
        (
            row(id, "coding-agent", "--model", "packages/coding-agent/args.ts:1"),
            Some("no longer names"),
        ),
        (
            row(id, "coding-agent", "--model", "packages/coding-agent/args.ts:3"),
            Some("line does not exist"),
        ),
        (
            row(id, "coding-agent", "--model", "packages/coding-agent/args.ts:0"),
            Some("line does not exist"),
        ),
        (
            row(id, "coding-agent", "--model", "packages/coding-agent/gone.ts:1"),
            Some("gone.ts"),
        ),
        (
            row(id, "coding-agent", "--model", "packages/coding-agent/args.ts"),
            Some("source without line"),
        ),
        (
            row(id, "coding-agent", "--model", "packages/coding-agent/args.ts:x"),
            Some("bad line number"),
        ),
        (
            row(
                "INV-coding-agent-cli-flag---mode",
                "coding-agent",
                "--model",
                "packages/coding-agent/args.ts:2",
            ),
            Some("id does not match"),
        ),
        (
            row(
                "INV-nope-cli-flag---model",
                "nope",
                "--model",
                "packages/coding-agent/args.ts:2",
            ),
            Some("unknown package"),
        ),
        (
            row(
                "INV-upstream-ai-cli-flag---model",
                "upstream-ai",
                "--model",
                "packages/coding-agent/args.ts:2",
            ),
            Some("points at"),
        ),
        (
            row(id, "coding-agent", "--model", "upstream:packages/ai/index.ts:1"),
            Some("points at"),
        ),
        (
            format!("{id}\tcoding-agent\tcli-flag\t--model"),
            Some("expected 5 columns"),
        ),
        (
            format!(
                "{}\textra",
                row(id, "coding-agent", "--model", "packages/coding-agent/args.ts:2")
            ),
            Some("expected 5 columns, got 6"),
        ),
    ];
    let mut files = HashMap::new();
    for (row, want) in &cases {
        let got = check_row(row, &mut files, &repo, &reference);
        match want {
            None => assert_eq!(got, Ok(()), "{row}"),
            Some(fragment) => {
                let err = got.expect_err(row);
                assert!(err.contains(fragment), "{row}: {err}");
            }
        }
    }
    std::fs::remove_dir_all(&root).unwrap();
}
