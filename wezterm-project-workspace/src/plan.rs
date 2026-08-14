use crate::git::{GitRepository, GitWorktree};
use crate::path::{managed_worktree_path, validate_managed_path};
use crate::types::{ProjectId, WorkspaceId};
use anyhow::{bail, Context};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreePlan {
    pub project_id: ProjectId,
    pub destination: PathBuf,
    pub branch: String,
    pub base_ref: Option<String>,
    pub argv: Vec<OsString>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBranchPlan(pub WorktreePlan);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingBranchPlan(pub WorktreePlan);

impl WorktreePlan {
    pub fn apply(&self, repository: &GitRepository) -> anyhow::Result<()> {
        if self.destination.exists() {
            bail!(
                "worktree destination {} already exists",
                self.destination.display()
            );
        }
        if let Some(base_ref) = self.base_ref.as_deref() {
            return repository.add_worktree(&self.destination, &self.branch, base_ref);
        }

        let parent = self
            .destination
            .parent()
            .context("worktree destination has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create worktree parent {}", parent.display()))?;
        // Existing-branch worktrees intentionally do not use --force.
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repository.identity.primary_root)
            .args(["worktree", "add"])
            .arg(&self.destination)
            .arg(&self.branch)
            .output()
            .context("run git worktree add")?;
        if !output.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

impl NewBranchPlan {
    pub fn create(
        repository: &GitRepository,
        managed_root: &Path,
        branch: &str,
        base_ref: &str,
    ) -> anyhow::Result<Self> {
        repository.validate_branch_name(branch)?;
        if repository.local_branch_exists(branch)? {
            bail!("local branch {branch:?} already exists");
        }
        let destination = managed_worktree_path(managed_root, &repository.identity.id, branch)?;
        validate_destination(managed_root, &destination)?;
        let argv = worktree_add_argv(
            &repository.identity.primary_root,
            &destination,
            Some(branch),
            base_ref,
        );
        Ok(Self(WorktreePlan {
            project_id: repository.identity.id.clone(),
            destination,
            branch: branch.to_string(),
            base_ref: Some(base_ref.to_string()),
            argv,
        }))
    }

    pub fn apply(&self, repository: &GitRepository) -> anyhow::Result<()> {
        self.0.apply(repository)
    }
}

impl ExistingBranchPlan {
    pub fn create(
        repository: &GitRepository,
        managed_root: &Path,
        branch: &str,
    ) -> anyhow::Result<Self> {
        repository.validate_branch_name(branch)?;
        if !repository.local_branch_exists(branch)? {
            bail!("local branch {branch:?} does not exist");
        }
        let worktrees = repository.worktrees()?;
        if let Some(existing) = worktrees
            .iter()
            .find(|worktree| worktree.branch.as_deref() == Some(branch))
        {
            bail!(
                "branch {branch:?} is already attached to {}",
                existing.path.display()
            );
        }
        let destination = managed_worktree_path(managed_root, &repository.identity.id, branch)?;
        validate_destination(managed_root, &destination)?;
        let argv = worktree_add_argv(
            &repository.identity.primary_root,
            &destination,
            None,
            branch,
        );
        Ok(Self(WorktreePlan {
            project_id: repository.identity.id.clone(),
            destination,
            branch: branch.to_string(),
            base_ref: None,
            argv,
        }))
    }

    pub fn apply(&self, repository: &GitRepository) -> anyhow::Result<()> {
        self.0.apply(repository)
    }
}

fn validate_destination(managed_root: &Path, destination: &Path) -> anyhow::Result<()> {
    if destination.exists() {
        bail!(
            "worktree destination {} already exists",
            destination.display()
        );
    }
    validate_managed_path(managed_root, destination)
}

fn worktree_add_argv(
    repository: &Path,
    destination: &Path,
    new_branch: Option<&str>,
    ref_name: &str,
) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("git"),
        OsString::from("-C"),
        repository.as_os_str().to_os_string(),
        OsString::from("worktree"),
        OsString::from("add"),
    ];
    if let Some(branch) = new_branch {
        argv.push(OsString::from("-b"));
        argv.push(OsString::from(branch));
    }
    argv.push(destination.as_os_str().to_os_string());
    argv.push(OsString::from(ref_name));
    argv
}

/// Return the stable development-workspace identity for a worktree.
pub fn workspace_id_for_worktree(
    repository: &GitRepository,
    worktree: &GitWorktree,
) -> WorkspaceId {
    WorkspaceId::for_worktree(&repository.identity.domain, &worktree.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_uses_direct_arguments_and_no_shell() {
        let argv = worktree_add_argv(
            Path::new("/repo with spaces"),
            Path::new("/worktree"),
            Some("feature/x"),
            "main",
        );
        assert_eq!(argv[0], "git");
        assert!(argv.iter().all(|arg| !arg.to_string_lossy().contains("&&")));
        assert_eq!(argv[5], "-b");
    }
}
