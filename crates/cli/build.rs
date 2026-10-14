//! Embeds the package files the binary reads at run time (see `src/assets.rs`): the Python runtime (`runtime.py`,
//! its sibling modules and `secret-patterns.json`), the bundled skills, the docs and the changelog. Writes
//! `$OUT_DIR/assets.rs` with one `include_bytes!` per file, in path order, and the SHA-256 of the whole set (paths,
//! modes and bytes), which names the directory the files are installed into.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Which file names to embed; `""` asks whether to descend into subdirectories.
type Keep = fn(&str) -> bool;

/// (directory inside the package, file-name filter).
const TREES: &[(&str, Keep)] = &[
    ("docs", |_| true),
    ("skills", |_| true),
    ("src/ultron/rlm", |name| {
        name.ends_with(".py") || name == "secret-patterns.json"
    }),
];
const FILES: &[&str] = &["CHANGELOG.md"];

/// Files under `dir` that `keep` accepts, in path order. `keep("")` says whether to descend into subdirectories.
fn walk(dir: &Path, keep: Keep, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // Never ship editor files, caches or bytecode.
        if name.starts_with('.') || name == "__pycache__" || name.ends_with(".pyc") {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            // The runtime directory's filter rejects "": its modules are flat.
            if keep("") {
                walk(&path, keep, out);
            }
        } else if meta.is_file() && keep(&name) {
            out.push(path);
        } else if meta.file_type().is_symlink() {
            panic!("asset {} is a symlink; embed real files only", path.display());
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let package = manifest.join("../../packages/coding-agent").canonicalize().unwrap();
    let mut files = Vec::new();
    for (tree, keep) in TREES {
        let dir = package.join(tree);
        println!("cargo:rerun-if-changed={}", dir.display());
        walk(&dir, *keep, &mut files);
    }
    for file in FILES {
        let path = package.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        files.push(path);
    }
    let mut hash = Sha256::new();
    let mut code = String::from("pub const FILES: &[Asset] = &[\n");
    for path in &files {
        let relative = path
            .strip_prefix(&package)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(path).unwrap();
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0
        };
        let mode: u32 = if executable { 0o755 } else { 0o644 };
        for part in [
            relative.as_bytes(),
            &mode.to_le_bytes(),
            &(bytes.len() as u64).to_le_bytes(),
            &bytes,
        ] {
            hash.update(part);
        }
        code.push_str(&format!(
            "    Asset {{ path: {relative:?}, mode: {mode:#o}, bytes: include_bytes!({:?}) }},\n",
            path.display().to_string()
        ));
    }
    code.push_str("];\n");
    let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    code.push_str(&format!("pub const HASH: &str = {digest:?};\n"));
    // The checkout a dev build may read live (ULTRON_DEV_ASSETS); a release binary carries no build path.
    let checkout = match std::env::var("PROFILE").as_deref() {
        Ok("debug") => format!("Some({:?})", package.display().to_string()),
        _ => "None".to_string(),
    };
    code.push_str(&format!("pub const CHECKOUT: Option<&str> = {checkout};\n"));
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("assets.rs");
    std::fs::write(out, code).unwrap();
}
