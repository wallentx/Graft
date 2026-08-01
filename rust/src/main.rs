//! `graft` — the Rust port, in progress.
//!
//! Nothing is written into an indexed repository. The graph lives in one SQLite
//! store outside every work tree; a repo is identified by the realpath of its git
//! toplevel. A directory graft has never indexed is reported as such rather than
//! answered from an empty graph.

mod db;
mod extract;
mod index;
mod repo;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "graft", about = "Repo context graph, stored outside the repo")]
struct Cli {
    /// Store to use. Defaults to $XDG_DATA_HOME/graft/graft.db.
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index a repo into the store.
    Build {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Find every symbol whose name or signature contains a literal.
    Grep {
        pattern: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Who calls a symbol (or, with --direction out, what it calls).
    Callers {
        symbol: String,
        /// Follow outgoing edges instead of incoming.
        #[arg(long, value_parser = ["in", "out"], default_value = "in")]
        direction: String,
        /// How far to walk. `all` follows every connected edge.
        #[arg(long, default_value = "1")]
        depth: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Every signature in one file.
    Skeleton {
        file: PathBuf,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Orientation: directory clusters, hubs, hotspots.
    Map {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Report what the store knows about a repo.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

/// The one message a caller sees for an unindexed directory. Every read path
/// routes through here so the instruction is identical wherever it surfaces —
/// including, later, the MCP server.
fn not_indexed(root: &std::path::Path) -> String {
    format!(
        "{} has not been indexed by graft — run `graft build` to index it",
        root.display()
    )
}

fn main() -> Result<()> {
    // Rust ignores SIGPIPE, so `graft grep x | head` panics on the first write
    // past the closed pipe instead of exiting quietly the way every other CLI
    // does. Restoring the default disposition makes piping behave.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    let store = match cli.store {
        Some(p) => p,
        None => db::default_path()?,
    };

    match cli.cmd {
        Cmd::Build { path } => {
            let root = repo::root_of(&path)?;
            let mut conn = db::open(&store)?;
            let t0 = std::time::Instant::now();
            let s = index::build(&mut conn, &root)?;
            println!(
                "indexed {} — {} files, {} symbols, {} edges ({} unresolved) in {:.2}s",
                root.display(),
                s.files,
                s.symbols,
                s.edges,
                s.unresolved,
                t0.elapsed().as_secs_f64()
            );
            println!("store: {}", store.display());
        }

        Cmd::Grep { pattern, path } => {
            let root = repo::root_of(&path)?;
            let conn = db::open(&store)?;
            let Some(repo_id) = index::repo_id_of(&conn, &root)? else {
                eprintln!("{}", not_indexed(&root));
                std::process::exit(2);
            };
            let r = index::grep(&conn, repo_id, &root, &pattern)?;
            if r.total_hits == 0 {
                println!(
                    "no hits for {pattern:?} in {} indexed files",
                    r.files_searched
                );
                return Ok(());
            }
            println!(
                "{pattern:?} — {} hits in {} symbols across {} files (searched {} indexed files)",
                r.total_hits,
                r.groups.len(),
                r.groups.iter().map(|g| &g.path).collect::<std::collections::HashSet<_>>().len(),
                r.files_searched
            );
            for g in &r.groups {
                match &g.symbol {
                    Some(name) => println!(
                        "\n{name} · {} · {}:L{}-L{} · {} in-edges",
                        g.kind, g.path, g.start_line, g.end_line, g.in_edges
                    ),
                    None => println!("\n{} · file scope", g.path),
                }
                for h in &g.hits {
                    println!("  L{}: {}", h.line, h.text);
                }
            }
            if r.unreadable > 0 {
                eprintln!("\n{} indexed file(s) could not be read", r.unreadable);
            }
        }

        Cmd::Callers { symbol, direction, depth, path } => {
            let root = repo::root_of(&path)?;
            let conn = db::open(&store)?;
            let Some(repo_id) = index::repo_id_of(&conn, &root)? else {
                eprintln!("{}", not_indexed(&root));
                std::process::exit(2);
            };
            let max = if depth == "all" { usize::MAX } else { depth.parse().unwrap_or(1) };
            let out = direction == "out";
            let (seeds, reached) = index::callers(&conn, repo_id, &symbol, out, max)?;
            if seeds.is_empty() {
                eprintln!("no symbol named {symbol:?} — check the spelling, or run `graft build`");
                std::process::exit(2);
            }
            for (_, name, kind, p, s0, s1) in &seeds {
                println!("{name} · {kind} · {p}:L{s0}-L{s1}");
            }
            if reached.is_empty() {
                println!("  (no {} edges)", if out { "outgoing" } else { "incoming" });
            }
            for r in &reached {
                let arrow = if out { "→" } else { "←" };
                println!(
                    "  {} {} {} ({}:L{}-L{}) [depth {}]",
                    r.edge, arrow, r.name, r.path, r.start_line, r.end_line, r.depth
                );
            }
        }

        Cmd::Skeleton { file, path } => {
            let root = repo::root_of(&path)?;
            let conn = db::open(&store)?;
            let Some(repo_id) = index::repo_id_of(&conn, &root)? else {
                eprintln!("{}", not_indexed(&root));
                std::process::exit(2);
            };
            // Accept either a repo-relative path or one the shell completed.
            let abs = std::fs::canonicalize(&file).unwrap_or_else(|_| root.join(&file));
            let rel = abs.strip_prefix(&root).unwrap_or(&file).to_string_lossy().replace('\\', "/");
            let rows = index::skeleton(&conn, repo_id, &rel)?;
            if rows.is_empty() {
                eprintln!("no indexed symbols in {rel} — is it indexed, and a language graft reads?");
                std::process::exit(2);
            }
            println!("graft skeleton — {rel}");
            for r in &rows {
                let owner = r.container.as_deref().map(|c| format!("{c}.")).unwrap_or_default();
                println!("- L{}-L{}  {} {}{}  {}", r.start_line, r.end_line, r.kind, owner, r.name, r.signature);
            }
        }

        Cmd::Map { path } => {
            let root = repo::root_of(&path)?;
            let conn = db::open(&store)?;
            let Some(repo_id) = index::repo_id_of(&conn, &root)? else {
                eprintln!("{}", not_indexed(&root));
                std::process::exit(2);
            };
            let m = index::repo_map(&conn, repo_id, 12)?;
            println!("repo map — {} files · {} symbols · {} edges", m.files, m.symbols, m.edges);
            println!();
            for d in &m.dirs {
                let hubs = d
                    .hubs
                    .iter()
                    .map(|h| {
                        let base = h.path.rsplit('/').next().unwrap_or(&h.path);
                        format!("{} ({}, {}←)", h.name, base, h.in_degree)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let tail = if hubs.is_empty() { String::new() } else { format!("   hubs: {hubs}") };
                println!("{:<18}{} files · {} symbols{}", d.dir, d.files, d.symbols, tail);
            }
            if !m.hotspots.is_empty() {
                println!();
                print!("hotspots:");
                for h in &m.hotspots {
                    print!("  {} · {} · {}:L{}-L{} · {}←", h.name, h.kind, h.path, h.start_line, h.end_line, h.in_degree);
                }
                println!();
            }
        }

        Cmd::Status { path } => {
            let root = repo::root_of(&path)?;
            let conn = db::open(&store)?;
            match index::repo_id_of(&conn, &root)? {
                None => {
                    eprintln!("{}", not_indexed(&root));
                    std::process::exit(2);
                }
                Some(id) => {
                    let (files, syms, edges): (i64, i64, i64) = conn.query_row(
                        "select (select count(*) from files   where repo_id=?1),
                                (select count(*) from symbols where repo_id=?1),
                                (select count(*) from edges   where repo_id=?1)",
                        [id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )?;
                    let stamp: String =
                        conn.query_row("select extractor_stamp from repos where id=?1", [id], |r| {
                            r.get(0)
                        })?;
                    println!("{}", root.display());
                    println!("  files    {files}");
                    println!("  symbols  {syms}");
                    println!("  edges    {edges}");
                    println!("  stamp    {stamp}");
                    println!("  store    {}", store.display());
                }
            }
        }
    }
    Ok(())
}
