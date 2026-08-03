//! Repo identity and the source-file walk.
//!
//! A repo is identified by the realpath of its git toplevel. That is the key in
//! `repos.root`, and it is what the MCP server resolves the working directory to
//! before deciding whether it can answer or has to report "not indexed". Using
//! the toplevel rather than the caller's cwd means `graft grep` from a
//! subdirectory hits the same store entry as one run from the root.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Target {
    pub label: String,
    pub root: PathBuf,
}

/// Languages the extractor understands, chosen by file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    TypeScript,
    Tsx,
    JavaScript,
    Rust,
    Python,
    Go,
}

impl Lang {
    pub fn of_path(p: &Path) -> Option<Lang> {
        match p.extension()?.to_str()? {
            // .d.ts carries no bodies and no call edges — only re-declarations of
            // symbols that already exist elsewhere. Indexing it produces duplicate
            // names that outrank the real definition.
            "ts" if !p.to_string_lossy().ends_with(".d.ts") => Some(Lang::TypeScript),
            "tsx" => Some(Lang::Tsx),
            "js" | "mjs" | "cjs" | "jsx" => Some(Lang::JavaScript),
            "rs" => Some(Lang::Rust),
            "py" => Some(Lang::Python),
            "go" => Some(Lang::Go),
            _ => None,
        }
    }
}

/// The realpath of the git toplevel containing `start`.
///
/// Falls back to the realpath of `start` itself when git is absent or the path is
/// not in a work tree, so graft still indexes a plain directory — it just cannot
/// share an entry between worktrees of the same repo.
pub fn root_of(start: &Path) -> Result<PathBuf> {
    if let Some(root) = git_toplevel(start) {
        return Ok(root);
    }
    std::fs::canonicalize(start).with_context(|| format!("canonicalize {}", start.display()))
}

fn git_dir(root: &Path) -> Option<PathBuf> {
    let marker = root.join(".git");
    if marker.is_dir() {
        return std::fs::canonicalize(marker).ok();
    }
    if !marker.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(marker).ok()?;
    let path = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    std::fs::canonicalize(path).ok()
}

