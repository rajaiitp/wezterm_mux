use crate::types::ProjectId;
use anyhow::{bail, Context};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// Return the platform data directory used for managed development worktrees.
pub fn default_managed_worktree_root() -> anyhow::Result<PathBuf> {
    let base = dirs_next::data_local_dir()
        .or_else(dirs_next::home_dir)
        .context("could not determine a user data directory")?;
    Ok(base.join("wezterm").join("worktrees"))
}

/// Construct a readable but collision-resistant managed worktree path.
pub fn managed_worktree_path(
    root: &Path,
    project_id: &ProjectId,
    branch: &str,
) -> anyhow::Result<PathBuf> {
    if branch.is_empty() {
        bail!("branch name cannot be empty");
    }

    let slug = branch_slug(branch);
    let mut hasher = Sha256::new();
    hasher.update(b"wezterm-worktree-path-v1\0");
    hasher.update(branch.as_bytes());
    let digest = hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let project = project_id
        .0
        .strip_prefix("project:")
        .unwrap_or(&project_id.0);

    Ok(root.join(project).join(format!("{slug}-{digest}")))
}

/// Verify that `path` is a child of the managed root.
///
/// Existing paths are canonicalized to account for symlinks. For a new path,
/// the nearest existing ancestor is canonicalized and the missing components
/// are appended. The root itself is not a valid worktree destination.
pub fn validate_managed_path(root: &Path, path: &Path) -> anyhow::Result<()> {
    let root = canonicalize_with_existing_ancestor(root)
        .with_context(|| format!("resolve managed root {}", root.display()))?;
    let path = canonicalize_with_existing_ancestor(path)
        .with_context(|| format!("resolve worktree path {}", path.display()))?;

    if path == root {
        bail!("worktree path cannot be the managed root");
    }
    if !path.starts_with(&root) {
        bail!(
            "worktree path {} is outside managed root {}",
            path.display(),
            root.display()
        );
    }
    Ok(())
}

fn canonicalize_with_existing_ancestor(path: &Path) -> anyhow::Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).map_err(Into::into);
    }

    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    while !current.exists() {
        let name = current
            .file_name()
            .context("path has no file name while resolving missing ancestor")?;
        missing.push(name.to_os_string());
        current = current
            .parent()
            .context("path has no existing ancestor")?
            .to_path_buf();
    }

    let mut resolved = fs::canonicalize(current)?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn branch_slug(branch: &str) -> String {
    let mut slug = String::with_capacity(branch.len().min(48));
    let mut last_dash = false;
    for ch in branch.chars() {
        let safe = ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.');
        if safe {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= 48 {
            break;
        }
    }
    while slug.ends_with('-') || slug.ends_with('.') {
        slug.pop();
    }
    if slug.is_empty() || slug == "." || slug == ".." {
        "worktree".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn managed_path_is_readable_and_unique_per_branch() {
        let dir = tempdir().unwrap();
        let project = ProjectId("project:abc".to_string());
        let first = managed_worktree_path(dir.path(), &project, "feature/one").unwrap();
        let second = managed_worktree_path(dir.path(), &project, "feature/one-two").unwrap();
        assert!(first.starts_with(dir.path()));
        assert!(first.to_string_lossy().contains("feature-one"));
        assert_ne!(first, second);
    }

    #[test]
    fn containment_rejects_sibling_and_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("worktrees");
        fs::create_dir_all(&root).unwrap();
        assert!(validate_managed_path(&root, &root).is_err());
        assert!(validate_managed_path(&root, &dir.path().join("other")).is_err());
        assert!(validate_managed_path(&root, &root.join("project/branch")).is_ok());
    }
}
