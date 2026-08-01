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
    let mut pending: Vec<(String, Vec<i64>, extract::Extracted)> = Vec::new();
    let mut by_name: HashMap<String, Vec<i64>> = HashMap::new();
    // (class, method) -> ids. What a receiver-typed call resolves against.
    let mut by_container: HashMap<(String, String), Vec<i64>> = HashMap::new();
    let mut n_symbols = 0usize;

    // A file is a node, like every other symbol. `contains` hangs off it, `map`
    // groups by it, and imports connect files to files. Modelling it as a row
    // rather than a special case keeps every traversal uniform.
    let mut file_symbol: HashMap<String, i64> = HashMap::new();
    let mut file_rows: HashMap<String, i64> = HashMap::new();

    for f in &files {
        let file_id = insert_file(&tx, repo_id, f)?;
        file_rows.insert(f.rel.clone(), file_id);
        // Real extent, so a file node reports the span a reader expects rather
        // than a degenerate L1-L1.
        let lines = f.text.lines().count().max(1) as i64;
        tx.execute(
            "insert into symbols(repo_id, file_id, name, kind, start_line, end_line, signature)
             values (?1, ?2, ?3, 'file', 1, ?4, ?3)",
            rusqlite::params![repo_id, file_id, f.rel, lines],
        )?;
        let fsym = tx.last_insert_rowid();
        file_symbol.insert(f.rel.clone(), fsym);
        n_symbols += 1;
        let ex = match extract::extract(f.lang, &f.text) {
            Ok(e) => e,
            // One unparseable file must not abort indexing the other 125.
            Err(_) => continue,
        };
        let mut ids = Vec::with_capacity(ex.symbols.len());
        for (i, s) in ex.symbols.iter().enumerate() {
            let container = ex.containers.get(i).cloned().flatten();
            tx.execute(
                "insert into symbols(repo_id, file_id, name, kind, start_line, end_line, signature, container)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![repo_id, file_id, s.name, s.kind, s.start_line, s.end_line, s.signature, container],
            )?;
            let id = tx.last_insert_rowid();
            ids.push(id);
            by_name.entry(s.name.clone()).or_default().push(id);
            if let Some(c) = container {
                by_container.entry((c, s.name.clone())).or_default().push(id);
            }
            n_symbols += 1;
        }
        pending.push((f.rel.clone(), ids, ex));
    }

    // Containment: every symbol hangs off its innermost enclosing symbol, or off
    // its file when it is top level.
    let mut n_edges = 0usize;
    for (rel, ids, ex) in &pending {
        let Some(&fsym) = file_symbol.get(rel) else { continue };
        for (i, &id) in ids.iter().enumerate() {
            let parent = match ex.parents.get(i).copied().flatten() {
                Some(p) => ids.get(p).copied().unwrap_or(fsym),
                None => fsym,
            };
            n_edges += tx.execute(
                "insert or ignore into edges(repo_id, src_symbol_id, dst_symbol_id, kind)
                 values (?1, ?2, ?3, 'contains')",
                rusqlite::params![repo_id, parent, id],
            )?;
        }
    }

    // Imports: file -> file when the specifier resolves inside the repo, else
    // file -> a module node standing in for the external package.
    let mut external: HashMap<String, i64> = HashMap::new();
    for (rel, _, ex) in &pending {
        let Some(&fsym) = file_symbol.get(rel) else { continue };
        for spec in &ex.imports {
            let target = match resolve_import(rel, spec, &file_symbol) {
                Some(id) => id,
                None => {
                    // One node per external package, created on first sight.
                    if let Some(&id) = external.get(spec) {
                        id
                    } else {
                        let anchor = *file_rows.get(rel).unwrap();
                        tx.execute(
                            "insert into symbols(repo_id, file_id, name, kind, start_line, end_line, signature)
                             values (?1, ?2, ?3, 'module', 0, 0, ?3)",
                            rusqlite::params![repo_id, anchor, spec],
                        )?;
                        let id = tx.last_insert_rowid();
                        external.insert(spec.clone(), id);
                        n_symbols += 1;
                        id
                    }
                }
            };
            n_edges += tx.execute(
                "insert or ignore into edges(repo_id, src_symbol_id, dst_symbol_id, kind)
                 values (?1, ?2, ?3, 'imports')",
                rusqlite::params![repo_id, fsym, target],
            )?;
        }
    }

    // Pass 2: resolve call intents against the now-complete repo-wide index.
    let mut unresolved = 0usize;
    for (rel, ids, ex) in &pending {
        // Per-file binding lookup: (scope symbol index or None, variable) -> type.
        let mut binds: HashMap<(Option<usize>, &str), &str> = HashMap::new();
        for b in &ex.bindings {
            binds.insert((b.owner, b.name.as_str()), b.ty.as_str());
        }

        for c in &ex.calls {
            // A top-level call is the file's own dependency.
            let from_id = match c.from {
                Some(i) => match ids.get(i) {
                    Some(&id) => id,
                    None => continue,
                },
                None => match file_symbol.get(rel) {
                    Some(&id) => id,
                    None => continue,
                },
            };

            // Receiver-typed resolution first: it names one class's method, where
            // the bare name would match every same-named method in the repo and be
            // dropped as ambiguous.
            if let Some(recv) = &c.receiver {
                let ty = if recv == "this" {
                    c.from.and_then(|i| ex.containers.get(i).cloned().flatten())
                } else if let Some(field) = recv.strip_prefix("this.") {
                    // A field binding is scoped to the class, not the method.
                    let class = c.from.and_then(|i| ex.containers.get(i).cloned().flatten());
                    class.and_then(|cls| {
                        ex.symbols
                            .iter()
                            .position(|s| s.name == cls && s.kind == "class")
                            .and_then(|ci| {
                                binds.get(&(Some(ci), format!("this.{field}").as_str())).copied()
                            })
                            .map(str::to_string)
                    })
                } else {
                    // Innermost first: the enclosing definition, then module scope.
                    // Deeper lexical nesting is not walked — a rarer shape, and
                    // guessing past the first miss risks a wrong edge.
                    binds
                        .get(&(c.from, recv.as_str()))
                        .or_else(|| binds.get(&(None, recv.as_str())))
                        .copied()
                        .map(str::to_string)
                };
                if let Some(ty) = ty {
                    if let Some([only]) = by_container.get(&(ty, c.callee.clone())).map(|v| v.as_slice()) {
                        n_edges += tx.execute(
                            "insert or ignore into edges(repo_id, src_symbol_id, dst_symbol_id, kind)
                             values (?1, ?2, ?3, 'calls')",
                            rusqlite::params![repo_id, from_id, *only],
                        )?;
                        continue;
                    }
                }
            }

            // A member call whose receiver type is unknown resolves to nothing.
            // Falling through to bare-name matching attributes every `arr.push()`
            // in the repo to a local function that happens to be named `push` —
            // which is how `push` came out as the top hub with 82 in-edges.
            if c.receiver.is_some() {
                unresolved += 1;
                continue;
            }

            match extract::resolve(&by_name, from_id, &c.callee) {
                Some(to_id) => {
                    // OR IGNORE: repeated calls between the same pair collapse to
                    // the one edge the unique index permits.
                    n_edges += tx.execute(
                        "insert or ignore into edges(repo_id, src_symbol_id, dst_symbol_id, kind)
                         values (?1, ?2, ?3, 'calls')",
                        rusqlite::params![repo_id, from_id, to_id],
                    )?;
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

/// Resolve a relative import specifier to a file symbol in the same repo.
///
/// ESM TypeScript imports `./stats.js` for what is on disk as `./stats.ts`, so a
/// literal path lookup finds nothing; the extension has to be re-mapped. Bare
/// specifiers (`node:fs`, `commander`) are external by definition.
fn resolve_import(from_rel: &str, spec: &str, files: &HashMap<String, i64>) -> Option<i64> {
    if !spec.starts_with('.') {
        return None;
    }
    let base = Path::new(from_rel).parent().unwrap_or(Path::new(""));
    let joined = base.join(spec);
    // Lexical normalisation: `a/b/../c` -> `a/c`, without touching the disk.
    let joined = joined.to_string_lossy().into_owned();
    let mut parts: Vec<&str> = Vec::new();
    for c in joined.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let stem = parts.join("/");
    let base = stem.strip_suffix(".js").unwrap_or(&stem);
    for cand in [
        format!("{base}.ts"),
        format!("{base}.tsx"),
        stem.clone(),
        format!("{base}/index.ts"),
        format!("{base}/index.tsx"),
    ] {
        if let Some(&id) = files.get(&cand) {
            return Some(id);
        }
    }
    None
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
          where s.repo_id = ?1 and s.kind not in ('file','module')",
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

/// One step away from the seed, with the distance that reached it.
pub struct Reached {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub edge: String,
    pub depth: usize,
}

/// Breadth-first traversal of the edge set from every symbol named `name`.
///
/// `Out` follows src->dst (what this calls); the default follows dst->src (what
/// calls this). Breadth-first and visited-guarded, so a cycle terminates and each
/// symbol is reported at its shortest distance rather than once per path.
pub fn callers(
    db: &Connection,
    repo_id: i64,
    name: &str,
    out: bool,
    max_depth: usize,
) -> Result<(Vec<(i64, String, String, String, i64, i64)>, Vec<Reached>)> {
    let mut seeds_stmt = db.prepare(
        "select s.id, s.name, s.kind, f.path, s.start_line, s.end_line
           from symbols s join files f on f.id = s.file_id
          where s.repo_id = ?1 and s.name = ?2
          order by f.path, s.start_line",
    )?;
    let seeds: Vec<(i64, String, String, String, i64, i64)> = seeds_stmt
        .query_map(rusqlite::params![repo_id, name], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .collect::<Result<_, _>>()?;

    let (from_col, to_col) = if out {
        ("src_symbol_id", "dst_symbol_id")
    } else {
        ("dst_symbol_id", "src_symbol_id")
    };
    let sql = format!(
        "select s.id, s.name, s.kind, f.path, s.start_line, s.end_line, e.kind
           from edges e
           join symbols s on s.id = e.{to_col}
           join files f on f.id = s.file_id
          where e.repo_id = ?1 and e.{from_col} = ?2 and e.kind = 'calls'
          order by f.path, s.start_line"
    );
    let mut step = db.prepare(&sql)?;

    let mut seen: std::collections::HashSet<i64> = seeds.iter().map(|s| s.0).collect();
    let mut frontier: Vec<i64> = seeds.iter().map(|s| s.0).collect();
    let mut reached = Vec::new();

    for depth in 1..=max_depth {
        let mut next = Vec::new();
        for id in &frontier {
            let rows = step.query_map(rusqlite::params![repo_id, id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    Reached {
                        name: r.get(1)?,
                        kind: r.get(2)?,
                        path: r.get(3)?,
                        start_line: r.get(4)?,
                        end_line: r.get(5)?,
                        edge: r.get(6)?,
                        depth,
                    },
                ))
            })?;
            for row in rows {
                let (id, hit) = row?;
                // First arrival wins: BFS means that is the shortest distance.
                if seen.insert(id) {
                    next.push(id);
                    reached.push(hit);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok((seeds, reached))
}

pub struct SkelRow {
    pub name: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub signature: String,
    pub container: Option<String>,
}

/// Every signature in one file, ordered as they appear.
///
/// Excludes the synthetic file node itself: the caller already named the file, and
/// listing it as a member of itself is noise.
pub fn skeleton(db: &Connection, repo_id: i64, rel: &str) -> Result<Vec<SkelRow>> {
    let mut stmt = db.prepare(
        "select s.name, s.kind, s.start_line, s.end_line, coalesce(s.signature,''), s.container
           from symbols s join files f on f.id = s.file_id
          where s.repo_id = ?1 and f.path = ?2 and s.kind not in ('file','module')
          order by s.start_line, s.end_line desc",
    )?;
    Ok(stmt
        .query_map(rusqlite::params![repo_id, rel], |r| {
            Ok(SkelRow {
                name: r.get(0)?,
                kind: r.get(1)?,
                start_line: r.get(2)?,
                end_line: r.get(3)?,
                signature: r.get(4)?,
                container: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

pub struct Hub {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub in_degree: i64,
}

pub struct DirEntry {
    pub dir: String,
    pub files: i64,
    pub symbols: i64,
    pub hubs: Vec<Hub>,
}

pub struct RepoMap {
    pub files: i64,
    pub symbols: i64,
    pub edges: i64,
    pub dirs: Vec<DirEntry>,
    pub hotspots: Vec<Hub>,
}

/// In-degree over `calls` edges only. Containment would rank every symbol at
/// exactly 1 and imports would rank files, neither of which says "many things
/// depend on this" — which is the question a hub answers.
const HUB_SQL: &str = "select s.name, s.kind, f.path, s.start_line, s.end_line,
        (select count(*) from edges e
          where e.dst_symbol_id = s.id and e.kind = 'calls') d
   from symbols s join files f on f.id = s.file_id
  where s.repo_id = ?1 and s.kind not in ('file','module')";

fn read_hubs(stmt: &mut rusqlite::Statement, repo_id: i64) -> Result<Vec<Hub>> {
    Ok(stmt
        .query_map([repo_id], |r| {
            Ok(Hub {
                name: r.get(0)?,
                kind: r.get(1)?,
                path: r.get(2)?,
                start_line: r.get(3)?,
                end_line: r.get(4)?,
                in_degree: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

/// Orientation for an unfamiliar repo: directory clusters, their hubs, and the
/// most-depended-on symbols overall.
pub fn repo_map(db: &Connection, repo_id: i64, top: usize) -> Result<RepoMap> {
    let (files, symbols, edges): (i64, i64, i64) = db.query_row(
        "select (select count(*) from files where repo_id=?1),
                (select count(*) from symbols where repo_id=?1 and kind not in ('file','module')),
                (select count(*) from edges where repo_id=?1)",
        [repo_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;

    let mut stmt = db.prepare(&format!("{HUB_SQL} order by d desc, s.name"))?;
    let all = read_hubs(&mut stmt, repo_id)?;

    // Top-level directory, or "." for a file at the root.
    let bucket = |p: &str| -> String {
        match p.split_once('/') {
            Some((head, _)) => format!("{head}/"),
            None => ".".to_string(),
        }
    };

    let mut per_dir: HashMap<String, (std::collections::HashSet<String>, i64, Vec<&Hub>)> =
        HashMap::new();
    for h in &all {
        let e = per_dir.entry(bucket(&h.path)).or_default();
        e.0.insert(h.path.clone());
        e.1 += 1;
        if h.in_degree > 0 && e.2.len() < 3 {
            e.2.push(h);
        }
    }

    let mut dirs: Vec<DirEntry> = per_dir
        .into_iter()
        .map(|(dir, (fs, n, hubs))| DirEntry {
            dir,
            files: fs.len() as i64,
            symbols: n,
            hubs: hubs
                .into_iter()
                .map(|h| Hub {
                    name: h.name.clone(),
                    kind: h.kind.clone(),
                    path: h.path.clone(),
                    start_line: h.start_line,
                    end_line: h.end_line,
                    in_degree: h.in_degree,
                })
                .collect(),
        })
        .collect();
    dirs.sort_by(|a, b| b.symbols.cmp(&a.symbols).then_with(|| a.dir.cmp(&b.dir)));

    let hotspots = all.into_iter().filter(|h| h.in_degree > 0).take(top).collect();
    Ok(RepoMap { files, symbols, edges, dirs, hotspots })
}
