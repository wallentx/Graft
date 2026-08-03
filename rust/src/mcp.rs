//! Bounded newline-delimited JSON-RPC server with lazy repository indexing.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::Path;

use crate::{ask, db, index, repo};

const MAX_MESSAGE: usize = 1024 * 1024;
const MAX_OUTPUT: usize = 1024 * 1024;

pub fn serve(store: &Path, start: &Path, no_refresh: bool) -> Result<()> {
    let targets = repo::targets(start)?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.len() > MAX_MESSAGE {
            write_response(&mut stdout, error(Value::Null, -32600, "request too large"))?;
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                write_response(&mut stdout, error(Value::Null, -32700, "parse error"))?;
                continue;
            }
        };
        let id = request.get("id").cloned();
        if id.is_none() {
            continue;
        }
        let id = id.unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let response = if !matches!(method, "initialize" | "ping" | "tools/list" | "tools/call") {
            error(id, -32601, &format!("method not found: {method}"))
        } else {
            match dispatch(store, &targets, method, &params, no_refresh) {
                Ok(result) => json!({"jsonrpc":"2.0", "id":id, "result":result}),
                Err(problem) => error(id, -32000, &problem.to_string()),
            }
        };
        write_response(&mut stdout, response)?;
    }
    Ok(())
}

fn write_response(output: &mut impl Write, response: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&response)?;
    if bytes.len() > MAX_OUTPUT {
        bytes = serde_json::to_vec(&error(
            response.get("id").cloned().unwrap_or(Value::Null),
            -32001,
            "response too large; narrow the query",
        ))?;
    }
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code, "message":message}})
}

fn dispatch(
    store: &Path,
    targets: &[repo::Target],
    method: &str,
    params: &Value,
    no_refresh: bool,
) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion":"2024-11-05",
            "capabilities":{"tools":{"listChanged":false}},
            "serverInfo":{"name":"graft", "version":env!("CARGO_PKG_VERSION")},
            "instructions":"Use find for ranked context, grep for exhaustive matches, and callers for graph traversal. Graft automatically indexes new repositories and refreshes changed source; freshness is an observational state check."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_schemas()})),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .context("missing tool name")?;
            let args = params.get("arguments").unwrap_or(&Value::Null);
            let data = call_tool(store, targets, name, args, no_refresh)?;
            Ok(json!({
                "content":[{"type":"text", "text":serde_json::to_string_pretty(&data)?}],
                "structuredContent":data,
                "isError":false
            }))
        }
        _ => unreachable!("method validated by serve"),
    }
}

fn tool_schemas() -> Vec<Value> {
    vec![
        schema("find", "Ranked code-context search", &["query"]),
        schema("grep", "Exhaustive source regex search", &["pattern"]),
        schema(
            "callers",
            "Incoming or outgoing graph traversal",
            &["symbol"],
        ),
        schema("skeleton", "Definitions in one file", &["file"]),
        schema("map", "Repository orientation map", &[]),
        schema("status", "Index counts and freshness", &[]),
        schema("freshness", "Compare indexed and live source", &[]),
    ]
}

fn schema(name: &str, description: &str, required: &[&str]) -> Value {
    let properties = match name {
        "find" => {
            json!({"query":{"type":"string"}, "limit":{"type":"integer"}, "in":{"type":"string"}, "source":{"type":"boolean"}, "repo":{"type":"string"}})
        }
        "grep" => {
            json!({"pattern":{"type":"string"}, "fixed":{"type":"boolean"}, "ignoreCase":{"type":"boolean"}, "in":{"type":"string"}, "repo":{"type":"string"}})
        }
        "callers" => {
            json!({"symbol":{"type":"string"}, "direction":{"enum":["in","out"]}, "depth":{"type":"integer"}, "in":{"type":"string"}, "repo":{"type":"string"}})
        }
        "skeleton" => json!({"file":{"type":"string"}, "repo":{"type":"string"}}),
        _ => json!({"repo":{"type":"string"}}),
    };
    json!({
        "name":name,
        "description":description,
        "inputSchema":{"type":"object", "properties":properties, "required":required, "additionalProperties":false}
    })
}