fn git_toplevel(start: &Path) -> Option<PathBuf> {
    let mut current = std::fs::canonicalize(start).ok()?;
    if current.is_file() {
        current.pop();
    }
    loop {
        if git_dir(&current).is_some() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// A git repository, or an immediate workspace containing at least two repos.
pub fn targets(start: &Path) -> Result<Vec<Target>> {
    let start = std::fs::canonicalize(start)
        .with_context(|| format!("canonicalize {}", start.display()))?;
    if let Some(root) = git_toplevel(&start) {
        return Ok(vec![Target {
            label: root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            root,
        }]);
    }

    let mut children = Vec::new();
    for entry in std::fs::read_dir(&start).with_context(|| format!("read {}", start.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let child = std::fs::canonicalize(entry.path())?;
        if git_toplevel(&child).as_deref() == Some(child.as_path()) {
            children.push(Target {
                label: entry.file_name().to_string_lossy().into_owned(),
                root: child,
            });
        }
    }
    children.sort_by(|left, right| left.label.cmp(&right.label));
    if children.len() >= 2 {
        Ok(children)
    } else {
        Ok(vec![Target {
            label: start
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            root: start,
        }])
    }
}

/// `git rev-parse --git-common-dir`, which is shared by every worktree of a repo.
/// Stored so a future change can decide whether worktrees share one graph.
pub fn git_common_dir(root: &Path) -> Option<String> {
    let git_dir = git_dir(root)?;
    let common = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .and_then(|text| {
            let path = PathBuf::from(text.trim());
            let path = if path.is_absolute() {
                path
            } else {
                git_dir.join(path)
            };
            std::fs::canonicalize(path).ok()
        })
        .unwrap_or(git_dir);
    Some(common.to_string_lossy().into_owned())
}

pub struct SourceFile {
    pub rel: String,
    pub lang: Lang,
    pub text: String,
    pub mtime: i64,
    pub size: i64,
    pub hash: String,
}

/// FNV-1a. Deliberately not `DefaultHasher`: SipHash's output is explicitly not
/// stable across Rust releases, so a compiler upgrade would silently invalidate
/// every cached file hash and force a full cold reparse of every indexed repo.
fn fnv1a(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Walk `root` for source files graft can extract, honouring .gitignore.
///
/// The `ignore` crate is ripgrep's walker, so "what graft indexes" and "what rg
/// searches" agree by construction — a file the user cannot grep will not quietly
/// show up in graft's results either.
pub fn walk(root: &Path) -> Result<Vec<SourceFile>> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .parents(true)
        .build();

    for dent in walker {
        let dent = match dent {
            Ok(d) => d,
            Err(_) => continue, // unreadable entry: skip, never abort the build
        };
        if !dent.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let abs = dent.path();
        let Some(lang) = Lang::of_path(abs) else {
            continue;
        };
        // Non-UTF8 files are not source we can parse; skipping beats failing.
        let Ok(text) = std::fs::read_to_string(abs) else {
            continue;
        };
        let meta = dent.metadata().ok();
        let mtime = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let rel = abs
            .strip_prefix(root)
            .unwrap_or(abs)
            .to_string_lossy()
            .replace('\\', "/");
        out.push(SourceFile {
            rel,
            lang,
            hash: fnv1a(text.as_bytes()),
            size: text.len() as i64,
            mtime,
            text,
        });
    }
    // Stable order so two builds of an unchanged tree produce identical rowids,
    // which is what lets the differential harness diff output byte for byte.
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn lang_of_path_skips_declaration_files() {
        assert_eq!(Lang::of_path(Path::new("a/b.ts")), Some(Lang::TypeScript));
        assert_eq!(Lang::of_path(Path::new("a/b.tsx")), Some(Lang::Tsx));
        assert_eq!(
            Lang::of_path(Path::new("scripts/build.mjs")),
            Some(Lang::JavaScript)
        );
        assert_eq!(Lang::of_path(Path::new("src/main.rs")), Some(Lang::Rust));
        assert_eq!(
            Lang::of_path(Path::new("a/b.d.ts")),
            None,
            ".d.ts has no bodies to index"
        );
        assert_eq!(Lang::of_path(Path::new("README.md")), None);
    }

    #[test]
    fn fnv_is_stable_and_distinguishes_content() {
        // Pinned literals, computed independently rather than recorded from this
        // implementation's own output. If they ever change, every stored file hash
        // is invalidated and every indexed repo cold-reparses — that should be a
        // deliberate schema bump, not an accident.
        assert_eq!(
            fnv1a(b""),
            "cbf29ce484222325",
            "the FNV-1a 64-bit offset basis"
        );
        assert_eq!(fnv1a(b"graft"), "32a1802977be3e45");
        assert_ne!(fnv1a(b"graft"), fnv1a(b"graftx"));
        assert_eq!(
            fnv1a(b"graft").len(),
            16,
            "zero-padded, so hashes sort as text"
        );
    }

    #[test]
    fn workspace_requires_two_immediate_git_children() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "graft-workspace-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for child in ["alpha", "beta"] {
            let path = root.join(child);
            std::fs::create_dir_all(&path).unwrap();
            assert!(
                Command::new("git")
                    .arg("init")
                    .arg("-q")
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let found = targets(&root).unwrap();
        assert_eq!(
            found
                .iter()
                .map(|target| target.label.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn native_git_discovery_finds_a_parent_without_spawning_git() {
        let root = std::env::temp_dir().join(format!("graft-git-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();

        assert_eq!(root_of(&root.join("src/nested")).unwrap(), root);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn native_git_discovery_resolves_linked_worktree_common_dir() {
        let base = std::env::temp_dir().join(format!("graft-worktree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let common = base.join("common");
        let git_dir = common.join("worktrees/linked");
        let worktree = base.join("linked");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();

        assert_eq!(root_of(&worktree.join("src")).unwrap(), worktree);
        assert_eq!(
            git_common_dir(&worktree).unwrap(),
            std::fs::canonicalize(&common).unwrap().to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(base);
    }
}
