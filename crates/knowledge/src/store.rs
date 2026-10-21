//! The store: schema, versioning and the writes the indexer makes.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::extract::{Construct, Extracted, Symbol};

/// The schema the kernel reader (`rlm/knowledge_api.py`) is written against. Bump it with every change to a table the
/// reader uses, and teach the reader the new version in the same change.
pub const SCHEMA_VERSION: i64 = 3;

/// Characters of a note's body kept.
pub const NOTE_CHARS: usize = 2000;

/// Trees kept per repository: enough for the reviews in flight and their re-reviews; older ones are dropped.
pub const TREES_PER_REPO: i64 = 50;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS repos (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
-- One row per indexed blob; `extractor` is the extractor version that read it.
CREATE TABLE IF NOT EXISTS blobs (oid TEXT PRIMARY KEY, lang TEXT NOT NULL, extractor INTEGER NOT NULL);
-- What a blob is made of: each construct kind once, at its first line, with how often it occurs.
CREATE TABLE IF NOT EXISTS constructs (
    blob TEXT NOT NULL, kind TEXT NOT NULL, line INTEGER NOT NULL, count INTEGER NOT NULL,
    PRIMARY KEY (blob, kind)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS constructs_kind ON constructs (kind, blob);
CREATE TABLE IF NOT EXISTS symbols (blob TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL, line INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS symbols_blob ON symbols (blob);
CREATE INDEX IF NOT EXISTS symbols_name ON symbols (name);
-- A repository at one commit: its indexable paths and their blobs.
CREATE TABLE IF NOT EXISTS trees (
    id INTEGER PRIMARY KEY, repo INTEGER NOT NULL, rev TEXT NOT NULL, indexed_at INTEGER NOT NULL,
    UNIQUE (repo, rev)
);
CREATE TABLE IF NOT EXISTS entries (
    tree INTEGER NOT NULL, path TEXT NOT NULL, blob TEXT NOT NULL,
    PRIMARY KEY (tree, path)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS entries_blob ON entries (blob);
-- Schema 2. Review comments left on the repository's pull requests, by anyone: what reviewers asked of a file,
-- and whether it was addressed (`addressed`: the thread was resolved; `outdated`: only a later commit changed its
-- lines; `open`).
-- Schema 3. A human's addressed comment is a rule the investigators can ask for (`rules_for`): `human` (not a bot,
-- not one of the reviewing accounts), `missed` (this reviewer reviewed the pull request and raised nothing there),
-- and the fates of the findings that cited it (`accepted`, `rejected`), its credit.
CREATE TABLE IF NOT EXISTS notes (
    id INTEGER PRIMARY KEY, repo INTEGER NOT NULL, pull INTEGER NOT NULL, path TEXT NOT NULL, line INTEGER,
    author TEXT NOT NULL, body TEXT NOT NULL, url TEXT NOT NULL, at TEXT NOT NULL, state TEXT NOT NULL,
    human INTEGER NOT NULL DEFAULT 1, missed INTEGER NOT NULL DEFAULT 0,
    accepted INTEGER NOT NULL DEFAULT 0, rejected INTEGER NOT NULL DEFAULT 0,
    UNIQUE (repo, url)
);
CREATE INDEX IF NOT EXISTS notes_path ON notes (repo, path);
";

/// A review comment, as [`Store::record_notes`] keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub pull: i64,
    pub path: String,
    pub line: Option<i64>,
    pub author: String,
    pub body: String,
    /// The comment's URL: a note is recorded once, its state updated.
    pub url: String,
    /// ISO time the comment was made.
    pub at: String,
    /// `addressed` (resolved), `outdated` (a later commit changed its lines, unresolved) or `open`.
    pub state: String,
    /// Written by a person: not a bot, not a reviewing account. Only a person's addressed comment is a rule.
    pub human: bool,
    /// This reviewer reviewed the pull request and raised nothing at the comment's place: a miss to learn from.
    pub missed: bool,
}

/// The columns schema 3 added to `notes`, for a store of schema 2.
const NOTE_COLUMNS_3: [(&str, &str); 4] = [
    ("human", "INTEGER NOT NULL DEFAULT 1"),
    ("missed", "INTEGER NOT NULL DEFAULT 0"),
    ("accepted", "INTEGER NOT NULL DEFAULT 0"),
    ("rejected", "INTEGER NOT NULL DEFAULT 0"),
];

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self(format!("knowledge store: {error}"))
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating when absent) the store at `path`, private to the user. WAL lets the kernel read while a
    /// review indexes; writers wait for each other instead of failing.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|error| Error(format!("{}: {error}", parent.display())))?;
        }
        let conn = Connection::open(path)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        conn.busy_timeout(Duration::from_secs(60))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), Error> {
        self.conn.execute_batch(SCHEMA)?;
        let found: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |row| {
                row.get(0)
            })
            .optional()?;
        match found.as_deref().map(str::parse::<i64>) {
            None => {
                self.conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(Ok(SCHEMA_VERSION)) => {}
            // Schema 1 had no notes (the table was created above); schema 2's lack the rule columns.
            Some(Ok(1 | 2)) => {
                let mut statement = self.conn.prepare("SELECT name FROM pragma_table_info('notes')")?;
                let have: Vec<String> = statement.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?;
                for (column, definition) in NOTE_COLUMNS_3 {
                    if !have.iter().any(|name| name == column) {
                        self.conn
                            .execute_batch(&format!("ALTER TABLE notes ADD COLUMN {column} {definition}"))?;
                    }
                }
                // Schema 2 counted an outdated thread as addressed: which of its addressed notes were resolved is
                // unknown, so none is a rule until the backfill, run again, reads the recent ones' threads.
                self.conn
                    .execute("UPDATE notes SET state = 'outdated' WHERE state = 'addressed'", [])?;
                self.conn
                    .execute("DELETE FROM meta WHERE key LIKE 'notes_backfilled:%'", [])?;
                self.conn.execute(
                    "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(Ok(version)) if version > SCHEMA_VERSION => {
                return Err(Error(format!(
                    "the knowledge store has schema {version}, newer than this build's {SCHEMA_VERSION}"
                )));
            }
            Some(other) => {
                return Err(Error(format!(
                    "the knowledge store has an unknown schema version ({other:?})"
                )));
            }
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64, Error> {
        let value: String = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |row| {
                row.get(0)
            })?;
        value.parse().map_err(|_| Error(format!("schema version {value:?}")))
    }

    /// The blobs among `oids` that are already indexed by extractor `version`.
    pub fn known_blobs(&self, oids: &[&str], version: i64) -> Result<std::collections::HashSet<String>, Error> {
        let mut known = std::collections::HashSet::new();
        let mut statement = self
            .conn
            .prepare_cached("SELECT 1 FROM blobs WHERE oid = ?1 AND extractor = ?2")?;
        for oid in oids {
            if statement.exists(params![oid, version])? {
                known.insert((*oid).to_string());
            }
        }
        Ok(known)
    }

    /// Records one revision: the newly read blobs, then the tree's path-to-blob table, in one transaction. Returns
    /// the tree's id.
    pub fn record_tree(
        &mut self,
        repo: &str,
        rev: &str,
        entries: &[(String, String)],
        blobs: &[(String, String, Extracted)],
        version: i64,
        indexed_at: i64,
    ) -> Result<i64, Error> {
        let tx = self.conn.transaction()?;
        {
            let mut blob_row = tx.prepare_cached(
                "INSERT INTO blobs (oid, lang, extractor) VALUES (?1, ?2, ?3)
                 ON CONFLICT (oid) DO UPDATE SET lang = excluded.lang, extractor = excluded.extractor",
            )?;
            let mut clear_constructs = tx.prepare_cached("DELETE FROM constructs WHERE blob = ?1")?;
            let mut clear_symbols = tx.prepare_cached("DELETE FROM symbols WHERE blob = ?1")?;
            let mut construct_row =
                tx.prepare_cached("INSERT INTO constructs (blob, kind, line, count) VALUES (?1, ?2, ?3, ?4)")?;
            let mut symbol_row =
                tx.prepare_cached("INSERT INTO symbols (blob, name, kind, line) VALUES (?1, ?2, ?3, ?4)")?;
            for (oid, lang, extracted) in blobs {
                blob_row.execute(params![oid, lang, version])?;
                clear_constructs.execute(params![oid])?;
                clear_symbols.execute(params![oid])?;
                for Construct { kind, line, count } in &extracted.constructs {
                    construct_row.execute(params![oid, kind, line, count])?;
                }
                for Symbol { name, kind, line } in &extracted.symbols {
                    symbol_row.execute(params![oid, name, kind, line])?;
                }
            }
        }
        tx.execute(
            "INSERT INTO repos (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
            params![repo],
        )?;
        let repo_id: i64 = tx.query_row("SELECT id FROM repos WHERE name = ?1", params![repo], |row| row.get(0))?;
        tx.execute(
            "INSERT INTO trees (repo, rev, indexed_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (repo, rev) DO UPDATE SET indexed_at = excluded.indexed_at",
            params![repo_id, rev, indexed_at],
        )?;
        let tree: i64 = tx.query_row(
            "SELECT id FROM trees WHERE repo = ?1 AND rev = ?2",
            params![repo_id, rev],
            |row| row.get(0),
        )?;
        tx.execute("DELETE FROM entries WHERE tree = ?1", params![tree])?;
        {
            let mut entry_row = tx.prepare_cached("INSERT INTO entries (tree, path, blob) VALUES (?1, ?2, ?3)")?;
            for (path, oid) in entries {
                entry_row.execute(params![tree, path, oid])?;
            }
        }
        // Older trees of this repository beyond the limit go, with the blobs no tree still holds.
        tx.execute(
            "DELETE FROM entries WHERE tree IN (
                 SELECT id FROM trees WHERE repo = ?1 ORDER BY indexed_at DESC, id DESC LIMIT -1 OFFSET ?2)",
            params![repo_id, TREES_PER_REPO],
        )?;
        let dropped = tx.execute(
            "DELETE FROM trees WHERE repo = ?1 AND id NOT IN (SELECT DISTINCT tree FROM entries) AND id != ?2",
            params![repo_id, tree],
        )?;
        if dropped > 0 {
            tx.execute_batch(
                "DELETE FROM constructs WHERE blob NOT IN (SELECT blob FROM entries);
                 DELETE FROM symbols WHERE blob NOT IN (SELECT blob FROM entries);
                 DELETE FROM blobs WHERE oid NOT IN (SELECT blob FROM entries);",
            )?;
        }
        tx.commit()?;
        Ok(tree)
    }

    /// Records review comments of `repo`, in one transaction: a new one is added, a known one (by URL) gets its
    /// current state, line and body. Returns how many were new.
    pub fn record_notes(&mut self, repo: &str, notes: &[Note]) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO repos (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
            params![repo],
        )?;
        let repo_id: i64 = tx.query_row("SELECT id FROM repos WHERE name = ?1", params![repo], |row| row.get(0))?;
        let mut added = 0;
        {
            let mut known = tx.prepare_cached("SELECT 1 FROM notes WHERE repo = ?1 AND url = ?2")?;
            // A miss stays one: the pull request's findings do not change once it closed.
            let mut row = tx.prepare_cached(
                "INSERT INTO notes (repo, pull, path, line, author, body, url, at, state, human, missed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (repo, url) DO UPDATE SET
                     path = excluded.path, line = excluded.line, body = excluded.body, state = excluded.state,
                     human = excluded.human, missed = MAX(missed, excluded.missed)",
            )?;
            for note in notes {
                if !known.exists(params![repo_id, note.url])? {
                    added += 1;
                }
                let body: String = note.body.chars().take(NOTE_CHARS).collect();
                row.execute(params![
                    repo_id,
                    note.pull,
                    note.path,
                    note.line,
                    note.author,
                    body,
                    note.url,
                    note.at,
                    note.state,
                    note.human,
                    note.missed
                ])?;
            }
        }
        tx.commit()?;
        Ok(added)
    }

    /// Credit to rules (notes, by id) from the fates of the findings that cited them: `true` accepted. Ids of
    /// another repository are passed over. Returns how many rules were credited.
    pub fn credit_rules(&mut self, repo: &str, credits: &[(i64, bool)]) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        let mut credited = 0;
        {
            let mut row = tx.prepare_cached(
                "UPDATE notes SET accepted = accepted + ?3, rejected = rejected + ?4
                 WHERE id = ?1 AND repo = (SELECT id FROM repos WHERE name = ?2)",
            )?;
            for (id, accepted) in credits {
                let (yes, no) = if *accepted { (1, 0) } else { (0, 1) };
                credited += row.execute(params![id, repo, yes, no])?;
            }
        }
        tx.commit()?;
        Ok(credited)
    }

    /// Marks the comments of `logins` (the reviewing accounts) and of GitHub Apps (`...[bot]`) as not a person's:
    /// the reviewer learns from people, never from its own text. Returns how many notes changed.
    pub fn mark_not_human(&self, repo: &str, logins: &[String]) -> Result<usize, Error> {
        let mut changed = self.conn.execute(
            "UPDATE notes SET human = 0
             WHERE human = 1 AND author LIKE '%[bot]' AND repo = (SELECT id FROM repos WHERE name = ?1)",
            params![repo],
        )?;
        for login in logins {
            changed += self.conn.execute(
                "UPDATE notes SET human = 0
                 WHERE human = 1 AND lower(author) = lower(?2) AND repo = (SELECT id FROM repos WHERE name = ?1)",
                params![repo, login],
            )?;
        }
        Ok(changed)
    }

    /// A rule's credit, for tests and the CLI: (accepted, rejected, missed, human).
    pub fn rule_credit(&self, id: i64) -> Result<Option<(i64, i64, bool, bool)>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT accepted, rejected, missed, human FROM notes WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?)
    }

    /// The id of the note at `url` in `repo`.
    pub fn note_id(&self, repo: &str, url: &str) -> Result<Option<i64>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT n.id FROM notes n JOIN repos r ON r.id = n.repo WHERE r.name = ?1 AND n.url = ?2",
                params![repo, url],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// A value the host keeps beside the graph (`meta`), such as when a repository's notes were backfilled.
    pub fn meta(&self, key: &str) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| row.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), Error> {
        if key == "schema_version" {
            return Err(Error("the schema version is the store's own".into()));
        }
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// How many notes the store holds for `repo`.
    pub fn note_count(&self, repo: &str) -> Result<i64, Error> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM notes n JOIN repos r ON r.id = n.repo WHERE r.name = ?1",
            params![repo],
            |row| row.get(0),
        )?)
    }

    /// The constructs of the file at `path` in `repo` at `rev`, for tests and the CLI: (kind, line, count).
    pub fn constructs_at(&self, repo: &str, rev: &str, path: &str) -> Result<Vec<(String, i64, i64)>, Error> {
        let mut statement = self.conn.prepare(
            "SELECT c.kind, c.line, c.count FROM constructs c
             JOIN entries e ON e.blob = c.blob
             JOIN trees t ON t.id = e.tree
             JOIN repos r ON r.id = t.repo
             WHERE r.name = ?1 AND t.rev = ?2 AND e.path = ?3
             ORDER BY c.line, c.kind",
        )?;
        let rows = statement.query_map(params![repo, rev, path], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// How many trees the store holds for `repo`.
    pub fn tree_count(&self, repo: &str) -> Result<i64, Error> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM trees t JOIN repos r ON r.id = t.repo WHERE r.name = ?1",
            params![repo],
            |row| row.get(0),
        )?)
    }
}
