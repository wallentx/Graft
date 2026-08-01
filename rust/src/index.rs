//! `build` and `grep`: the write and read ends of the store.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::extract;
use crate::repo::{self, SourceFile};

/// Identifies the extractor. Any change to the queries or the stored shape must
/// bump this, because it is what tells an existing store its rows were produced
/// by a different extractor and cannot be trusted.
pub const EXTRACTOR_STAMP: &str = "rs-ts-1";

pub struct BuildStats {
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
    pub unresolved: usize,
}

/// Index `root` into `db`, replacing whatever was there for that repo.
///
/// Whole-repo replace rather than per-file diffing: correctness first. The
/// `files.hash` column is populated so an incremental path can be added without a
/// schema change, but nothing reads it yet — a partially-correct incremental
/// build is worse than a slower complete one.
pub fn build(db: &mut Connection, root: &Path) -> Result<BuildStats> {
    let files = repo::walk(root).context("walk the work tree")?;
    let common = repo::git_common_dir(root);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let tx = db.transaction().context("begin build transaction")?;

    // ON CONFLICT rather than delete-then-insert: the repo row keeps its id, so a
    // rebuild does not orphan anything that might later reference it.
    tx.execute(
        "insert into repos(root, git_common_dir, indexed_at, extractor_stamp)
         values (?1, ?2, ?3, ?4)
         on conflict(root) do update set
           git_common_dir=excluded.git_common_dir,
           indexed_at=excluded.indexed_at,
           extractor_stamp=excluded.extractor_stamp",
        rusqlite::params![root.to_string_lossy(), common, now, EXTRACTOR_STAMP],
    )?;
    let repo_id: i64 = tx.query_row(
        "select id from repos where root=?1",
        [root.to_string_lossy()],
        |r| r.get(0),
    )?;

    // Cascades through symbols and edges, and the FTS triggers retract with them.
    tx.execute("delete from files where repo_id=?1", [repo_id])?;

    // Pass 1: symbols. Call targets cannot be resolved until every file's symbols
    // exist, because a callee is usually defined somewhere else.
    let mut pending: Vec<(Vec<i64>, Vec<extract::CallIntent>)> = Vec::new();
    let mut by_name: HashMap<String, Vec<i64>> = HashMap::new();
    let mut n_symbols = 0usize;

    for f in &files {
        let file_id = insert_file(&tx, repo_id, f)?;
        let ex = match extract::extract(f.lang, &f.text) {
            Ok(e) => e,
            // One unparseable file must not abort indexing the other 125.
            Err(_) => continue,
        };
        let mut ids = Vec::with_capacity(ex.symbols.len());
        for s in &ex.symbols {
            tx.execute(
                "insert into symbols(repo_id, file_id, name, kind, start_line, end_line, signature)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![repo_id, file_id, s.name, s.kind, s.start_line, s.end_line, s.signature],
            )?;
            let id = tx.last_insert_rowid();
            ids.push(id);
            by_name.entry(s.name.clone()).or_default().push(id);
            n_symbols += 1;
        }
        pending.push((ids, ex.calls));
    }

    // Pass 2: resolve call intents against the now-complete repo-wide index.
    let mut n_edges = 0usize;
    let mut unresolved = 0usize;
    for (ids, calls) in &pending {
        for c in calls {
            let Some(&from_id) = ids.get(c.from) else { continue };
            match extract::resolve(&by_name, from_id, &c.callee) {
                Some(to_id) => {
                    tx.execute(
                        "insert into edges(repo_id, src_symbol_id, dst_symbol_id, kind)
                         values (?1, ?2, ?3, 'calls')",
                        rusqlite::params![repo_id, from_id, to_id],
                    )?;
                    n_edges += 1;
                }
                // Calls into node_modules, globals, and ambiguous names all land
                // here. Counted rather than dropped silently so the number is
                // visible if it ever looks wrong.
                None => unresolved += 1,
            }
        }
    }

    tx.commit().context("commit build")?;
    Ok(BuildStats { files: files.len(), symbols: n_symbols, edges: n_edges, unresolved })
}

fn insert_file(tx: &rusqlite::Transaction, repo_id: i64, f: &SourceFile) -> Result<i64> {
    tx.execute(
        "insert into files(repo_id, path, mtime, size, hash) values (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![repo_id, f.rel, f.mtime, f.size, f.hash],
    )?;
    Ok(tx.last_insert_rowid())
}

/// One occurrence of the literal, with the line it sits on.
pub struct Occurrence {
    pub line: i64,
    pub text: String,
}

/// Occurrences grouped under the symbol that encloses them.
pub struct Group {
    pub path: String,
    /// None for a match outside any definition — imports, top-level config.
    pub symbol: Option<String>,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub in_edges: i64,
    pub hits: Vec<Occurrence>,
}

pub struct GrepResult {
    pub groups: Vec<Group>,
    pub total_hits: usize,
    pub files_searched: usize,
    /// Indexed files that could not be re-read. Surfaced rather than swallowed:
    /// a silent skip makes an incomplete search look exhaustive.
    pub unreadable: usize,
}

