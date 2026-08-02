//! Deterministic, explicit exports from SQLite. Never writes into a repository
//! unless the caller names that destination.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
struct ExportSymbol {
    name: String,
    kind: String,
    start_line: i64,
    end_line: i64,
    signature: String,
    container: Option<String>,
}

#[derive(Serialize)]
struct ExportFile {
    path: String,
    symbols: Vec<ExportSymbol>,
}

pub fn run(
    db: &Connection,
    repo_id: i64,
    destination: &Path,
    json: bool,
    force: bool,
) -> Result<()> {
    let files = read_files(db, repo_id)?;
    if json {
        if destination.exists() && !force {
            bail!(
                "{} exists; pass --force to replace it",
                destination.display()
            );
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            destination,
            format!("{}\n", serde_json::to_string_pretty(&files)?),
        )?;
        return Ok(());
    }

    if destination.exists() && !destination.is_dir() {
        bail!("{} is not a directory", destination.display());
    }
    std::fs::create_dir_all(destination)?;
    let index_path = destination.join("INDEX.md");
    if index_path.exists() && !force {
        bail!(
            "{} exists; pass --force to replace the export",
            index_path.display()
        );
    }
    let cards = destination.join("files");
    if cards.exists() && !force {
        bail!(
            "{} exists; pass --force to replace the export",
            cards.display()
        );
    }
    if cards.exists() {
        std::fs::remove_dir_all(&cards)?;
    }
    std::fs::create_dir_all(&cards)?;
    let mut index = String::from("# Graft export\n\n");
    for (number, file) in files.iter().enumerate() {
        let card_name = format!("{:05}.md", number + 1);
        index.push_str(&format!("- [{}](files/{card_name})\n", file.path));
        let mut card = format!("# {}\n\n", file.path);
        for symbol in &file.symbols {
            let owner = symbol
                .container
                .as_deref()
                .map(|container| format!("{container}."))
                .unwrap_or_default();
            card.push_str(&format!(
                "- `L{}-L{}` {} `{}{}`: `{}`\n",
                symbol.start_line,
                symbol.end_line,
                symbol.kind,
                owner,
                symbol.name,
                symbol.signature.replace('`', "\\`")
            ));
        }
        std::fs::write(cards.join(card_name), card)?;
    }
    std::fs::write(index_path, index)?;
    Ok(())
}

fn read_files(db: &Connection, repo_id: i64) -> Result<Vec<ExportFile>> {
    let mut files_statement =
        db.prepare("select id, path from files where repo_id=?1 order by path")?;
    let file_rows = files_statement
        .query_map([repo_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut symbol_statement = db.prepare(
        "select name, kind, start_line, end_line, coalesce(signature,''), container
           from symbols where file_id=?1 and kind not in ('file','module')
          order by start_line, end_line desc, id",
    )?;
    file_rows
        .into_iter()
        .map(|(file_id, path)| {
            let symbols = symbol_statement
                .query_map([file_id], |row| {
                    Ok(ExportSymbol {
                        name: row.get(0)?,
                        kind: row.get(1)?,
                        start_line: row.get(2)?,
                        end_line: row.get(3)?,
                        signature: row.get(4)?,
                        container: row.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ExportFile { path, symbols })
        })
        .collect::<Result<Vec<_>>>()
        .context("read export rows")
}
