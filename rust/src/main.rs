//! `graft` — the Rust port, in progress.
//!
//! Nothing is written into an indexed repository. The graph lives in one SQLite
//! store outside every work tree; a repo is identified by the realpath of its git
//! toplevel. A directory graft has never indexed is reported as such rather than
//! answered from an empty graph.

mod ask;
mod db;
mod export;
mod extract;
mod index;
mod init;
mod mcp;
mod repo;
mod viz;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "graft",
    version,
    about = "Repo context graph, stored outside the repo"
)]
struct Cli {
    /// Store to use. Defaults to $XDG_DATA_HOME/graft/graft.db.
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    /// Answer from the current store without checking/rebuilding changed source.
    #[arg(long, global = true)]
    no_refresh: bool,
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
    /// Find the most relevant symbols for a natural-language question.
    Ask {
        query: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(short = 'n', long, default_value_t = 8)]
        limit: usize,
        #[arg(long)]
        source: bool,
        #[arg(long)]
        full: bool,
        #[arg(long = "in")]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Exhaustively search indexed source with a regular expression.
    Grep {
        pattern: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Treat the pattern as a literal string.
        #[arg(long)]
        fixed: bool,
        #[arg(short = 'i', long)]
        ignore_case: bool,
        #[arg(long = "in")]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
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
        #[arg(long = "in")]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Every signature in one file.
    Skeleton {
        file: PathBuf,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Orientation: directory clusters, hubs, hotspots.
    Map {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 12)]
        max_dirs: usize,
        #[arg(long)]
        json: bool,
    },
    /// Serve Graft tools to MCP hosts over stdin/stdout JSON-RPC.
    Mcp {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Register the installed binary with user-level agent host configuration.
    Init {
        #[arg(long = "host", required = true)]
        hosts: Vec<String>,
        /// Print every target without writing anything.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Fail when the store is absent, stale, or built by another extractor.
    Check {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Export deterministic Markdown cards or one JSON document.
    Export {
        destination: PathBuf,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        force: bool,
    },
    /// Print completion code for a shell.
    Completions { shell: Shell },
    /// Store maintenance.
    Cache {
        #[command(subcommand)]
        command: CacheCmd,
    },
    /// Detailed build, install, and store compatibility identity.
    Version {
        #[arg(long)]
        json: bool,
    },
    /// Show the safe package-manager update path; never self-modifies.
    Upgrade,
    /// Export or serve a self-contained repository map viewer.
    Viz {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Write HTML instead of starting a server.
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value = "127.0.0.1:0")]
        bind: String,
        #[arg(long)]
        allow_remote: bool,
        #[arg(long)]
        force: bool,
    },
    /// Report what the store knows about a repo.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Remove records for repository paths that no longer exist.
    Prune,
    /// Run SQLite's full integrity check.
    Doctor,
    /// Preserve a corrupt store and create a clean replacement.
    Recover,
    /// Remove one repository graph from the shared store.
    Reset {
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

fn refresh_disabled(flag: bool) -> bool {
    if flag {
        return true;
    }
    std::env::var("GRAFT_NO_REFRESH")
        .ok()
        .is_some_and(|value| !matches!(value.as_str(), "" | "0" | "false"))
}

fn ready_repo(
    conn: &mut rusqlite::Connection,
    root: &std::path::Path,
    no_refresh: bool,
) -> Result<Option<i64>> {
    let state = index::freshness(conn, root)?;
    if !state.indexed {
        return Ok(None);
    }
    if !state.is_clean() && !refresh_disabled(no_refresh) {
        let changed = state.changed_count();
        let stats = index::build(conn, root)?;
        eprintln!(
            "[graft] refreshed {} changed file(s): {} parsed, {} reused, {} deleted",
            changed, stats.parsed, stats.reused, stats.deleted
        );
    }
    index::repo_id_of(conn, root)
}

struct ReadyTarget {
    target: repo::Target,
    conn: rusqlite::Connection,
    repo_id: i64,
}

#[derive(Serialize)]
struct StatusOutput {
    scope: String,
    root: String,
    #[serde(flatten)]
    status: index::RepoStatus,
    schema_version: i64,
    age_seconds: i64,
    store: String,
    freshness: index::Freshness,
}

fn ready_targets(
    store: &std::path::Path,
    path: &std::path::Path,
    no_refresh: bool,
) -> Result<Option<Vec<ReadyTarget>>> {
    let mut ready = Vec::new();
    for target in repo::targets(path)? {
        let mut conn = db::open(store)?;
        let Some(repo_id) = ready_repo(&mut conn, &target.root, no_refresh)? else {
            eprintln!("{}", not_indexed(&target.root));
            return Ok(None);
        };
        ready.push(ReadyTarget {
            target,
            conn,
            repo_id,
        });
    }
    Ok(Some(ready))
}

/// Interpret `--in repo/path` as a workspace child plus a child-local path.
/// A scope without a child label applies inside every child.
fn target_scope<'a>(
    targets: &[ReadyTarget],
    target_label: &str,
    scope: Option<&'a str>,
) -> Option<Option<&'a str>> {
    let Some(scope) = scope else {
        return Some(None);
    };
    let scope = scope.trim_matches('/');
    let (head, tail) = scope.split_once('/').unwrap_or((scope, ""));
    if targets.iter().any(|target| target.target.label == head) {
        return (head == target_label).then_some((!tail.is_empty()).then_some(tail));
    }
    Some((!scope.is_empty()).then_some(scope))
}

fn parse_depth(value: &str) -> Option<usize> {
    if value == "all" {
        return Some(usize::MAX);
    }
    value.parse().ok().filter(|depth| *depth > 0)
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
    let no_refresh = cli.no_refresh;
    let store = match cli.store {
        Some(p) => p,
        None => db::default_path()?,
    };

    match cli.cmd {
        Cmd::Build { path } => {
            let mut conn = db::open(&store)?;
            for target in repo::targets(&path)? {
                let t0 = std::time::Instant::now();
                let s = index::build(&mut conn, &target.root)?;
                println!(
                    "[{}] indexed {} — {} files, {} symbols, {} edges ({} unresolved; {} parsed, {} reused, {} deleted) in {:.2}s",
                    target.label,
                    target.root.display(),
                    s.files,
                    s.symbols,
                    s.edges,
                    s.unresolved,
                    s.parsed,
                    s.reused,
                    s.deleted,
                    t0.elapsed().as_secs_f64()
                );
            }
            println!("store: {}", store.display());
        }

        Cmd::Ask {
            query,
            path,
            limit,
            source,
            full,
            scope,
            json,
        } => {
            let Some(ready) = ready_targets(&store, &path, no_refresh)? else {
                std::process::exit(2);
            };
            let mut results = Vec::new();
            for target in &ready {
                let Some(child_scope) =
                    target_scope(&ready, &target.target.label, scope.as_deref())
                else {
                    continue;
                };
                let result = ask::ask(
                    &target.conn,
                    target.repo_id,
                    &target.target.root,
                    &query,
                    ask::AskOptions {
                        limit,
                        scope: child_scope,
                        source: source || full,
                        full,
                    },
                )?;
                results.push((target.target.label.as_str(), result));
            }
            #[derive(Serialize)]
            struct ScopedHit<'a> {
                scope: &'a str,
                #[serde(flatten)]
                hit: &'a ask::AskHit,
            }
            let mut hits = Vec::new();
            let max_rank = results
                .iter()
                .map(|(_, result)| result.hits.len())
                .max()
                .unwrap_or(0);
            for rank in 0..max_rank {
                for (label, result) in &results {
                    if let Some(hit) = result.hits.get(rank) {
                        hits.push(ScopedHit { scope: label, hit });
                        if hits.len() == limit.max(1) {
                            break;
                        }
                    }
                }
                if hits.len() == limit.max(1) {
                    break;
                }
            }
            let mode = if results.len() == 1 {
                results[0].1.mode
            } else if results
                .iter()
                .all(|(_, result)| result.mode.starts_with("structural"))
            {
                "structural-workspace"
            } else {
                "lexical-workspace"
            };
            if json {
                #[derive(Serialize)]
                struct Output<'a> {
                    query: &'a str,
                    mode: &'a str,
                    hits: Vec<ScopedHit<'a>>,
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&Output {
                        query: &query,
                        mode,
                        hits,
                    })?
                );
            } else {
                println!("graft ask — {query:?} ({mode})\n");
                for (scope, result) in &results {
                    if let Some(note) = &result.note {
                        println!("[{scope}/] note: {note}\n");
                    }
                }
                if hits.is_empty() {
                    println!("no matching nodes");
                }
                for (position, scoped) in hits.iter().enumerate() {
                    let hit = scoped.hit;
                    println!(
                        "{}. [{}/] {} · {}  {}:L{}-L{}\n   {}",
                        position + 1,
                        scoped.scope,
                        hit.name,
                        hit.kind,
                        hit.path,
                        hit.start_line,
                        hit.end_line,
                        hit.signature
                    );
                    if let Some(source) = &hit.source {
                        println!("\n```\n{source}\n```");
                    }
                    println!();
                }
            }
        }

        Cmd::Grep {
            pattern,
            path,
            fixed,
            ignore_case,
            scope,
            json,
        } => {
            if !fixed && regex::RegexBuilder::new(&pattern).build().is_err() {
                eprintln!("invalid regular expression {pattern:?}");
                std::process::exit(2);
            }
            let Some(ready) = ready_targets(&store, &path, no_refresh)? else {
                std::process::exit(2);
            };
            let mut results = Vec::new();
            for target in &ready {
                let Some(child_scope) =
                    target_scope(&ready, &target.target.label, scope.as_deref())
                else {
                    continue;
                };
                let result = index::grep_with_options(
                    &target.conn,
                    target.repo_id,
                    &target.target.root,
                    &pattern,
                    index::GrepOptions {
                        ignore_case,
                        fixed,
                        scope: child_scope,
                    },
                )?;
                results.push((target.target.label.as_str(), result));
            }
            if json {
                #[derive(Serialize)]
                struct Scoped<'a> {
                    scope: &'a str,
                    #[serde(flatten)]
                    result: &'a index::GrepResult,
                }
                let scoped: Vec<_> = results
                    .iter()
                    .map(|(scope, result)| Scoped { scope, result })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&scoped)?);
                return Ok(());
            }
            let total: usize = results.iter().map(|(_, result)| result.total_hits).sum();
            if total == 0 {
                let searched: usize = results
                    .iter()
                    .map(|(_, result)| result.files_searched)
                    .sum();
                println!("no hits for {pattern:?} in {searched} indexed files");
                return Ok(());
            }
            for (label, r) in &results {
                println!(
                    "[{label}/] {pattern:?} — {} hits in {} symbols across {} files (searched {} indexed files)",
                    r.total_hits,
                    r.groups.len(),
                    r.groups
                        .iter()
                        .map(|group| &group.path)
                        .collect::<std::collections::HashSet<_>>()
                        .len(),
                    r.files_searched
                );
                for group in &r.groups {
                    match &group.symbol {
                        Some(name) => println!(
                            "\n[{label}/] {name} · {} · {}:L{}-L{} · {} in-edges",
                            group.kind,
                            group.path,
                            group.start_line,
                            group.end_line,
                            group.in_edges
                        ),
                        None => println!("\n[{label}/] {} · file scope", group.path),
                    }
                    for hit in &group.hits {
                        println!("  L{}: {}", hit.line, hit.text);
                    }
                }
                if r.unreadable > 0 {
                    eprintln!(
                        "\n[{label}/] {} indexed file(s) could not be read",
                        r.unreadable
                    );
                }
            }
        }

        Cmd::Callers {
            symbol,
            direction,
            depth,
            path,
            scope,
            json,
        } => {
            let Some(ready) = ready_targets(&store, &path, no_refresh)? else {
                std::process::exit(2);
            };
            let Some(max) = parse_depth(&depth) else {
                eprintln!("--depth must be a positive integer or 'all'");
                std::process::exit(2);
            };
            let out = direction == "out";
            let mut results = Vec::new();
            for target in &ready {
                let Some(child_scope) =
                    target_scope(&ready, &target.target.label, scope.as_deref())
                else {
                    continue;
                };
                let (seeds, reached) = index::callers_scoped(
                    &target.conn,
                    target.repo_id,
                    &symbol,
                    out,
                    max,
                    child_scope,
                )?;
                results.push((target.target.label.as_str(), seeds, reached));
            }
            if results.iter().all(|(_, seeds, _)| seeds.is_empty()) {
                eprintln!("no symbol named {symbol:?} — check the spelling, or run `graft build`");
                std::process::exit(2);
            }
            if json {
                #[derive(Serialize)]
                struct Output<'a> {
                    scope: &'a str,
                    seeds: &'a [index::Seed],
                    reached: &'a [index::Reached],
                }
                let output: Vec<_> = results
                    .iter()
                    .map(|(scope, seeds, reached)| Output {
                        scope,
                        seeds,
                        reached,
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "symbol": symbol,
                        "direction": direction,
                        "results": output,
                    }))?
                );
                return Ok(());
            }
            for (label, seeds, reached) in &results {
                for seed in seeds {
                    println!(
                        "[{label}/] {} · {} · {}:L{}-L{}",
                        seed.name, seed.kind, seed.path, seed.start_line, seed.end_line
                    );
                }
                if !seeds.is_empty() && reached.is_empty() {
                    println!("  (no {} edges)", if out { "outgoing" } else { "incoming" });
                }
                for reached in reached {
                    let arrow = if out { "→" } else { "←" };
                    println!(
                        "  {} {} [{label}/] {} · {} ({}:L{}-L{}) [depth {}]",
                        reached.edge,
                        arrow,
                        reached.name,
                        reached.kind,
                        reached.path,
                        reached.start_line,
                        reached.end_line,
                        reached.depth
                    );
                }
            }
        }

        Cmd::Skeleton { file, path, json } => {
            let Some(ready) = ready_targets(&store, &path, no_refresh)? else {
                std::process::exit(2);
            };
            let requested = file.to_string_lossy().replace('\\', "/");
            let mut matches = Vec::new();
            for target in &ready {
                let local = std::fs::canonicalize(&file)
                    .ok()
                    .and_then(|absolute| {
                        absolute
                            .strip_prefix(&target.target.root)
                            .ok()
                            .map(|path| path.to_string_lossy().replace('\\', "/"))
                    })
                    .or_else(|| {
                        requested
                            .strip_prefix(&format!("{}/", target.target.label))
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| requested.clone());
                if let Some(rel) = index::skeleton_path(&target.conn, target.repo_id, &local)? {
                    let rows = index::skeleton(&target.conn, target.repo_id, &rel)?;
                    if !rows.is_empty() {
                        matches.push((target.target.label.as_str(), rel, rows));
                    }
                }
            }
            if matches.len() != 1 {
                eprintln!(
                    "expected one indexed file matching {requested:?}; found {}",
                    matches.len()
                );
                std::process::exit(2);
            }
            let (label, rel, rows) = &matches[0];
            if json {
                #[derive(Serialize)]
                struct Output<'a> {
                    scope: &'a str,
                    path: &'a str,
                    symbols: &'a [index::SkelRow],
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&Output {
                        scope: label,
                        path: rel,
                        symbols: rows,
                    })?
                );
                return Ok(());
            }
            println!("graft skeleton — [{label}/] {rel}");
            for r in rows {
                let owner = r
                    .container
                    .as_deref()
                    .map(|c| format!("{c}."))
                    .unwrap_or_default();
                println!(
                    "- L{}-L{}  {} {}{}  {}",
                    r.start_line, r.end_line, r.kind, owner, r.name, r.signature
                );
            }
        }

        Cmd::Map {
            path,
            max_dirs,
            json,
        } => {
            let Some(ready) = ready_targets(&store, &path, no_refresh)? else {
                std::process::exit(2);
            };
            let mut maps = Vec::new();
            for target in &ready {
                maps.push((
                    target.target.label.as_str(),
                    index::repo_map(&target.conn, target.repo_id, max_dirs)?,
                ));
            }
            if json {
                #[derive(Serialize)]
                struct Scoped<'a> {
                    scope: &'a str,
                    #[serde(flatten)]
                    map: &'a index::RepoMap,
                }
                let output: Vec<_> = maps
                    .iter()
                    .map(|(scope, map)| Scoped { scope, map })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&output)?);
                return Ok(());
            }
            for (label, m) in &maps {
                println!(
                    "repo map — [{label}/] {} files · {} symbols · {} edges",
                    m.files, m.symbols, m.edges
                );
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
                    let tail = if hubs.is_empty() {
                        String::new()
                    } else {
                        format!("   hubs: {hubs}")
                    };
                    println!(
                        "{:<18}{} files · {} symbols{}",
                        d.dir, d.files, d.symbols, tail
                    );
                }
                if m.dropped_dirs > 0 {
                    println!("... {} more directorie(s)", m.dropped_dirs);
                }
                if !m.hotspots.is_empty() {
                    println!();
                    print!("hotspots:");
                    for h in &m.hotspots {
                        print!(
                            "  {} · {} · {}:L{}-L{} · {}←",
                            h.name, h.kind, h.path, h.start_line, h.end_line, h.in_degree
                        );
                    }
                    println!();
                }
                println!();
            }
        }

        Cmd::Mcp { path } => mcp::serve(&store, &path)?,

        Cmd::Init {
            hosts,
            dry_run,
            json,
        } => {
            let writes = init::run(&hosts, dry_run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&writes)?);
            } else {
                println!("graft init{}", if dry_run { " --dry-run" } else { "" });
                for write in writes {
                    println!(
                        "  {} {} ({}, {})",
                        if dry_run { "would write" } else { "wrote" },
                        write.path,
                        write.host,
                        write.format
                    );
                }
            }
        }

        Cmd::Check { path, json } => {
            let conn = db::open(&store)?;
            let mut states = Vec::new();
            for target in repo::targets(&path)? {
                states.push((
                    target.label,
                    target.root.clone(),
                    index::freshness(&conn, &target.root)?,
                ));
            }
            if json {
                #[derive(Serialize)]
                struct Scoped<'a> {
                    scope: &'a str,
                    root: String,
                    #[serde(flatten)]
                    state: &'a index::Freshness,
                }
                let output: Vec<_> = states
                    .iter()
                    .map(|(scope, root, state)| Scoped {
                        scope,
                        root: root.to_string_lossy().into_owned(),
                        state,
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                for (scope, root, state) in &states {
                    if !state.indexed {
                        println!("[{scope}/] graft check: NO INDEX\n\n{}", not_indexed(root));
                    } else if state.is_clean() {
                        println!("[{scope}/] graft check: OK — index matches source");
                    } else {
                        println!("[{scope}/] graft check: STALE");
                        if !state.extractor_current {
                            println!("  extractor changed — run `graft build`");
                        }
                        for (label, paths) in [
                            ("added", &state.added),
                            ("modified", &state.modified),
                            ("deleted", &state.deleted),
                        ] {
                            for path in paths {
                                println!("  {label}: {path}");
                            }
                        }
                    }
                }
            }
            if states.iter().any(|(_, _, state)| !state.is_clean()) {
                std::process::exit(1);
            }
        }

        Cmd::Export {
            destination,
            path,
            json,
            force,
        } => {
            let Some(ready) = ready_targets(&store, &path, no_refresh)? else {
                std::process::exit(2);
            };
            if ready.len() == 1 {
                export::run(&ready[0].conn, ready[0].repo_id, &destination, json, force)?;
                println!(
                    "exported {} to {}",
                    ready[0].target.root.display(),
                    destination.display()
                );
            } else {
                std::fs::create_dir_all(&destination)?;
                for target in &ready {
                    let child_destination = if json {
                        destination.join(format!("{}.json", target.target.label))
                    } else {
                        destination.join(&target.target.label)
                    };
                    export::run(
                        &target.conn,
                        target.repo_id,
                        &child_destination,
                        json,
                        force,
                    )?;
                    println!(
                        "[{}] exported {} to {}",
                        target.target.label,
                        target.target.root.display(),
                        child_destination.display()
                    );
                }
            }
        }

        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "graft", &mut std::io::stdout());
        }

        Cmd::Cache { command } => {
            if matches!(command, CacheCmd::Recover) {
                let backup = db::recover(&store)?;
                println!("preserved old store at {}", backup.display());
                println!("created clean store at {}", store.display());
                return Ok(());
            }
            let conn = db::open(&store)?;
            match command {
                CacheCmd::Prune => {
                    let removed = db::prune_missing(&conn)?;
                    println!("pruned {} missing repositorie(s)", removed.len());
                    for root in removed {
                        println!("  {root}");
                    }
                }
                CacheCmd::Doctor => {
                    let result = db::integrity(&conn)?;
                    println!("store integrity: {result}");
                    if result != "ok" {
                        std::process::exit(1);
                    }
                }
                CacheCmd::Recover => unreachable!(),
                CacheCmd::Reset { path } => {
                    for target in repo::targets(&path)? {
                        if db::reset_repo(&conn, &target.root)? {
                            println!("removed index for {}", target.root.display());
                        } else {
                            println!("no index stored for {}", target.root.display());
                        }
                    }
                }
            }
        }

        Cmd::Version { json } => {
            #[derive(Serialize)]
            struct VersionInfo {
                version: &'static str,
                build: &'static str,
                executable: String,
                schema_version: i64,
                extractor_stamp: &'static str,
                store: String,
            }
            let info = VersionInfo {
                version: env!("CARGO_PKG_VERSION"),
                build: option_env!("GRAFT_GIT_SHA").unwrap_or("source-build"),
                executable: std::env::current_exe()?.to_string_lossy().into_owned(),
                schema_version: db::SCHEMA_VERSION,
                extractor_stamp: index::EXTRACTOR_STAMP,
                store: store.to_string_lossy().into_owned(),
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                println!("graft {} ({})", info.version, info.build);
                println!("  executable  {}", info.executable);
                println!("  schema      {}", info.schema_version);
                println!("  extractor   {}", info.extractor_stamp);
                println!("  store       {}", info.store);
            }
        }

        Cmd::Upgrade => {
            println!("graft does not download or execute updates itself.");
            println!("Update through the same package or release installer used for this binary.");
            println!("Source install: git pull, then cargo install --path rust --locked --force");
        }

        Cmd::Viz {
            path,
            output,
            bind,
            allow_remote,
            force,
        } => {
            let root = repo::root_of(&path)?;
            let mut conn = db::open(&store)?;
            let Some(repo_id) = ready_repo(&mut conn, &root, no_refresh)? else {
                eprintln!("{}", not_indexed(&root));
                std::process::exit(2);
            };
            let title = root.file_name().unwrap_or_default().to_string_lossy();
            let html = viz::render(&conn, repo_id, &title)?;
            if let Some(output) = output {
                viz::write(&output, &html, force)?;
                println!("wrote {}", output.display());
            } else {
                viz::serve(&bind, html, allow_remote)?;
            }
        }

        Cmd::Status { path, json } => {
            let conn = db::open(&store)?;
            let mut outputs = Vec::new();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs() as i64);
            for target in repo::targets(&path)? {
                let Some(repo_id) = index::repo_id_of(&conn, &target.root)? else {
                    eprintln!("{}", not_indexed(&target.root));
                    std::process::exit(2);
                };
                let status = index::repo_status(&conn, repo_id)?;
                let age_seconds = now.saturating_sub(status.indexed_at);
                outputs.push(StatusOutput {
                    scope: target.label,
                    root: target.root.to_string_lossy().into_owned(),
                    status,
                    schema_version: db::SCHEMA_VERSION,
                    age_seconds,
                    store: store.to_string_lossy().into_owned(),
                    freshness: index::freshness(&conn, &target.root)?,
                });
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&outputs)?);
                return Ok(());
            }
            for output in outputs {
                println!("[{}/] {}", output.scope, output.root);
                println!("  files    {}", output.status.files);
                println!("  symbols  {}", output.status.symbols);
                println!("  edges    {}", output.status.edges);
                println!("  stamp    {}", output.status.extractor_stamp);
                println!("  schema   {}", output.schema_version);
                println!("  age      {}s", output.age_seconds);
                println!(
                    "  source   {}",
                    if output.freshness.is_clean() {
                        "current"
                    } else {
                        "stale"
                    }
                );
                println!("  store    {}", output.store);
            }
        }
    }
    Ok(())
}
