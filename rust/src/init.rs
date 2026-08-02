//! User-level host registration. Machine-specific executable paths belong only
//! in user configuration, never in repository files.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize)]
pub struct PlannedWrite {
    pub host: String,
    pub path: String,
    pub format: &'static str,
    pub action: &'static str,
}

pub fn run(hosts: &[String], dry_run: bool) -> Result<Vec<PlannedWrite>> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let home = PathBuf::from(home);
    let executable = std::env::current_exe()?.canonicalize()?;
    let mut writes = Vec::new();
    for host in hosts {
        let (path, format, top_key) = target(&home, host)?;
        let action = if path.exists() { "update" } else { "create" };
        writes.push(PlannedWrite {
            host: host.clone(),
            path: path.to_string_lossy().into_owned(),
            format,
            action,
        });
        if dry_run {
            continue;
        }
        match format {
            "toml" => upsert_codex(&path, &executable)?,
            "opencode-json" => merge_json(
                &path,
                top_key,
                json!({"type":"local", "command":[executable, "mcp"], "enabled":true}),
            )?,
            "json" => merge_json(
                &path,
                top_key,
                json!({"command":executable, "args":["mcp"]}),
            )?,
            _ => unreachable!(),
        }
    }
    Ok(writes)
}

fn target(home: &Path, host: &str) -> Result<(PathBuf, &'static str, &'static str)> {
    let target = match host {
        "claude" => (home.join(".claude.json"), "json", "mcpServers"),
        "codex" => (home.join(".codex/config.toml"), "toml", ""),
        "cursor" => (home.join(".cursor/mcp.json"), "json", "mcpServers"),
        "gemini" => (home.join(".gemini/settings.json"), "json", "mcpServers"),
        "antigravity" => (
            home.join(".gemini/config/mcp_config.json"),
            "json",
            "mcpServers",
        ),
        "opencode" => (
            home.join(".config/opencode/opencode.json"),
            "opencode-json",
            "mcp",
        ),
        "copilot" => (home.join(".copilot/mcp-config.json"), "json", "mcpServers"),
        _ => bail!(
            "unsupported host {host:?}; expected claude, codex, cursor, gemini, antigravity, opencode, or copilot"
        ),
    };
    Ok(target)
}

fn merge_json(path: &Path, top_key: &str, entry: Value) -> Result<()> {
    let mut root = if path.exists() {
        serde_json::from_str::<Value>(&std::fs::read_to_string(path)?)
            .with_context(|| format!("refusing to rewrite invalid JSON in {}", path.display()))?
    } else {
        json!({})
    };
    let object = root
        .as_object_mut()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let bucket = object.entry(top_key).or_insert_with(|| json!({}));
    let bucket = bucket
        .as_object_mut()
        .with_context(|| format!("{top_key} in {} must be an object", path.display()))?;
    bucket.insert("graft".to_string(), entry);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private_atomic(
        path,
        format!("{}\n", serde_json::to_string_pretty(&root)?).as_bytes(),
    )?;
    Ok(())
}

fn upsert_codex(path: &Path, executable: &Path) -> Result<()> {
    let mut text = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let section = format!(
        "[mcp_servers.graft]\ncommand = {}\nargs = [\"mcp\"]\n",
        serde_json::to_string(&executable.to_string_lossy())?
    );
    if let Some(start) = text.find("[mcp_servers.graft]") {
        let after_header = start + "[mcp_servers.graft]".len();
        let end = text[after_header..]
            .find("\n[")
            .map_or(text.len(), |offset| after_header + offset + 1);
        text.replace_range(start..end, &section);
        write_private_atomic(path, text.as_bytes())?;
        return Ok(());
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&section);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private_atomic(path, text.as_bytes())?;
    Ok(())
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("configuration path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".graft-init-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = path
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        options.mode(mode);
    }
    let write_result = (|| -> Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_supported_hosts_use_user_level_paths() {
        let home = Path::new("/home/tester");
        for host in [
            "claude",
            "codex",
            "cursor",
            "gemini",
            "antigravity",
            "opencode",
            "copilot",
        ] {
            let (path, _, _) = target(home, host).unwrap();
            assert!(path.starts_with(home));
        }
    }

    #[test]
    fn json_merge_preserves_foreign_servers() {
        let base = std::env::temp_dir().join(format!("graft-init-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = base.join("mcp.json");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&path, r#"{"mcpServers":{"other":{"command":"other"}}}"#).unwrap();
        merge_json(
            &path,
            "mcpServers",
            json!({"command":"/safe/graft", "args":["mcp"]}),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["other"]["command"], "other");
        assert_eq!(value["mcpServers"]["graft"]["command"], "/safe/graft");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn codex_registration_updates_only_its_existing_section() {
        let base =
            std::env::temp_dir().join(format!("graft-init-toml-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("config.toml");
        std::fs::write(
            &path,
            "model = \"keep\"\n\n[mcp_servers.graft]\ncommand = \"/old/graft\"\nargs = [\"mcp\"]\n\n[mcp_servers.other]\ncommand = \"other\"\n",
        )
        .unwrap();
        upsert_codex(&path, Path::new("/new/graft")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("model = \"keep\""));
        assert!(text.contains("command = \"/new/graft\""));
        assert!(text.contains("[mcp_servers.other]\ncommand = \"other\""));
        assert!(!text.contains("/old/graft"));
        let _ = std::fs::remove_dir_all(base);
    }
}
