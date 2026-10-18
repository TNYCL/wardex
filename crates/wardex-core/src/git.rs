//! Small git helpers: repository state for summaries and worktree management.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::GitState;

const MAX_FILES: usize = 200;

async fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Branch and uncommitted changes of the repository at `cwd`, or `None` when
/// it is not a git repository.
pub async fn state(cwd: &Path) -> Option<GitState> {
    let status = git(cwd, &["status", "--porcelain=v1", "--branch"])
        .await
        .ok()?;
    let mut branch = None;
    let mut changed_files = Vec::new();
    for line in status.lines() {
        if let Some(b) = line.strip_prefix("## ") {
            let name = b.split("...").next().unwrap_or(b);
            branch = (!name.starts_with("HEAD (no branch)")).then(|| name.to_string());
        } else if line.len() > 3 && changed_files.len() < MAX_FILES {
            changed_files.push(line[3..].to_string());
        }
    }
    Some(GitState {
        branch,
        changed_files,
    })
}

/// Top-level directory of the repository containing `cwd`.
pub async fn repo_root(cwd: &Path) -> Option<PathBuf> {
    git(cwd, &["rev-parse", "--show-toplevel"])
        .await
        .ok()
        .map(|s| PathBuf::from(s.trim()))
}

/// Create a worktree at `<root>/<dir>/<name>`. With `branch`, check it out
/// (creating it if missing); otherwise git creates a branch named `name`.
/// Also adds `<dir>/` to the repository's local exclude file.
pub async fn add_worktree(
    root: &Path,
    dir: &str,
    name: &str,
    branch: Option<&str>,
) -> Result<PathBuf, String> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return Err(format!("invalid worktree name {name:?}"));
    }
    let path = root.join(dir).join(name);
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    let path_str = path.to_string_lossy().into_owned();
    match branch {
        Some(b) => {
            let exists = git(
                root,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{b}"),
                ],
            )
            .await
            .is_ok();
            if exists {
                git(root, &["worktree", "add", &path_str, b]).await?;
            } else {
                git(root, &["worktree", "add", "-b", b, &path_str]).await?;
            }
        }
        None => {
            git(root, &["worktree", "add", &path_str]).await?;
        }
    }
    exclude_dir(root, dir).await;
    Ok(path)
}

/// Remove a worktree. Fails (and leaves it in place) if it has changes.
pub async fn remove_worktree(root: &Path, path: &Path) -> Result<(), String> {
    git(root, &["worktree", "remove", &path.to_string_lossy()])
        .await
        .map(|_| ())
}

async fn exclude_dir(root: &Path, dir: &str) {
    let Ok(common) = git(root, &["rev-parse", "--git-common-dir"]).await else {
        return;
    };
    let common = PathBuf::from(common.trim());
    let common = if common.is_absolute() {
        common
    } else {
        root.join(common)
    };
    let file = common.join("info/exclude");
    let entry = format!("/{}/", dir.trim_matches('/'));
    let current = std::fs::read_to_string(&file).unwrap_or_default();
    if current.lines().any(|l| l.trim() == entry) {
        return;
    }
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut text = current;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&entry);
    text.push('\n');
    let _ = std::fs::write(&file, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn init_repo(dir: &Path) {
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
            vec!["commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            git(dir, &args).await.unwrap();
        }
    }

    #[tokio::test]
    async fn state_and_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(state(root).await.is_none());
        init_repo(root).await;
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let st = state(root).await.unwrap();
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.changed_files, vec!["a.txt".to_string()]);

        let wt = add_worktree(root, ".worktree", "feat", Some("feat-branch"))
            .await
            .unwrap();
        assert!(wt.join(".git").exists());
        assert_eq!(
            state(&wt).await.unwrap().branch.as_deref(),
            Some("feat-branch")
        );
        let exclude = std::fs::read_to_string(root.join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("/.worktree/"));
        // The worktree dir is excluded, so the main checkout only sees a.txt.
        assert_eq!(state(root).await.unwrap().changed_files.len(), 1);
        assert!(add_worktree(root, ".worktree", "feat", None).await.is_err());
        remove_worktree(root, &wt).await.unwrap();
        assert!(!wt.exists());
    }
}
