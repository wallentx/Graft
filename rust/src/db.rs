//! The store: one SQLite database, all repos, outside every working tree.
//!
//! Location is `$XDG_DATA_HOME/graft/graft.db`, falling back to
//! `~/.local/share/graft/graft.db`. Nothing is ever written into an indexed
//! repository — a repo is identified by its path, not by a marker file inside it.
//!
//! Schema notes that are load-bearing rather than incidental:
//!
//! * `repos.root` is the realpath of the git toplevel and is UNIQUE. It is the
//!   key the MCP server looks up on startup to decide between answering and
//!   reporting "not indexed".
//! * `repos.extractor_stamp` replaces the TypeScript implementation's
//!   stamp-in-the-filename scheme (`extract.<stamp>.json`). Two graft versions
//!   sharing one store invalidate by column comparison instead of by writing to
//!   different paths.
//! * `files.hash` is what makes a rebuild incremental: unchanged hash means the
//!   file's symbols and edges are still valid and are not re-parsed.
//! * `symbols_fts` is an external-content FTS5 table over `symbols`, kept in sync
//!   by triggers. External content means the text is not stored twice; the
//!   triggers are mandatory, not an optimization, because a contentless FTS index
//!   silently returns stale rows if they drift.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Bumped whenever the DDL below changes in a way an existing store cannot serve.
/// Read from and written to `pragma user_version`.
pub const SCHEMA_VERSION: i64 = 4;

const DDL: &str = r#"
create table if not exists repos (
  id              integer primary key,
  root            text    not null unique,
  git_common_dir  text,
  indexed_at      integer not null,
  extractor_stamp text    not null
);

create table if not exists files (
  id       integer primary key,
  repo_id  integer not null references repos(id) on delete cascade,
  path     text    not null,
  mtime    integer not null,
  size     integer not null,
  hash     text    not null,
  unique (repo_id, path)
);

-- Parsed extraction payload for incremental builds. Symbol rows remain the
-- query surface; this cache retains unresolved calls/imports/bindings so edges
-- can be rebuilt repo-wide without reparsing unchanged source files.
create table if not exists file_extracts (
  file_id          integer primary key references files(id) on delete cascade,
  extractor_stamp  text    not null,
  payload           text    not null
);

create table if not exists symbols (
  id         integer primary key,
  repo_id    integer not null references repos(id) on delete cascade,
  file_id    integer not null references files(id) on delete cascade,
  name       text    not null,
  kind       text    not null,
  start_line integer not null,
  end_line   integer not null,
  signature  text,
  crux       text,
  summary    text,
  -- Enclosing class for a method, else null. This is what lets a member call
  -- resolve: `repo.scan()` binds `repo` to type Repo, and the edge target is the
  -- symbol named `scan` whose container is `Repo`. Without it every method named
  -- `scan` in the repo is an equally good candidate and the edge is dropped.
  container  text
);
create index if not exists symbols_by_name on symbols(repo_id, name);
create index if not exists symbols_by_file on symbols(file_id);
create index if not exists symbols_by_container on symbols(repo_id, container, name);

create table if not exists edges (
  repo_id       integer not null references repos(id) on delete cascade,
  src_symbol_id integer not null references symbols(id) on delete cascade,
  dst_symbol_id integer not null references symbols(id) on delete cascade,
  kind          text    not null
);
-- An edge is a relationship, not an occurrence: three calls to the same function
-- from one caller are one edge. Without this, in-edge counts inflate and callers
-- lists the same caller once per call site.
create unique index if not exists edges_unique
  on edges(repo_id, src_symbol_id, dst_symbol_id, kind);
-- callers(x) walks dst->src; --direction out walks src->dst. Both need an index
-- or every traversal degrades to a full scan of the repo's edge set.
create index if not exists edges_by_dst on edges(repo_id, dst_symbol_id);
create index if not exists edges_by_src on edges(repo_id, src_symbol_id);

create virtual table if not exists symbols_fts using fts5(
  name, signature, summary,
  content='symbols',
  content_rowid='id'
);

create trigger if not exists symbols_ai after insert on symbols begin
  insert into symbols_fts(rowid, name, signature, summary)
  values (new.id, new.name, new.signature, new.summary);
end;
create trigger if not exists symbols_ad after delete on symbols begin
  insert into symbols_fts(symbols_fts, rowid, name, signature, summary)
  values ('delete', old.id, old.name, old.signature, old.summary);
end;
create trigger if not exists symbols_au after update on symbols begin
  insert into symbols_fts(symbols_fts, rowid, name, signature, summary)
  values ('delete', old.id, old.name, old.signature, old.summary);
  insert into symbols_fts(rowid, name, signature, summary)
  values (new.id, new.name, new.signature, new.summary);
end;
"#;

