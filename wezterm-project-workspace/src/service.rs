use crate::git::GitRepository;
use crate::layout::LayoutProfile;
use crate::path::validate_managed_path;
use crate::plan::{ExistingBranchPlan, NewBranchPlan, WorktreePlan};
use crate::types::{DomainKey, Lifecycle, ProjectId, WorkspaceId};
use anyhow::Context;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeSelection {
    Primary,
    Existing(PathBuf),
    NewBranch { branch: String, base_ref: String },
    ExistingBranch { branch: String },
}

#[derive(Debug, Clone)]
pub struct WorkspaceRequest {
    pub project_path: PathBuf,
    pub selection: WorktreeSelection,
    pub managed_root: PathBuf,
    pub label: Option<String>,
    pub profile: LayoutProfile,
}

#[derive(Debug, Clone)]
pub struct WorkspaceDescriptor {
    pub id: WorkspaceId,
    pub label: String,
    pub domain: DomainKey,
    pub project_root: PathBuf,
    pub project_id: ProjectId,
    pub worktree_path: PathBuf,
    pub branch: Option<String>,
    pub managed: bool,
    pub profile: LayoutProfile,
    pub lifecycle: Lifecycle,
}

#[derive(Debug, Clone)]
pub struct WorkspacePlan {
    pub descriptor: WorkspaceDescriptor,
    pub worktree_mutation: Option<WorktreePlan>,
}

/// Build a validated workspace plan without mutating Git, the registry, or
/// the mux. Applying a plan is intentionally a separate integration step.
pub fn plan_workspace(request: &WorkspaceRequest) -> anyhow::Result<WorkspacePlan> {
    request
        .profile
        .validate()
        .context("validate layout profile")?;
    let repository = GitRepository::discover(&request.project_path)?;
    let worktrees = repository.worktrees()?;

    let (worktree_path, branch, mutation) = match &request.selection {
        WorktreeSelection::Primary => {
            let worktree = find_worktree(&worktrees, &repository.identity.primary_root)?;
            (worktree.path.clone(), worktree.branch.clone(), None)
        }
        WorktreeSelection::Existing(path) => {
            let worktree = find_worktree(&worktrees, path)?;
            (worktree.path.clone(), worktree.branch.clone(), None)
        }
        WorktreeSelection::NewBranch { branch, base_ref } => {
            let plan = NewBranchPlan::create(&repository, &request.managed_root, branch, base_ref)?;
            (
                plan.0.destination.clone(),
                Some(branch.clone()),
                Some(plan.0),
            )
        }
        WorktreeSelection::ExistingBranch { branch } => {
            let plan = ExistingBranchPlan::create(&repository, &request.managed_root, branch)?;
            (
                plan.0.destination.clone(),
                Some(branch.clone()),
                Some(plan.0),
            )
        }
    };

    let managed = is_managed_path(&request.managed_root, &worktree_path);
    let id = WorkspaceId::for_worktree(&repository.identity.domain, &worktree_path);
    let project_name = repository
        .identity
        .primary_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    let label = request.label.clone().unwrap_or_else(|| {
        branch
            .as_deref()
            // The label doubles as the mux workspace name, so use the same
            // human-readable `project : branch` form that the tab bar shows.
            .map(|branch| format!("{project_name} : {branch}"))
            .unwrap_or_else(|| project_name.to_string())
    });

    Ok(WorkspacePlan {
        descriptor: WorkspaceDescriptor {
            id,
            label,
            domain: repository.identity.domain,
            project_root: repository.identity.primary_root,
            project_id: repository.identity.id,
            worktree_path,
            branch,
            managed,
            profile: request.profile.clone(),
            lifecycle: Lifecycle::Incomplete,
        },
        worktree_mutation: mutation,
    })
}

fn find_worktree<'a>(
    worktrees: &'a [crate::git::GitWorktree],
    requested: &Path,
) -> anyhow::Result<&'a crate::git::GitWorktree> {
    let requested = fs::canonicalize(requested)
        .with_context(|| format!("resolve worktree path {}", requested.display()))?;
    worktrees
        .iter()
        .find(|worktree| {
            fs::canonicalize(&worktree.path)
                .map(|path| path == requested)
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow::anyhow!("{} is not a worktree of this project", requested.display()))
}

