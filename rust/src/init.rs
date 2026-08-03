//! User-level provider registration. Machine-specific executable paths belong
//! only in user configuration, never in repository files.

use anyhow::{Context, Result, bail};
use dialoguer::{MultiSelect, console::Term, theme::SimpleTheme};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

struct Provider {
    id: &'static str,
    name: &'static str,
    config: &'static str,
    detection: &'static str,
    format: &'static str,
    top_key: &'static str,
}

const PROVIDERS: &[Provider] = &[
    Provider {
        id: "claude",
        name: "Claude Code",
        config: ".claude.json",
        detection: ".claude",
        format: "json",
        top_key: "mcpServers",
    },
    Provider {
        id: "codex",
        name: "Codex",
        config: ".codex/config.toml",
        detection: ".codex",
        format: "toml",
        top_key: "",
    },
    Provider {
        id: "cursor",
        name: "Cursor",
        config: ".cursor/mcp.json",
        detection: ".cursor",
        format: "json",
        top_key: "mcpServers",
    },
    Provider {
        id: "gemini",
        name: "Gemini CLI",
        config: ".gemini/settings.json",
        detection: ".gemini/settings.json",
        format: "json",
        top_key: "mcpServers",
    },
    Provider {
        id: "antigravity",
        name: "Antigravity",
        config: ".gemini/config/mcp_config.json",
        detection: ".gemini/config",
        format: "json",
        top_key: "mcpServers",
    },
    Provider {
        id: "opencode",
        name: "OpenCode",
        config: ".config/opencode/opencode.json",
        detection: ".config/opencode",
        format: "opencode-json",
        top_key: "mcp",
    },
    Provider {
        id: "copilot",
        name: "GitHub Copilot CLI",
        config: ".copilot/mcp-config.json",
        detection: ".copilot",
        format: "json",
        top_key: "mcpServers",
    },
];

#[derive(Debug, Serialize)]
pub struct PlannedWrite {
    pub provider: String,
    pub path: String,
    pub format: &'static str,
    pub action: &'static str,
}

#[derive(Clone, Copy)]
struct Registration {
    present: bool,
    current: bool,
}

/// Show the interactive checkbox picker used by plain `graft init`.
pub fn select_providers() -> Result<Vec<String>> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let home = PathBuf::from(home);
    let executable = std::env::current_exe()?.canonicalize()?;
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(
            "graft init needs a terminal for provider selection; pass one or more --provider <id> values in scripts"
        );
    }

    let states: Vec<Registration> = PROVIDERS
        .iter()
        .map(|provider| registration(&home, provider, &executable))
        .collect::<Result<_>>()?;
    let items: Vec<String> = PROVIDERS
        .iter()
        .zip(&states)
        .map(|(provider, state)| {
            let detected = home.join(provider.detection).exists();
            format!(
                "{:<20}  ~/{:<34}{}",
                provider.name,
                provider.config,
                if state.present {
                    " registered"
                } else if detected {
                    " detected"
                } else {
                    ""
                }
            )
        })
        .collect();
    let defaults: Vec<bool> = states.iter().map(|state| state.present).collect();
    let selections = MultiSelect::with_theme(&SimpleTheme)
        .with_prompt("Select providers (space toggles, enter confirms)")
        .items(&items)
        .defaults(&defaults)
        .report(false)
        .interact_on(&Term::stderr())
        .context("provider selection failed")?;
    Ok(selections
        .into_iter()
        .map(|index| PROVIDERS[index].id.to_string())
        .collect())
}

/// Reconcile every provider with an interactive checkbox selection.
pub fn reconcile(providers: &[String], dry_run: bool) -> Result<Vec<PlannedWrite>> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let home = PathBuf::from(home);
    let executable = std::env::current_exe()?.canonicalize()?;
    reconcile_at(&home, &executable, providers, dry_run)
}

/// Add or refresh explicitly named registrations without touching other providers.
pub fn register(providers: &[String], dry_run: bool) -> Result<Vec<PlannedWrite>> {
    change_named(providers, dry_run, true)
}

