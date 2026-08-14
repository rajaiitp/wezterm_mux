//! Typed project, Git worktree, and development-workspace primitives.
//!
//! This crate deliberately does not contain GUI or mux code. It owns the
//! filesystem/Git model used by the native workspace picker and keeps plans
//! independent from the eventual local or remote domain executor.

mod catalog;
mod git;
mod layout;
mod path;
mod plan;
mod registry;
mod service;
mod types;

pub use catalog::{CatalogCandidate, CatalogSource, ProjectCatalog};
pub use git::{parse_worktree_list, GitRepository, GitWorktree, GitWorktreeState};
pub use layout::{ApplicationSpec, LayoutNode, LayoutProfile, PaneRole, SplitDirection};
pub use path::{default_managed_worktree_root, managed_worktree_path, validate_managed_path};
pub use plan::{workspace_id_for_worktree, ExistingBranchPlan, NewBranchPlan, WorktreePlan};
pub use registry::{default_registry_path, ProjectRecord, Registry, RegistryWorkspace};
pub use service::{
    plan_workspace, WorkspaceDescriptor, WorkspacePlan, WorkspaceRequest, WorktreeSelection,
};
pub use types::{DomainKey, DomainKind, Lifecycle, ProjectId, ProjectIdentity, WorkspaceId};