fn is_managed_path(root: &Path, path: &Path) -> bool {
    validate_managed_path(root, path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::tempdir;

    fn run_git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q", "-b", "main"]);
        run_git(&repo, &["config", "user.email", "test@example.com"]);
        run_git(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("README"), "hello").unwrap();
        run_git(&repo, &["add", "README"]);
        run_git(&repo, &["commit", "-qm", "initial"]);
        (dir, repo)
    }

    #[test]
    fn plans_primary_checkout_without_mutation() {
        let (_dir, repo) = repository();
        let request = WorkspaceRequest {
            project_path: repo.clone(),
            selection: WorktreeSelection::Primary,
            managed_root: repo.parent().unwrap().join("worktrees"),
            label: None,
            profile: LayoutProfile::default_agentic(),
        };
        let plan = plan_workspace(&request).unwrap();
        assert_eq!(
            plan.descriptor.worktree_path,
            fs::canonicalize(repo).unwrap()
        );
        assert!(plan.worktree_mutation.is_none());
        assert_eq!(plan.descriptor.label, "repo : main");
        assert_eq!(plan.descriptor.lifecycle, Lifecycle::Incomplete);
    }

    #[test]
    fn plans_existing_branch_worktree_without_mutating_git() {
        let (_dir, repo) = repository();
        run_git(&repo, &["branch", "feature/existing"]);
        let managed_root = repo.parent().unwrap().join("worktrees");
        let request = WorkspaceRequest {
            project_path: repo.clone(),
            selection: WorktreeSelection::ExistingBranch {
                branch: "feature/existing".to_string(),
            },
            managed_root: managed_root.clone(),
            label: None,
            profile: LayoutProfile::default_agentic(),
        };
        let plan = plan_workspace(&request).unwrap();
        assert_eq!(plan.descriptor.branch.as_deref(), Some("feature/existing"));
        assert_eq!(plan.descriptor.label, "repo : feature/existing");
        assert!(plan.descriptor.managed);
        assert_eq!(plan.worktree_mutation.as_ref().unwrap().base_ref, None);
        assert!(!plan.descriptor.worktree_path.exists());
    }

    #[test]
    fn plans_existing_worktree_without_creating_another() {
        let (_dir, repo) = repository();
        let existing = repo.parent().unwrap().join("checked-out");
        run_git(&repo, &["branch", "feature/checked-out"]);
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                existing.to_str().unwrap(),
                "feature/checked-out",
            ],
        );
        let request = WorkspaceRequest {
            project_path: repo.clone(),
            selection: WorktreeSelection::Existing(existing.clone()),
            managed_root: repo.parent().unwrap().join("worktrees"),
            label: None,
            profile: LayoutProfile::default_agentic(),
        };
        let plan = plan_workspace(&request).unwrap();
        assert_eq!(
            plan.descriptor.worktree_path,
            fs::canonicalize(existing).unwrap()
        );
        assert_eq!(
            plan.descriptor.branch.as_deref(),
            Some("feature/checked-out")
        );
        assert_eq!(plan.descriptor.label, "repo : feature/checked-out");
        assert!(plan.worktree_mutation.is_none());
    }

    #[test]
    fn plans_new_branch_inside_managed_root() {
        let (_dir, repo) = repository();
        let managed_root = repo.parent().unwrap().join("worktrees");
        let request = WorkspaceRequest {
            project_path: repo,
            selection: WorktreeSelection::NewBranch {
                branch: "feature/agent-picker".to_string(),
                base_ref: "main".to_string(),
            },
            managed_root,
            label: Some("agent picker".to_string()),
            profile: LayoutProfile::default_agentic(),
        };
        let plan = plan_workspace(&request).unwrap();
        assert_eq!(plan.descriptor.label, "agent picker");
        assert!(plan.descriptor.managed);
        assert!(plan.worktree_mutation.is_some());
    }

    #[test]
    fn applies_new_branch_plan_without_force() {
        let (_dir, repo) = repository();
        let managed_root = repo.parent().unwrap().join("worktrees");
        let request = WorkspaceRequest {
            project_path: repo.clone(),
            selection: WorktreeSelection::NewBranch {
                branch: "feature/apply".to_string(),
                base_ref: "main".to_string(),
            },
            managed_root,
            label: None,
            profile: LayoutProfile::default_agentic(),
        };
        let plan = plan_workspace(&request).unwrap();
        plan.worktree_mutation
            .as_ref()
            .unwrap()
            .apply(&GitRepository::discover(&repo).unwrap())
            .unwrap();
        assert!(plan.descriptor.worktree_path.is_dir());
        assert!(GitRepository::discover(&repo)
            .unwrap()
            .worktrees()
            .unwrap()
            .iter()
            .any(|worktree| worktree.branch.as_deref() == Some("feature/apply")));
    }

    #[test]
    fn rejects_path_that_is_not_a_project_worktree() {
        let (_dir, repo) = repository();
        let request = WorkspaceRequest {
            project_path: repo,
            selection: WorktreeSelection::Existing(PathBuf::from("/tmp/not-a-worktree")),
            managed_root: PathBuf::from("/tmp/worktrees"),
            label: None,
            profile: LayoutProfile::default_agentic(),
        };
        assert!(plan_workspace(&request).is_err());
    }
}