/// Remove explicitly named registrations without touching other providers.
pub fn deregister(providers: &[String], dry_run: bool) -> Result<Vec<PlannedWrite>> {
    change_named(providers, dry_run, false)
}

fn reconcile_at(
    home: &Path,
    executable: &Path,
    selected: &[String],
    dry_run: bool,
) -> Result<Vec<PlannedWrite>> {
    let selected: HashSet<&str> = selected.iter().map(String::as_str).collect();
    for id in &selected {
        provider(id)?;
    }
    let mut writes = Vec::new();
    for provider in PROVIDERS {
        let state = registration(home, provider, executable)?;
        let checked = selected.contains(provider.id);
        let action = match (checked, state.present, state.current) {
            (true, false, _) => Some("register"),
            (true, true, false) => Some("update"),
            (false, true, _) => Some("deregister"),
            _ => None,
        };
        if let Some(action) = action {
            change_provider(home, executable, provider, action, dry_run, &mut writes)?;
        }
    }
    Ok(writes)
}

fn change_named(providers: &[String], dry_run: bool, add: bool) -> Result<Vec<PlannedWrite>> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let home = PathBuf::from(home);
    let executable = std::env::current_exe()?.canonicalize()?;
    let mut writes = Vec::new();
    let mut seen = HashSet::new();
    for id in providers {
        if !seen.insert(id.as_str()) {
            continue;
        }
        let provider = provider(id)?;
        let state = registration(&home, provider, &executable)?;
        let action = if add {
            if state.current {
                continue;
            }
            if state.present { "update" } else { "register" }
        } else {
            if !state.present {
                continue;
            }
            "deregister"
        };
        change_provider(&home, &executable, provider, action, dry_run, &mut writes)?;
    }
    Ok(writes)
}

fn change_provider(
    home: &Path,
    executable: &Path,
    provider: &Provider,
    action: &'static str,
    dry_run: bool,
    writes: &mut Vec<PlannedWrite>,
) -> Result<()> {
    let path = home.join(provider.config);
    writes.push(PlannedWrite {
        provider: provider.id.to_string(),
        path: path.to_string_lossy().into_owned(),
        format: provider.format,
        action,
    });
    if dry_run {
        return Ok(());
    }
    if action == "deregister" {
        match provider.format {
            "toml" => remove_codex(&path)?,
            "json" | "opencode-json" => remove_json(&path, provider.top_key)?,
            _ => unreachable!(),
        }
    } else {
        match provider.format {
            "toml" => upsert_codex(&path, executable)?,
            "opencode-json" => merge_json(
                &path,
                provider.top_key,
                json!({"type":"local", "command":[executable, "mcp"], "enabled":true}),
            )?,
            "json" => merge_json(
                &path,
                provider.top_key,
                json!({"command":executable, "args":["mcp"]}),
            )?,
            _ => unreachable!(),
        }
    }
    Ok(())
}