/// `$XDG_DATA_HOME/graft/graft.db`, else `~/.local/share/graft/graft.db`.
pub fn default_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => {
            let home = std::env::var_os("HOME").context("neither XDG_DATA_HOME nor HOME is set")?;
            PathBuf::from(home).join(".local").join("share")
        }
    };
    Ok(base.join("graft").join("graft.db"))
}

/// Open (creating if absent), enable WAL and foreign keys, and apply the schema.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;

    // WAL so a query can read while a build writes. `journal_mode` returns the
    // mode actually in force, which is not necessarily the one requested — a
    // filesystem without the shared memory WAL needs will quietly stay in
    // rollback mode, and every concurrent read would then block behind a build.
    let mode: String = db.query_row("pragma journal_mode=wal", [], |r| r.get(0))?;
    anyhow::ensure!(
        mode.eq_ignore_ascii_case("wal"),
        "journal_mode is {mode:?}, not wal"
    );
    db.execute_batch("pragma foreign_keys=on; pragma synchronous=normal;")?;

    let found: i64 = db.query_row("pragma user_version", [], |r| r.get(0))?;
    anyhow::ensure!(
        matches!(found, 0 | 3 | SCHEMA_VERSION),
        "store at {} is schema v{found}; supported migrations are v3 -> v{SCHEMA_VERSION}",
        path.display()
    );
    db.execute_batch(DDL).context("apply schema")?;
    if found != SCHEMA_VERSION {
        db.execute_batch(&format!("pragma user_version={SCHEMA_VERSION}"))?;
    }
    Ok(db)
}

pub fn reset_repo(db: &Connection, root: &Path) -> Result<bool> {
    Ok(db.execute("delete from repos where root=?1", [root.to_string_lossy()])? > 0)
}

pub fn prune_missing(db: &Connection) -> Result<Vec<String>> {
    let mut statement = db.prepare("select root from repos order by root")?;
    let roots = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let missing: Vec<String> = roots
        .into_iter()
        .filter(|root| !Path::new(root).exists())
        .collect();
    for root in &missing {
        db.execute("delete from repos where root=?1", [root])?;
    }
    Ok(missing)
}

pub fn integrity(db: &Connection) -> Result<String> {
    db.query_row("pragma integrity_check", [], |row| row.get(0))
        .context("run SQLite integrity check")
}