/// Every occurrence of a literal, grouped by enclosing symbol.
///
/// Searches file *contents*, not stored symbol metadata. `grep` answers "where is
/// this used", so the definition is the least interesting of its hits — matching
/// only names would return one line and hide the fourteen call sites that
/// actually matter for a change.
///
/// Files are re-read from the work tree rather than duplicated into the store.
/// The store holds structure; the source is already on disk, and copying it would
/// double the database for data that goes stale the moment anyone edits.
pub fn grep(db: &Connection, repo_id: i64, root: &Path, needle: &str) -> Result<GrepResult> {
    // Symbol spans per file, so a matching line can be attributed to a definition.
    let mut spans: HashMap<String, Vec<(String, String, i64, i64, i64)>> = HashMap::new();
    let mut stmt = db.prepare(
        "select f.path, s.name, s.kind, s.start_line, s.end_line,
                (select count(*) from edges e where e.dst_symbol_id = s.id)
           from symbols s join files f on f.id = s.file_id
          where s.repo_id = ?1",
    )?;
    let mut rows = stmt.query([repo_id])?;
    while let Some(r) = rows.next()? {
        let path: String = r.get(0)?;
        spans.entry(path).or_default().push((
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ));
    }

    let mut files_stmt = db.prepare("select path from files where repo_id=?1 order by path")?;
    let paths: Vec<String> = files_stmt
        .query_map([repo_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;

    let mut groups: Vec<Group> = Vec::new();
    let mut total_hits = 0usize;
    let mut unreadable = 0usize;

    for path in &paths {
        let Ok(text) = std::fs::read_to_string(root.join(path)) else {
            unreadable += 1;
            continue;
        };
        // (symbol key) -> group index, so occurrences accumulate per definition.
        let mut seen: HashMap<Option<String>, usize> = HashMap::new();
        for (i, line_text) in text.lines().enumerate() {
            if !line_text.contains(needle) {
                continue;
            }
            let line = i as i64 + 1;
            // Innermost enclosing definition: narrowest span containing the line.
            let owner = spans.get(path).and_then(|v| {
                v.iter()
                    .filter(|(_, _, s, e, _)| *s <= line && line <= *e)
                    .min_by_key(|(_, _, s, e, _)| e - s)
            });
            let key = owner.map(|(n, ..)| n.clone());
            let idx = match seen.get(&key) {
                Some(&i) => i,
                None => {
                    groups.push(Group {
                        path: path.clone(),
                        symbol: key.clone(),
                        kind: owner.map(|(_, k, ..)| k.clone()).unwrap_or_else(|| "file".into()),
                        start_line: owner.map(|(_, _, s, ..)| *s).unwrap_or(0),
                        end_line: owner.map(|(_, _, _, e, _)| *e).unwrap_or(0),
                        in_edges: owner.map(|(_, _, _, _, n)| *n).unwrap_or(0),
                        hits: Vec::new(),
                    });
                    seen.insert(key, groups.len() - 1);
                    groups.len() - 1
                }
            };
            groups[idx].hits.push(Occurrence { line, text: line_text.trim().to_string() });
            total_hits += 1;
        }
    }

    // Most-referenced definitions first: the symbol other code depends on is the
    // one a reader is usually looking for.
    groups.sort_by(|a, b| {
        b.in_edges
            .cmp(&a.in_edges)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });

    Ok(GrepResult { groups, total_hits, files_searched: paths.len(), unreadable })
}

pub fn repo_id_of(db: &Connection, root: &Path) -> Result<Option<i64>> {
    let mut stmt = db.prepare("select id from repos where root=?1")?;
    let mut rows = stmt.query([root.to_string_lossy()])?;
    Ok(match rows.next()? {
        Some(r) => Some(r.get(0)?),
        None => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway repo on disk, index it, and hand back (store, root).
    fn fixture(files: &[(&str, &str)]) -> (Connection, PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "graft-idx-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, body) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let mut conn = crate::db::open(&root.join("store.db")).unwrap();
        crate::index::build(&mut conn, &root).unwrap();
        (conn, root)
    }

    #[test]
    fn grep_finds_call_sites_not_just_the_definition() {
        // The whole point of grep: a definition plus every place that uses it.
        // Matching stored symbol names alone returns 1 hit and hides the callers.
        let (db, root) = fixture(&[
            ("src/a.ts", "export function serverEntry() { return 1; }\n"),
            (
                "src/b.ts",
                "import { serverEntry } from './a';\nfunction runInit() { return serverEntry(); }\n",
            ),
        ]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let r = grep(&db, id, &root, "serverEntry").unwrap();

        assert!(r.total_hits >= 3, "definition + import + call, got {}", r.total_hits);
        let paths: Vec<_> = r.groups.iter().map(|g| g.path.as_str()).collect();
        assert!(paths.contains(&"src/b.ts"), "must reach the calling file: {paths:?}");
        assert!(
            r.groups.iter().any(|g| g.symbol.as_deref() == Some("runInit")),
            "call site must be grouped under its enclosing symbol"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_match_outside_any_definition_is_still_reported() {
        // Top-level imports sit in no symbol's span. Dropping them would make an
        // exhaustive search quietly non-exhaustive.
        let (db, root) = fixture(&[("src/a.ts", "import { x } from './b';\nfunction f() {}\n")]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let r = grep(&db, id, &root, "import").unwrap();
        assert_eq!(r.total_hits, 1);
        assert_eq!(r.groups[0].symbol, None, "file-level hit, not attributed to f()");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rebuilding_replaces_rather_than_duplicates() {
        let (mut db, root) = fixture(&[("src/a.ts", "export function only() {}\n")]);
        let before: i64 = db
            .query_row("select count(*) from symbols", [], |r| r.get(0))
            .unwrap();
        build(&mut db, &root).unwrap();
        let after: i64 = db
            .query_row("select count(*) from symbols", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "a second build must not double the symbol rows");
        let _ = std::fs::remove_dir_all(&root);
    }
}