fn provider(id: &str) -> Result<&'static Provider> {
    PROVIDERS
        .iter()
        .find(|provider| provider.id == id)
        .with_context(|| {
            format!(
                "unsupported provider {id:?}; expected {}",
                PROVIDERS
                    .iter()
                    .map(|provider| provider.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn registration(home: &Path, provider: &Provider, executable: &Path) -> Result<Registration> {
    let path = home.join(provider.config);
    if !path.exists() {
        return Ok(Registration {
            present: false,
            current: false,
        });
    }
    if provider.format == "toml" {
        let text = std::fs::read_to_string(&path)?;
        let Some((start, end)) = codex_section_range(&text) else {
            return Ok(Registration {
                present: false,
                current: false,
            });
        };
        let section = &text[start..end];
        let command = format!(
            "command = {}",
            serde_json::to_string(&executable.to_string_lossy())?
        );
        return Ok(Registration {
            present: true,
            current: section.lines().any(|line| line.trim() == command)
                && section
                    .lines()
                    .any(|line| line.trim() == "args = [\"mcp\"]"),
        });
    }

    let root = serde_json::from_str::<Value>(&std::fs::read_to_string(&path)?)
        .with_context(|| format!("refusing to read invalid JSON in {}", path.display()))?;
    let Some(bucket) = root.get(provider.top_key) else {
        return Ok(Registration {
            present: false,
            current: false,
        });
    };
    let bucket = bucket.as_object().with_context(|| {
        format!(
            "{} in {} must be an object",
            provider.top_key,
            path.display()
        )
    })?;
    let Some(entry) = bucket.get("graft") else {
        return Ok(Registration {
            present: false,
            current: false,
        });
    };
    let desired = if provider.format == "opencode-json" {
        json!({"type":"local", "command":[executable, "mcp"], "enabled":true})
    } else {
        json!({"command":executable, "args":["mcp"]})
    };
    Ok(Registration {
        present: true,
        current: entry == &desired,
    })
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

fn remove_json(path: &Path, top_key: &str) -> Result<()> {
    let mut root = serde_json::from_str::<Value>(&std::fs::read_to_string(path)?)
        .with_context(|| format!("refusing to rewrite invalid JSON in {}", path.display()))?;
    let object = root
        .as_object_mut()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let bucket = object
        .get_mut(top_key)
        .with_context(|| format!("{top_key} is missing from {}", path.display()))?
        .as_object_mut()
        .with_context(|| format!("{top_key} in {} must be an object", path.display()))?;
    bucket.remove("graft");
    write_private_atomic(
        path,
        format!("{}\n", serde_json::to_string_pretty(&root)?).as_bytes(),
    )?;
    Ok(())
}

fn codex_section_range(text: &str) -> Option<(usize, usize)> {
    let mut start = None;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.split('#').next().unwrap_or(line).trim();
        if start.is_none() {
            if trimmed == "[mcp_servers.graft]" {
                start = Some(offset);
            }
        } else if trimmed.starts_with('[') && trimmed.ends_with(']') {
            return Some((start?, offset));
        }
        offset += line.len();
    }
    start.map(|start| (start, text.len()))
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
    if let Some((start, end)) = codex_section_range(&text) {
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

fn remove_codex(path: &Path) -> Result<()> {
    let mut text = std::fs::read_to_string(path)?;
    if let Some((start, end)) = codex_section_range(&text) {
        text.replace_range(start..end, "");
        write_private_atomic(path, text.as_bytes())?;
    }
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
    fn all_supported_providers_use_user_level_paths() {
        let home = Path::new("/home/tester");
        for provider in PROVIDERS {
            let path = home.join(provider.config);
            assert!(path.starts_with(home));
        }
    }

    #[test]
    fn duplicate_provider_ids_are_applied_once() {
        let home = std::env::temp_dir().join(format!("graft-init-dedupe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let providers = ["codex".to_string(), "codex".to_string()];
        let writes = reconcile_at(&home, Path::new("/safe/graft"), &providers, true).unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].provider, "codex");
        let _ = std::fs::remove_dir_all(home);
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
    fn json_deregistration_preserves_foreign_servers() {
        let base =
            std::env::temp_dir().join(format!("graft-init-remove-json-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = base.join(".claude.json");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            &path,
            r#"{"mcpServers":{"other":{"command":"other"},"graft":{"command":"/safe/graft","args":["mcp"]}}}"#,
        )
        .unwrap();

        let writes = reconcile_at(&base, Path::new("/safe/graft"), &[], false).unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].action, "deregister");
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["other"]["command"], "other");
        assert!(value["mcpServers"].get("graft").is_none());
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

    #[test]
    fn codex_deregistration_removes_only_its_section() {
        let base =
            std::env::temp_dir().join(format!("graft-init-remove-toml-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = base.join(".codex/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "model = \"keep\"\n\n[mcp_servers.graft] # managed registration\ncommand = \"/safe/graft\"\nargs = [\"mcp\"]\n\n[mcp_servers.other]\ncommand = \"other\"\n",
        )
        .unwrap();
        let writes = reconcile_at(&base, Path::new("/safe/graft"), &[], false).unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].action, "deregister");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("model = \"keep\""));
        assert!(text.contains("[mcp_servers.other]\ncommand = \"other\""));
        assert!(!text.contains("[mcp_servers.graft]"));
        assert!(!text.contains("/safe/graft"));
        let _ = std::fs::remove_dir_all(base);
    }
}