fn call_tool(
    store: &Path,
    targets: &[repo::Target],
    name: &str,
    args: &Value,
    no_refresh: bool,
) -> Result<Value> {
    match name {
        "find" => {
            string_arg(args, "query")?;
        }
        "grep" => {
            string_arg(args, "pattern")?;
        }
        "callers" => {
            string_arg(args, "symbol")?;
        }
        "skeleton" => {
            string_arg(args, "file")?;
        }
        "map" | "status" | "freshness" => {}
        _ => return Err(anyhow!("unknown tool: {name}")),
    }
    let selected = select_targets(targets, args.get("repo").and_then(Value::as_str))?;
    let mut results = serde_json::Map::new();
    for target in selected {
        let mut conn = db::open(store)?;
        let mut state = index::freshness(&conn, &target.root)?;
        if name == "freshness" {
            results.insert(target.label.clone(), serde_json::to_value(state)?);
            continue;
        }
        if !state.indexed {
            if no_refresh {
                return Err(anyhow!(
                    "{} has not been indexed by graft and automatic indexing is disabled",
                    target.root.display()
                ));
            }
            index::build(&mut conn, &target.root)?;
            if name == "status" {
                state = index::freshness(&conn, &target.root)?;
            }
        } else if !state.is_clean() && !no_refresh {
            index::build(&mut conn, &target.root)?;
            if name == "status" {
                state = index::freshness(&conn, &target.root)?;
            }
        }
        let repo_id = index::repo_id_of(&conn, &target.root)?.context("current index missing")?;
        let value = match name {
            "find" => serde_json::to_value(ask::ask(
                &conn,
                repo_id,
                &target.root,
                string_arg(args, "query")?,
                ask::AskOptions {
                    limit: usize_arg(args, "limit", 8),
                    scope: args.get("in").and_then(Value::as_str),
                    source: bool_arg(args, "source"),
                    full: false,
                },
            )?)?,
            "grep" => serde_json::to_value(index::grep_with_options(
                &conn,
                repo_id,
                &target.root,
                string_arg(args, "pattern")?,
                index::GrepOptions {
                    ignore_case: bool_arg_named(args, "ignoreCase"),
                    fixed: bool_arg(args, "fixed"),
                    scope: args.get("in").and_then(Value::as_str),
                },
            )?)?,
            "callers" => {
                let (seeds, reached) = index::callers_scoped(
                    &conn,
                    repo_id,
                    string_arg(args, "symbol")?,
                    args.get("direction").and_then(Value::as_str) == Some("out"),
                    usize_arg(args, "depth", 1).min(32),
                    args.get("in").and_then(Value::as_str),
                )?;
                json!({"seeds":seeds, "reached":reached})
            }
            "skeleton" => {
                let requested = string_arg(args, "file")?;
                let rel = index::skeleton_path(&conn, repo_id, requested)?
                    .with_context(|| format!("no unique indexed file matching {requested:?}"))?;
                json!({"path":rel, "symbols":index::skeleton(&conn, repo_id, &rel)?})
            }
            "map" => serde_json::to_value(index::repo_map(&conn, repo_id, 12)?)?,
            "status" => json!({
                "root":target.root,
                "store":index::repo_status(&conn, repo_id)?,
                "freshness":state,
            }),
            _ => unreachable!("tool name validated before indexing"),
        };
        results.insert(target.label.clone(), value);
    }
    Ok(Value::Object(results))
}

fn select_targets<'a>(
    targets: &'a [repo::Target],
    label: Option<&str>,
) -> Result<Vec<&'a repo::Target>> {
    match label {
        None => Ok(targets.iter().collect()),
        Some(label) => targets
            .iter()
            .find(|target| target.label == label)
            .map(|target| vec![target])
            .with_context(|| format!("unknown workspace repo {label:?}")),
    }
}

fn string_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument {name:?}"))
}

fn usize_arg(args: &Value, name: &str, default: usize) -> usize {
    args.get(name)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
}

fn bool_arg(args: &Value, name: &str) -> bool {
    bool_arg_named(args, name)
}

fn bool_arg_named(args: &Value, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "graft-mcp-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(name: &str) -> (TempDir, std::path::PathBuf, repo::Target) {
        let temp = TempDir::new(name);
        let root = temp.0.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("lib.rs"), "pub fn original() {}\n").unwrap();
        let store = temp.0.join("graft.db");
        let target = repo::Target {
            label: "repo".to_string(),
            root,
        };
        (temp, store, target)
    }

    #[test]
    fn tool_list_has_unique_names_and_object_schemas() {
        let tools = tool_schemas();
        let names: std::collections::HashSet<_> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), tools.len());
        assert!(
            tools
                .iter()
                .all(|tool| tool["inputSchema"]["type"] == "object")
        );
    }

    #[test]
    fn freshness_is_observational_but_other_tools_build_and_refresh() {
        let (_temp, store, target) = fixture("lazy-index");
        let targets = [target.clone()];

        let first = call_tool(&store, &targets, "freshness", &json!({}), false).unwrap();
        assert_eq!(first["repo"]["indexed"], false);

        let status = call_tool(&store, &targets, "status", &json!({}), false).unwrap();
        assert_eq!(status["repo"]["freshness"]["indexed"], true);
        assert!(
            status["repo"]["freshness"]["added"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        std::fs::write(
            target.root.join("lib.rs"),
            "pub fn original() {}\npub fn added() {}\n",
        )
        .unwrap();
        let stale = call_tool(&store, &targets, "freshness", &json!({}), false).unwrap();
        assert_eq!(stale["repo"]["modified"], json!(["lib.rs"]));

        let refreshed = call_tool(&store, &targets, "status", &json!({}), false).unwrap();
        assert!(
            refreshed["repo"]["freshness"]["modified"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let conn = db::open(&store).unwrap();
        let repo_id = index::repo_id_of(&conn, &target.root).unwrap().unwrap();
        let matches = index::grep(&conn, repo_id, &target.root, "added").unwrap();
        assert!(!matches.groups.is_empty());
    }

    #[test]
    fn no_refresh_does_not_create_a_missing_index() {
        let (_temp, store, target) = fixture("no-refresh");
        let error = call_tool(
            &store,
            std::slice::from_ref(&target),
            "status",
            &json!({}),
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("automatic indexing is disabled"));
        let conn = db::open(&store).unwrap();
        assert!(!index::freshness(&conn, &target.root).unwrap().indexed);
    }

    #[test]
    fn invalid_tool_call_does_not_create_an_index() {
        let (_temp, store, target) = fixture("invalid-call");
        let error = call_tool(
            &store,
            std::slice::from_ref(&target),
            "unknown",
            &json!({}),
            false,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(error, "unknown tool: unknown");
        let conn = db::open(&store).unwrap();
        assert!(!index::freshness(&conn, &target.root).unwrap().indexed);
    }
}
