//! `build` and `grep`: the write and read ends of the store.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;

use crate::extract;
use crate::repo::{self, Lang, SourceFile};

/// Identifies the extractor. Any change to the queries or the stored shape must
/// bump this, because it is what tells an existing store its rows were produced
/// by a different extractor and cannot be trusted.
pub const EXTRACTOR_STAMP: &str = "rs-multilang-2";

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
    let mut pending: Vec<(String, Lang, Vec<i64>, extract::Extracted)> = Vec::new();
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
        let ex = extract::extract(f.lang, &f.text).with_context(|| format!("extract {}", f.rel))?;
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
                by_container
                    .entry((c, s.name.clone()))
                    .or_default()
                    .push(id);
            }
            n_symbols += 1;
        }
        pending.push((f.rel.clone(), f.lang, ids, ex));
    }

    // Containment: every symbol hangs off its innermost enclosing symbol, or off
    // its file when it is top level.
    let mut n_edges = 0usize;
    for (rel, _, ids, ex) in &pending {
        let Some(&fsym) = file_symbol.get(rel) else {
            continue;
        };
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
    let go_module = go_module_path(root);
    for (rel, lang, _, ex) in &pending {
        let Some(&fsym) = file_symbol.get(rel) else {
            continue;
        };
        for spec in &ex.imports {
            let mut targets = resolve_import(rel, *lang, spec, go_module.as_deref(), &file_symbol);
            if targets.is_empty() {
                targets.push(
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
                    },
                );
            }
            for target in targets {
                n_edges += tx.execute(
                    "insert or ignore into edges(repo_id, src_symbol_id, dst_symbol_id, kind)
                     values (?1, ?2, ?3, 'imports')",
                    rusqlite::params![repo_id, fsym, target],
                )?;
            }
        }
    }

    // Pass 2: resolve call intents against the now-complete repo-wide index.
    let mut unresolved = 0usize;
    for (rel, _, ids, ex) in &pending {
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
                // `self` in Python and Go-style receivers behave exactly as `this`
                // does in TypeScript: they name the enclosing type.
                let ty = if recv == "this" || recv == "self" {
                    c.from.and_then(|i| ex.containers.get(i).cloned().flatten())
                } else if let Some(field) = recv
                    .strip_prefix("this.")
                    .or_else(|| recv.strip_prefix("self."))
                {
                    // A field binding is scoped to the class, not the method.
                    let class = c.from.and_then(|i| ex.containers.get(i).cloned().flatten());
                    class.and_then(|cls| {
                        ex.symbols
                            .iter()
                            .position(|s| s.name == cls && s.kind == "class")
                            .and_then(|ci| {
                                // Stored under the bare field name for Python, and
                                // under `this.<field>` for TypeScript.
                                binds
                                    .get(&(Some(ci), format!("this.{field}").as_str()))
                                    .or_else(|| binds.get(&(Some(ci), field)))
                                    .copied()
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
                if let Some(ty) = ty
                    && let Some([only]) = by_container
                        .get(&(ty, c.callee.clone()))
                        .map(|v| v.as_slice())
                {
                    n_edges += tx.execute(
                        "insert or ignore into edges(repo_id, src_symbol_id, dst_symbol_id, kind)
                         values (?1, ?2, ?3, 'calls')",
                        rusqlite::params![repo_id, from_id, *only],
                    )?;
                    continue;
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
    Ok(BuildStats {
        files: files.len(),
        symbols: n_symbols,
        edges: n_edges,
        unresolved,
    })
}

/// Lexically normalize a repo-relative path without consulting the filesystem.
fn normalize_path(path: &Path) -> String {
    let joined = path.to_string_lossy();
    // Lexical normalisation: `a/b/../c` -> `a/c`, without touching the disk.
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
    parts.join("/")
}

/// Resolve one language's import to file nodes in the same repo.
///
/// Go packages may contain multiple files, so this returns every file node in the
/// package. TypeScript and Python module paths resolve to at most one file.
fn resolve_import(
    from_rel: &str,
    lang: Lang,
    spec: &str,
    go_module: Option<&str>,
    files: &HashMap<String, i64>,
) -> Vec<i64> {
    let from_dir = Path::new(from_rel).parent().unwrap_or(Path::new(""));
    let candidates = match lang {
        Lang::TypeScript | Lang::Tsx => {
            if !spec.starts_with('.') {
                return Vec::new();
            }
            let stem = normalize_path(&from_dir.join(spec));
            let base = stem.strip_suffix(".js").unwrap_or(&stem);
            vec![
                format!("{base}.ts"),
                format!("{base}.tsx"),
                stem.clone(),
                format!("{base}/index.ts"),
                format!("{base}/index.tsx"),
            ]
        }
        Lang::Python => {
            let dots = spec.bytes().take_while(|b| *b == b'.').count();
            let module = spec[dots..].replace('.', "/");
            let mut base = from_dir.to_path_buf();
            if dots == 0 {
                base = Path::new("").to_path_buf();
            } else {
                for _ in 1..dots {
                    base.pop();
                }
            }
            let stem = normalize_path(&base.join(module));
            vec![format!("{stem}.py"), format!("{stem}/__init__.py")]
        }
        Lang::Go => {
            let Some(module) = go_module else {
                return Vec::new();
            };
            let package = if spec == module {
                ""
            } else if let Some(rest) = spec.strip_prefix(module).and_then(|s| s.strip_prefix('/')) {
                rest
            } else {
                return Vec::new();
            };
            let mut ids: Vec<i64> = files
                .iter()
                .filter(|(path, _)| {
                    path.ends_with(".go")
                        && Path::new(path.as_str()).parent().unwrap_or(Path::new(""))
                            == Path::new(package)
                })
                .map(|(_, id)| *id)
                .collect();
            ids.sort_unstable();
            return ids;
        }
    };
    for candidate in candidates {
        if let Some(&id) = files.get(&candidate) {
            return vec![id];
        }
    }
    Vec::new()
}

fn go_module_path(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("go.mod")).ok()?;
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("module ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
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

struct SpanRow {
    id: i64,
    name: String,
    kind: String,
    start: i64,
    end: i64,
    in_edges: i64,
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
    let mut spans: HashMap<String, Vec<SpanRow>> = HashMap::new();
    let mut stmt = db.prepare(
        "select f.path, s.id, s.name, s.kind, s.start_line, s.end_line,
                (select count(*) from edges e where e.dst_symbol_id = s.id)
           from symbols s join files f on f.id = s.file_id
          where s.repo_id = ?1 and s.kind not in ('file','module')",
    )?;
    let mut rows = stmt.query([repo_id])?;
    while let Some(r) = rows.next()? {
        let path: String = r.get(0)?;
        spans.entry(path).or_default().push(SpanRow {
            id: r.get(1)?,
            name: r.get(2)?,
            kind: r.get(3)?,
            start: r.get(4)?,
            end: r.get(5)?,
            in_edges: r.get(6)?,
        });
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
        let mut seen: HashMap<Option<i64>, usize> = HashMap::new();
        for (i, line_text) in text.lines().enumerate() {
            if !line_text.contains(needle) {
                continue;
            }
            let line = i as i64 + 1;
            // Innermost enclosing definition: narrowest span containing the line.
            let owner = spans.get(path).and_then(|v| {
                v.iter()
                    .filter(|span| span.start <= line && line <= span.end)
                    .min_by_key(|span| span.end - span.start)
            });
            let key = owner.map(|span| span.id);
            let idx = match seen.get(&key) {
                Some(&i) => i,
                None => {
                    groups.push(Group {
                        path: path.clone(),
                        symbol: owner.map(|span| span.name.clone()),
                        kind: owner
                            .map(|span| span.kind.clone())
                            .unwrap_or_else(|| "file".into()),
                        start_line: owner.map(|span| span.start).unwrap_or(0),
                        end_line: owner.map(|span| span.end).unwrap_or(0),
                        in_edges: owner.map(|span| span.in_edges).unwrap_or(0),
                        hits: Vec::new(),
                    });
                    seen.insert(key, groups.len() - 1);
                    groups.len() - 1
                }
            };
            groups[idx].hits.push(Occurrence {
                line,
                text: line_text.trim().to_string(),
            });
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

    Ok(GrepResult {
        groups,
        total_hits,
        files_searched: paths.len(),
        unreadable,
    })
}

pub fn repo_id_of(db: &Connection, root: &Path) -> Result<Option<i64>> {
    let mut stmt = db.prepare("select id from repos where root=?1 and extractor_stamp=?2")?;
    let mut rows = stmt.query(rusqlite::params![root.to_string_lossy(), EXTRACTOR_STAMP])?;
    Ok(match rows.next()? {
        Some(r) => Some(r.get(0)?),
        None => None,
    })
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

pub struct Seed {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
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
) -> Result<(Vec<Seed>, Vec<Reached>)> {
    let mut seeds_stmt = db.prepare(
        "select s.id, s.name, s.kind, f.path, s.start_line, s.end_line
           from symbols s join files f on f.id = s.file_id
          where s.repo_id = ?1 and s.name = ?2
          order by f.path, s.start_line",
    )?;
    let seeds: Vec<Seed> = seeds_stmt
        .query_map(rusqlite::params![repo_id, name], |r| {
            Ok(Seed {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                path: r.get(3)?,
                start_line: r.get(4)?,
                end_line: r.get(5)?,
            })
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
          where e.repo_id = ?1 and e.{from_col} = ?2 and e.kind != 'contains'
          order by f.path, s.start_line"
    );
    let mut step = db.prepare(&sql)?;

    let mut seen: std::collections::HashSet<i64> = seeds.iter().map(|s| s.id).collect();
    let mut frontier: Vec<i64> = seeds.iter().map(|s| s.id).collect();
    if max_depth > 1 {
        let mut file_members = db.prepare(
            "select member.id
               from symbols file
               join symbols member on member.file_id = file.file_id
              where file.repo_id=?1 and file.id=?2
                and file.kind='file' and member.kind not in ('file','module')",
        )?;
        for seed in &seeds {
            if seed.kind != "file" {
                continue;
            }
            let ids = file_members.query_map(rusqlite::params![repo_id, seed.id], |r| r.get(0))?;
            for id in ids {
                let id = id?;
                if seen.insert(id) {
                    frontier.push(id);
                }
            }
        }
    }
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
    let mut file_stmt = db.prepare("select path from files where repo_id=?1 order by path")?;
    let paths = file_stmt.query_map([repo_id], |r| r.get::<_, String>(0))?;
    for path in paths {
        let path = path?;
        per_dir.entry(bucket(&path)).or_default().0.insert(path);
    }
    for h in &all {
        let e = per_dir.entry(bucket(&h.path)).or_default();
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

    let hotspots = all
        .into_iter()
        .filter(|h| h.in_degree > 0)
        .take(top)
        .collect();
    Ok(RepoMap {
        files,
        symbols,
        edges,
        dirs,
        hotspots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

        assert!(
            r.total_hits >= 3,
            "definition + import + call, got {}",
            r.total_hits
        );
        let paths: Vec<_> = r.groups.iter().map(|g| g.path.as_str()).collect();
        assert!(
            paths.contains(&"src/b.ts"),
            "must reach the calling file: {paths:?}"
        );
        assert!(
            r.groups
                .iter()
                .any(|g| g.symbol.as_deref() == Some("runInit")),
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
        assert_eq!(
            r.groups[0].symbol, None,
            "file-level hit, not attributed to f()"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_keeps_same_named_symbols_as_separate_groups() {
        let (db, root) = fixture(&[(
            "src/a.ts",
            "class A {\n  run() {\n    needle();\n  }\n}\nclass B {\n  run() {\n    needle();\n  }\n}\n",
        )]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let r = grep(&db, id, &root, "needle").unwrap();
        let runs: Vec<_> = r
            .groups
            .iter()
            .filter(|g| g.symbol.as_deref() == Some("run"))
            .collect();
        assert_eq!(
            runs.len(),
            2,
            "same name, distinct symbol ids/spans: {}",
            r.groups.len()
        );
        assert_ne!(runs[0].start_line, runs[1].start_line);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_extractor_rows_force_a_rebuild() {
        let (db, root) = fixture(&[("src/a.ts", "function f() {}\n")]);
        db.execute(
            "update repos set extractor_stamp='obsolete-extractor' where root=?1",
            [root.to_string_lossy()],
        )
        .unwrap();
        assert!(repo_id_of(&db, &root).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn python_imports_resolve_to_internal_file_nodes() {
        let (db, root) = fixture(&[
            (
                "pkg/a.py",
                "from pkg.b import helper\ndef run():\n    helper()\n",
            ),
            ("pkg/b.py", "def helper():\n    pass\n"),
        ]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let imports: i64 = db
            .query_row(
                "select count(*) from edges e
                   join symbols src on src.id=e.src_symbol_id
                   join symbols dst on dst.id=e.dst_symbol_id
                  where e.repo_id=?1 and e.kind='imports'
                    and src.name='pkg/a.py' and dst.name='pkg/b.py'",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(imports, 1);
        let (_, reached) = callers(&db, id, "pkg/b.py", false, 1).unwrap();
        assert!(
            reached
                .iter()
                .any(|hit| hit.name == "pkg/a.py" && hit.edge == "imports")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn go_package_imports_and_receiver_calls_resolve() {
        let (db, root) = fixture(&[
            ("go.mod", "module example.com/project\n"),
            (
                "main.go",
                "package main\nimport _ \"example.com/project/pkg\"\nfunc main() {}\n",
            ),
            (
                "pkg/worker.go",
                "package pkg\ntype Worker struct{}\nfunc (w *Worker) Step() {}\nfunc (w *Worker) Run() { w.Step() }\n",
            ),
        ]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let imports: i64 = db
            .query_row(
                "select count(*) from edges e
                   join symbols src on src.id=e.src_symbol_id
                   join symbols dst on dst.id=e.dst_symbol_id
                  where e.repo_id=?1 and e.kind='imports'
                    and src.name='main.go' and dst.name='pkg/worker.go'",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(imports, 1);
        let calls: i64 = db
            .query_row(
                "select count(*) from edges e
                   join symbols src on src.id=e.src_symbol_id
                   join symbols dst on dst.id=e.dst_symbol_id
                  where e.repo_id=?1 and e.kind='calls'
                    and src.name='Run' and dst.name='Step'",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(calls, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn callers_aggregates_symbols_defined_by_a_file_seed() {
        let (db, root) = fixture(&[
            ("src/a.ts", "export function target() {}\n"),
            ("src/b.ts", "function run() { target(); }\n"),
        ]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let (_, reached) = callers(&db, id, "src/a.ts", false, 2).unwrap();
        assert!(
            reached
                .iter()
                .any(|hit| hit.name == "run" && hit.edge == "calls")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repo_map_counts_files_without_definitions() {
        let (db, root) = fixture(&[("config/settings.py", "VALUE = 1\n")]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let map = repo_map(&db, id, 12).unwrap();
        let config = map.dirs.iter().find(|d| d.dir == "config/").unwrap();
        assert_eq!(config.files, 1);
        assert_eq!(config.symbols, 0);
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
        assert_eq!(
            before, after,
            "a second build must not double the symbol rows"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
