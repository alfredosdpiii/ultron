//! Indexing one revision of a repository: list its tree, read only the blobs the store has not seen, record the
//! tree. A second index of a nearby revision reads a handful of blobs.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::extract::{EXTRACTOR_VERSION, Extractor, MAX_BYTES, language};
use crate::store::{Error, Store};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// The commit indexed (the full id).
    pub rev: String,
    /// Indexable files in the tree.
    pub files: usize,
    /// Blobs read this time; the others were already in the store.
    pub read: usize,
    pub ms: u128,
}

fn git(repo_dir: &Path, args: &[&str]) -> Result<Vec<u8>, Error> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error(format!("git {}: {error}", args.join(" "))))?;
    if !output.status.success() {
        return Err(Error(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// (path, blob, size) of the indexable files at `rev`.
fn tree_entries(repo_dir: &Path, rev: &str) -> Result<Vec<(String, String, u64)>, Error> {
    let listing = git(repo_dir, &["ls-tree", "-r", "-l", "-z", "--full-tree", rev])?;
    let mut entries = Vec::new();
    for record in listing.split(|&byte| byte == 0).filter(|record| !record.is_empty()) {
        let record = String::from_utf8_lossy(record);
        // `<mode> SP <type> SP <object> SP+ <size> TAB <path>`
        let Some((meta, path)) = record.split_once('\t') else { continue };
        let fields: Vec<&str> = meta.split_whitespace().collect();
        let [_mode, kind, oid, size] = fields[..] else { continue };
        if kind != "blob" || language(path).is_none() {
            continue;
        }
        let Ok(size) = size.parse::<u64>() else { continue };
        if size > MAX_BYTES {
            continue;
        }
        entries.push((path.to_string(), oid.to_string(), size));
    }
    Ok(entries)
}

/// The contents of `oids`, in order, through one `git cat-file --batch`.
fn read_blobs(repo_dir: &Path, oids: &[String]) -> Result<Vec<Vec<u8>>, Error> {
    if oids.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error(format!("git cat-file: {error}")))?;
    let mut stdin = child.stdin.take().ok_or_else(|| Error("git cat-file: no stdin".into()))?;
    let input = oids.join("\n") + "\n";
    // Written from a thread: git answers while it reads, and a full pipe either way would deadlock.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let stdout = child.stdout.take().ok_or_else(|| Error("git cat-file: no stdout".into()))?;
    let mut reader = BufReader::new(stdout);
    let mut contents = Vec::with_capacity(oids.len());
    for oid in oids {
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .map_err(|error| Error(format!("git cat-file: {error}")))?;
        let fields: Vec<&str> = header.split_whitespace().collect();
        let size = match fields[..] {
            [_, "blob", size] => size.parse::<usize>().map_err(|_| Error(format!("git cat-file: {header:?}")))?,
            _ => return Err(Error(format!("git cat-file: {oid}: {}", header.trim()))),
        };
        let mut body = vec![0; size + 1];
        reader
            .read_exact(&mut body)
            .map_err(|error| Error(format!("git cat-file: {error}")))?;
        body.truncate(size);
        contents.push(body);
    }
    let _ = writer.join();
    let _ = child.wait();
    Ok(contents)
}

/// Indexes `repo` (a stable name such as `github.com/owner/name`) at `rev` of the git checkout `repo_dir` into the
/// store at `db`. Indexing the same revision again only refreshes its tree.
pub fn index_tree(db: &Path, repo: &str, repo_dir: &Path, rev: &str) -> Result<IndexStats, Error> {
    let started = Instant::now();
    if rev.is_empty() || rev.starts_with('-') {
        return Err(Error(format!("not a revision: {rev:?}")));
    }
    let commit = git(repo_dir, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")])?;
    let commit = String::from_utf8_lossy(&commit).trim().to_string();
    let entries = tree_entries(repo_dir, &commit)?;
    let mut store = Store::open(db)?;
    let oids: Vec<&str> = entries.iter().map(|(_, oid, _)| oid.as_str()).collect();
    let known = store.known_blobs(&oids, EXTRACTOR_VERSION)?;
    let mut unread: Vec<(String, String)> = Vec::new();
    let mut queued = std::collections::HashSet::new();
    for (path, oid, _) in &entries {
        if !known.contains(oid) && queued.insert(oid.clone()) {
            unread.push((path.clone(), oid.clone()));
        }
    }
    let mut extractor = Extractor::new().map_err(Error)?;
    let mut blobs = Vec::with_capacity(unread.len());
    let unread_oids: Vec<String> = unread.iter().map(|(_, oid)| oid.clone()).collect();
    for ((path, oid), body) in unread.iter().zip(read_blobs(repo_dir, &unread_oids)?) {
        let Some(lang) = language(path) else { continue };
        // A file that is not UTF-8 is not source anyone reads; it is stored with nothing extracted.
        let extracted = match String::from_utf8(body) {
            Ok(text) => extractor.extract(lang, &text),
            Err(_) => Default::default(),
        };
        blobs.push((oid.clone(), lang.name().to_string(), extracted));
    }
    let paths: Vec<(String, String)> = entries.into_iter().map(|(path, oid, _)| (path, oid)).collect();
    let indexed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64);
    store.record_tree(repo, &commit, &paths, &blobs, EXTRACTOR_VERSION, indexed_at)?;
    Ok(IndexStats {
        rev: commit,
        files: paths.len(),
        read: blobs.len(),
        ms: started.elapsed().as_millis(),
    })
}
