//! Toolchain spike — NOT the real CLI.
//!
//! Proves the three native dependencies the rewrite rests on actually build and
//! run on this device (Termux, aarch64) before any real code is written against
//! them. Every install problem this fork has hit so far has been native-toolchain
//! packaging, so this is the risk worth retiring first.
//!
//! Checks, in order:
//!   1. bundled SQLite opens, and FTS5 is compiled in (the `ask`/`grep` index
//!      depends on it — rusqlite's `bundled` feature is only useful here if FTS5
//!      came along).
//!   2. WAL mode engages (a reader can query while a build writes).
//!   3. Each tree-sitter grammar parses a snippet of its language.

mod db;

use anyhow::{Context, Result};
use rusqlite::Connection;

fn check_sqlite() -> Result<()> {
    // File-backed, not `:memory:` — an in-memory database silently reports
    // `memory` for `journal_mode=wal`, which reads like a pass and proves nothing.
    // WAL is what lets the MCP server query while a build is writing, so it has to
    // be checked against a real file on this filesystem.
    let dir = std::env::temp_dir().join("graft-spike");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("spike.db");
    let _ = std::fs::remove_file(&path);
    let db = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;

    let version: String = db.query_row("select sqlite_version()", [], |r| r.get(0))?;
    println!("  sqlite      {version}");

    let mode: String = db.query_row("pragma journal_mode=wal", [], |r| r.get(0))?;
    anyhow::ensure!(
        mode.eq_ignore_ascii_case("wal"),
        "journal_mode came back {mode:?}, not wal — this filesystem may not support \
         the shared memory WAL needs (Android/Termux storage is worth suspecting)"
    );
    println!("  journal     {mode}");

    // A second connection must be able to read while the first holds a write txn.
    db.execute_batch("create table t(x); begin; insert into t values (1);")?;
    let reader = Connection::open(&path).context("second connection")?;
    let n: i64 = reader
        .query_row("select count(*) from t", [], |r| r.get(0))
        .context("concurrent read during an open write txn")?;
    db.execute_batch("commit")?;
    println!("  concurrency reader saw {n} row(s) mid-write (pre-commit snapshot)");

    db.execute_batch(
        "create virtual table symbols_fts using fts5(name, signature, summary);
         insert into symbols_fts values
           ('parseRepo', 'fn parseRepo(p: Path)', 'walks the tree and extracts symbols'),
           ('renderCard', 'fn renderCard(n: Node)', 'projects a node to markdown');",
    )
    .context("create fts5 table — bundled sqlite may lack SQLITE_ENABLE_FTS5")?;

    let mut stmt =
        db.prepare("select name, rank from symbols_fts where symbols_fts match ?1 order by rank")?;
    let hits: Vec<(String, f64)> = stmt
        .query_map(["parse*"], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    println!("  fts5        available, ranked hits: {hits:?}");

    Ok(())
}

fn check_grammars() -> Result<()> {
    let cases: [(&str, tree_sitter::Language, &str); 4] = [
        (
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "export function greet(name: string): string { return `hi ${name}`; }",
        ),
        (
            "tsx",
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            "const App = () => <div className=\"x\">hi</div>;",
        ),
        (
            "python",
            tree_sitter_python::LANGUAGE.into(),
            "def greet(name: str) -> str:\n    return f'hi {name}'\n",
        ),
        (
            "go",
            tree_sitter_go::LANGUAGE.into(),
            "package main\nfunc Greet(name string) string { return \"hi \" + name }\n",
        ),
    ];

    for (label, lang, src) in cases {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang)
            .with_context(|| format!("set_language for {label}"))?;
        let tree = parser
            .parse(src, None)
            .with_context(|| format!("parse {label}"))?;
        let root = tree.root_node();
        anyhow::ensure!(!root.has_error(), "{label}: parse tree has an ERROR node");
        println!(
            "  {label:<11} ok — root={} children={}",
            root.kind(),
            root.child_count()
        );
    }

    Ok(())
}

fn main() -> Result<()> {
    println!(
        "graft rust spike — v{} on {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH
    );
    println!("sqlite:");
    check_sqlite()?;
    println!("grammars:");
    check_grammars()?;
    println!("store:");
    println!("  default     {}", db::default_path()?.display());
    println!("\nall native dependencies build and run here.");
    Ok(())
}