/// Preserve a broken store beside the replacement. Recovery never destroys the
/// only copy an operator might need for diagnosis.
pub fn recover(path: &Path) -> Result<PathBuf> {
    anyhow::ensure!(path.exists(), "{} does not exist", path.display());
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let backup = path.with_extension(format!("corrupt-{timestamp}.db"));
    std::fs::rename(path, &backup)
        .with_context(|| format!("preserve corrupt store as {}", backup.display()))?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
        if sidecar.exists() {
            let backup_sidecar = PathBuf::from(format!("{}{suffix}", backup.display()));
            std::fs::rename(sidecar, backup_sidecar)?;
        }
    }
    open(path).context("create replacement store")?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempdir::Guard, Connection) {
        let g = tempdir::Guard::new();
        let db = open(&g.path().join("graft.db")).expect("open");
        (g, db)
    }

    /// Minimal scratch-dir helper so the tests need no dev-dependency.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Guard(PathBuf);
        impl Guard {
            pub fn new() -> Self {
                // Monotonic counter + pid: unique per test without pulling in rand.
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let p = std::env::temp_dir().join(format!(
                    "graft-test-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Guard(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn seed_repo(db: &Connection) -> i64 {
        db.execute(
            "insert into repos(root, git_common_dir, indexed_at, extractor_stamp)
             values ('/src/demo', '/src/demo/.git', 1, 'stamp-a')",
            [],
        )
        .unwrap();
        let repo = db.last_insert_rowid();
        db.execute(
            "insert into files(repo_id, path, mtime, size, hash)
             values (?1, 'src/a.ts', 1, 10, 'h')",
            [repo],
        )
        .unwrap();
        let file = db.last_insert_rowid();
        db.execute(
            "insert into symbols(repo_id, file_id, name, kind, start_line, end_line, signature, summary)
             values (?1, ?2, 'parseRepo', 'function', 1, 9, 'fn parseRepo(p)', 'walks the tree')",
            [repo, file],
        )
        .unwrap();
        repo
    }

    #[test]
    fn open_is_idempotent_and_stamps_the_version() {
        let g = tempdir::Guard::new();
        let p = g.path().join("graft.db");
        let db = open(&p).unwrap();
        let v: i64 = db
            .query_row("pragma user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        drop(db);
        open(&p).expect("re-opening an existing store must not fail");
    }

    #[test]
    fn migrates_v3_store_without_dropping_repo_rows() {
        let g = tempdir::Guard::new();
        let p = g.path().join("graft.db");
        let db = open(&p).unwrap();
        seed_repo(&db);
        db.execute_batch("drop table file_extracts; pragma user_version=3;")
            .unwrap();
        drop(db);

        let db = open(&p).unwrap();
        let repos: i64 = db
            .query_row("select count(*) from repos", [], |r| r.get(0))
            .unwrap();
        let cache_table: i64 = db
            .query_row(
                "select count(*) from sqlite_master where type='table' and name='file_extracts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(repos, 1, "migration must preserve indexed repositories");
        assert_eq!(cache_table, 1, "migration must create extraction cache");
    }

    #[test]
    fn wal_reader_sees_committed_state_while_writer_is_open() {
        let g = tempdir::Guard::new();
        let path = g.path().join("graft.db");
        let mut writer = open(&path).unwrap();
        let reader = open(&path).unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute(
            "insert into repos(root, indexed_at, extractor_stamp) values ('/pending', 1, 'x')",
            [],
        )
        .unwrap();
        let before: i64 = reader
            .query_row("select count(*) from repos", [], |row| row.get(0))
            .unwrap();
        assert_eq!(before, 0);
        tx.commit().unwrap();
        let after: i64 = reader
            .query_row("select count(*) from repos", [], |row| row.get(0))
            .unwrap();
        assert_eq!(after, 1);
    }

    #[test]
    fn dropped_transaction_leaves_no_partial_repo() {
        let g = tempdir::Guard::new();
        let path = g.path().join("graft.db");
        let mut db = open(&path).unwrap();
        {
            let tx = db.transaction().unwrap();
            tx.execute(
                "insert into repos(root, indexed_at, extractor_stamp) values ('/partial', 1, 'x')",
                [],
            )
            .unwrap();
        }
        let count: i64 = db
            .query_row("select count(*) from repos", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn fts_mirrors_symbol_inserts_updates_and_deletes() {
        // The external-content index is only correct while the triggers fire; if
        // they ever stop, searches keep returning rows that no longer exist.
        let (_g, db) = temp_db();
        seed_repo(&db);

        let hit = |q: &str| -> Vec<String> {
            db.prepare("select name from symbols_fts where symbols_fts match ?1")
                .unwrap()
                .query_map([q], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };

        assert_eq!(hit("parse*"), vec!["parseRepo".to_string()], "insert");

        // A match is against EVERY indexed column, so the update has to move the
        // token out of `signature` as well. Renaming `name` alone would leave
        // `fn parseRepo(p)` behind and `parse*` would still hit — correctly, which
        // makes that a useless assertion rather than a failing one.
        db.execute(
            "update symbols set name='renameRepo', signature='fn renameRepo(p)'
             where name='parseRepo'",
            [],
        )
        .unwrap();
        assert!(
            hit("parse*").is_empty(),
            "update must retract the old terms"
        );
        assert_eq!(hit("rename*"), vec!["renameRepo".to_string()], "update");

        db.execute("delete from symbols", []).unwrap();
        assert!(hit("rename*").is_empty(), "delete must retract the row");

        // The index and its content table must still agree; external-content FTS5
        // reports drift here rather than at query time, where it surfaces as the
        // far less obvious "database disk image is malformed".
        db.execute(
            "insert into symbols_fts(symbols_fts) values('integrity-check')",
            [],
        )
        .expect("fts index and content table disagree");
    }

    #[test]
    fn dropping_a_repo_cascades_to_files_symbols_edges_and_the_index() {
        let (_g, db) = temp_db();
        let repo = seed_repo(&db);
        let sym: i64 = db
            .query_row("select id from symbols", [], |r| r.get(0))
            .unwrap();
        db.execute(
            "insert into edges(repo_id, src_symbol_id, dst_symbol_id, kind)
             values (?1, ?2, ?2, 'calls')",
            [repo, sym],
        )
        .unwrap();

        db.execute("delete from repos where id=?1", [repo]).unwrap();

        for t in ["files", "symbols", "edges"] {
            let n: i64 = db
                .query_row(&format!("select count(*) from {t}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{t} should have cascaded away");
        }
        let n: i64 = db
            .query_row(
                "select count(*) from symbols_fts where symbols_fts match 'parse*'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "fts must not outlive the symbols it indexes");
    }

    #[test]
    fn a_repo_root_can_only_be_registered_once() {
        let (_g, db) = temp_db();
        seed_repo(&db);
        let again = db.execute(
            "insert into repos(root, indexed_at, extractor_stamp)
             values ('/src/demo', 2, 'stamp-b')",
            [],
        );
        assert!(
            again.is_err(),
            "repos.root is the lookup key; duplicates would make it ambiguous"
        );
    }
}
