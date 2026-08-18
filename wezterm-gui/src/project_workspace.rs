//! GUI-facing project catalog and native picker plumbing.
//!
//! Rendering and mux provisioning remain separate from the core crate. This
//! module is the narrow bridge from WezTerm configuration/state to the typed
//! project catalog used by the native picker.

use config::keyassignment::{
    InputSelector, InputSelectorEntry, KeyAssignment, PromptInputLine, SpawnTabDomain,
};
use config::{Config, ProjectWorkspaceApplication as ConfigApplication, HOME_DIR};
use mux::domain::{ProjectLayoutRequest, ProjectPaneSpec};
use mux::tab::ProjectLayout;
use mux::Mux;
use portable_pty::CommandBuilder;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use wezterm_project_workspace::{
    default_managed_worktree_root, default_registry_path, plan_workspace, ApplicationSpec,
    GitRepository, LayoutProfile, Lifecycle, PaneRole, ProjectCatalog, Registry, RegistryWorkspace,
    WorkspaceDescriptor, WorkspaceId, WorkspaceRequest, WorktreeSelection,
};
use wezterm_term::TerminalSize;

pub(crate) const PROJECT_EVENT: &str = "__wezterm_project_workspace_project";
pub(crate) const WORKTREE_EVENT: &str = "__wezterm_project_workspace_worktree";
pub(crate) const NEW_BRANCH_ENTRY_PREFIX: &str = "__wezterm_project_workspace_new_branch:";
pub(crate) const EXISTING_BRANCH_ENTRY_PREFIX: &str =
    "__wezterm_project_workspace_existing_branch:";
pub(crate) const BRANCH_EVENT_PREFIX: &str = "__wezterm_project_workspace_branch:";
pub(crate) const EXISTING_BRANCH_EVENT_PREFIX: &str =
    "__wezterm_project_workspace_existing_branch:";

pub(crate) fn discover_projects(config: &Config) -> ProjectCatalog {
    let settings = &config.project_workspaces;
    if !settings.enabled {
        return ProjectCatalog::default();
    }

    let roots = if settings.roots.is_empty() {
        default_roots()
    } else {
        settings.roots.clone()
    };
    let recent = load_recent_paths();
    let zoxide = if settings.use_zoxide {
        ProjectCatalog::query_zoxide(64).unwrap_or_default()
    } else {
        Vec::new()
    };

    ProjectCatalog::discover(
        &roots,
        &recent,
        &zoxide,
        settings.scan_depth,
        &settings.excluded_directories,
    )
}

fn configured_profile() -> anyhow::Result<LayoutProfile> {
    let settings = &config::configuration().project_workspaces;
    let layout = settings.layout.trim().to_ascii_lowercase();
    ProjectLayout::from_str(&layout)
        .map_err(|error| anyhow::anyhow!("invalid project workspace layout: {error}"))?;
    let applications = settings
        .applications
        .iter()
        .map(configured_application)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let profile = LayoutProfile {
        name: layout.clone(),
        layout,
        applications,
    };
    profile.validate()?;
    Ok(profile)
}

fn configured_role(role: &str) -> anyhow::Result<PaneRole> {
    match role.to_ascii_lowercase().as_str() {
        "agent" => Ok(PaneRole::Agent),
        "editor" => Ok(PaneRole::Editor),
        "review" => Ok(PaneRole::Review),
        "terminal" => Ok(PaneRole::Terminal),
        other => anyhow::bail!("unknown project workspace pane role {other:?}"),
    }
}

fn configured_application(application: &ConfigApplication) -> anyhow::Result<ApplicationSpec> {
    Ok(ApplicationSpec {
        role: configured_role(&application.role)?,
        launch: application.launch.clone(),
        resume: application.resume.clone(),
        required: application.required,
    })
}

pub(crate) fn select_project(
    term_window: &mut crate::termwindow::TermWindow,
    entry: Option<InputSelectorEntry>,
) {
    let Some(path) = entry.and_then(|entry| entry.id).map(PathBuf::from) else {
        return;
    };
    let Ok(repository) = GitRepository::discover(&path) else {
        log::warn!(
            "selected project is no longer a Git repository: {}",
            path.display()
        );
        return;
    };
    let Ok(worktrees) = repository.worktrees() else {
        log::warn!("unable to list Git worktrees for {}", path.display());
        return;
    };
    let choices = worktrees
        .into_iter()
        .map(|worktree| InputSelectorEntry {
            label: format!(
                "{}  {}",
                worktree.branch.as_deref().unwrap_or("detached"),
                worktree.path.display()
            ),
            id: Some(worktree.path.to_string_lossy().into_owned()),
        })
        .collect();
    term_window.show_input_selector(&InputSelector {
        action: Box::new(KeyAssignment::EmitEvent(WORKTREE_EVENT.to_string())),
        title: "Choose worktree".to_string(),
        choices,
        fuzzy: true,
        alphabet: Default::default(),
        description: Default::default(),
        fuzzy_description: Default::default(),
        delete_action: None,
    });
}

pub(crate) fn select_worktree(
    term_window: &mut crate::termwindow::TermWindow,
    entry: Option<InputSelectorEntry>,
) {
    let Some(worktree_id) = entry.and_then(|entry| entry.id) else {
        return;
    };
    let managed_root = configured_managed_root(&term_window.config);
    if let Some(project_path) = worktree_id.strip_prefix(NEW_BRANCH_ENTRY_PREFIX) {
        show_branch_prompt(
            term_window,
            BRANCH_EVENT_PREFIX,
            project_path,
            &managed_root,
            "New branch name",
        );
        return;
    }
    if let Some(project_path) = worktree_id.strip_prefix(EXISTING_BRANCH_ENTRY_PREFIX) {
        show_branch_prompt(
            term_window,
            EXISTING_BRANCH_EVENT_PREFIX,
            project_path,
            &managed_root,
            "Existing local branch name",
        );
        return;
    }
    let worktree_path = PathBuf::from(worktree_id);
    let request = WorkspaceRequest {
        project_path: worktree_path.clone(),
        selection: WorktreeSelection::Existing(worktree_path),
        managed_root,
        label: None,
        profile: match configured_profile() {
            Ok(profile) => profile,
            Err(error) => {
                log::error!("invalid project workspace configuration: {error:#}");
                return;
            }
        },
    };
    let Ok(plan) = plan_workspace(&request) else {
        log::warn!("selected path is no longer a valid Git worktree");
        return;
    };
    // Project workspace labels are their mux names too. Keep the friendly
    // `project : branch` form, but add a stable short ID only when another
    // live workspace already owns that label.
    let mux = Mux::get();
    let workspace = workspace_name_for_descriptor(&plan.descriptor, &mux);
    if mux.workspace_exists(&workspace) {
        promise::spawn::spawn_into_main_thread(async move {
            crate::frontend::front_end().switch_workspace(&workspace, false);
            anyhow::Result::<()>::Ok(())
        })
        .detach();
        return;
    }
    // Migrate workspaces created before labels became mux names. If the rename
    // fails, keep the legacy workspace usable instead of switching to a name
    // that does not exist.
    let legacy_workspace = plan.descriptor.id.0.clone();
    if mux.workspace_exists(&legacy_workspace) {
        if let Err(error) = mux.rename_workspace(&legacy_workspace, &workspace) {
            log::warn!(
                "unable to rename legacy workspace {legacy_workspace:?} to {workspace:?}: {error:#}"
            );
            promise::spawn::spawn_into_main_thread(async move {
                crate::frontend::front_end().switch_workspace(&legacy_workspace, false);
                anyhow::Result::<()>::Ok(())
            })
            .detach();
            return;
        }
        update_registry_workspace_label(&plan.descriptor.id, &workspace);
        promise::spawn::spawn_into_main_thread(async move {
            crate::frontend::front_end().switch_workspace(&workspace, false);
            anyhow::Result::<()>::Ok(())
        })
        .detach();
        return;
    }
    promise::spawn::spawn(async move {
        if let Err(error) = launch_workspace(plan).await {
            log::error!("unable to launch project workspace: {error:#}");
        }
        anyhow::Result::<()>::Ok(())
    })
    .detach();
}

fn show_branch_prompt(
    term_window: &mut crate::termwindow::TermWindow,
    event_prefix: &str,
    project_path: &str,
    managed_root: &Path,
    description: &str,
) {
    let event = format!(
        "{event_prefix}{project_path}\0{}",
        managed_root.to_string_lossy()
    );
    term_window.show_prompt_input_line(&PromptInputLine {
        action: Box::new(KeyAssignment::EmitEvent(event)),
        initial_value: None,
        description: description.to_string(),
        prompt: "Branch: ".to_string(),
    });
}

fn split_branch_payload(payload: &str) -> (&str, PathBuf) {
    let Some((project_path, managed_root)) = payload.split_once('\0') else {
        return (
            payload,
            default_managed_worktree_root().unwrap_or_else(|_| {
                HOME_DIR
                    .join(".local")
                    .join("share")
                    .join("wezterm")
                    .join("worktrees")
            }),
        );
    };
    (project_path, PathBuf::from(managed_root))
}

fn configured_managed_root(config: &config::ConfigHandle) -> PathBuf {
    config
        .project_workspaces
        .worktree_root
        .clone()
        .or_else(|| default_managed_worktree_root().ok())
        .unwrap_or_else(|| {
            HOME_DIR
                .join(".local")
                .join("share")
                .join("wezterm")
                .join("worktrees")
        })
}

pub(crate) fn select_existing_branch(project_path: &str, branch: Option<String>) {
    let Some(branch) = branch.filter(|branch| !branch.trim().is_empty()) else {
        return;
    };
    let (project_path, managed_root) = split_branch_payload(project_path);
    let request = WorkspaceRequest {
        project_path: PathBuf::from(project_path),
        selection: WorktreeSelection::ExistingBranch { branch },
        managed_root,
        label: None,
        profile: match configured_profile() {
            Ok(profile) => profile,
            Err(error) => {
                log::error!("invalid project workspace configuration: {error:#}");
                return;
            }
        },
    };
    let Ok(plan) = plan_workspace(&request) else {
        log::warn!("unable to plan existing-branch project worktree");
        return;
    };
    promise::spawn::spawn(async move {
        if let Err(error) = launch_workspace(plan).await {
            log::error!("unable to launch existing-branch workspace: {error:#}");
        }
        anyhow::Result::<()>::Ok(())
    })
    .detach();
}

pub(crate) fn select_new_branch(project_path: &str, branch: Option<String>) {
    let Some(branch) = branch.filter(|branch| !branch.trim().is_empty()) else {
        return;
    };
    let (project_path, managed_root) = split_branch_payload(project_path);
    let project_path = PathBuf::from(project_path);
    let Ok(repository) = GitRepository::discover(&project_path) else {
        log::warn!(
            "project disappeared before branch creation: {}",
            project_path.display()
        );
        return;
    };
    let Ok(worktrees) = repository.worktrees() else {
        return;
    };
    let base_ref = worktrees
        .iter()
        .find(|worktree| {
            fs::canonicalize(&worktree.path)
                .map(|path| path == repository.identity.primary_root)
                .unwrap_or(false)
        })
        .and_then(|worktree| worktree.branch.clone())
        .unwrap_or_else(|| "HEAD".to_string());
    let request = WorkspaceRequest {
        project_path,
        selection: WorktreeSelection::NewBranch { branch, base_ref },
        managed_root,
        label: None,
        profile: match configured_profile() {
            Ok(profile) => profile,
            Err(error) => {
                log::error!("invalid project workspace configuration: {error:#}");
                return;
            }
        },
    };
    let Ok(plan) = plan_workspace(&request) else {
        log::warn!("unable to plan new project worktree");
        return;
    };
    promise::spawn::spawn(async move {
        if let Err(error) = launch_workspace(plan).await {
            log::error!("unable to launch new project workspace: {error:#}");
        }
        anyhow::Result::<()>::Ok(())
    })
    .detach();
}

async fn launch_workspace(plan: wezterm_project_workspace::WorkspacePlan) -> anyhow::Result<()> {
    let wezterm_project_workspace::WorkspacePlan {
        descriptor,
        worktree_mutation,
    } = plan;
    if let Some(mutation) = worktree_mutation.as_ref() {
        let repository = GitRepository::discover(&descriptor.project_root)?;
        mutation.apply(&repository)?;
    }
    // Keep the mux workspace name identical to the project/worktree label
    // shown at the top-left of the tab bar (for example, `mind_me : main`).
    // `descriptor.id` remains the stable opaque registry/environment ID.
    // Resolve the name as late as possible so a second project with the same
    // basename and branch gets a deterministic collision suffix rather than
    // sharing the first project's panes.
    let mux = Mux::get();
    let workspace = workspace_name_for_descriptor(&descriptor, &mux);
    if mux.workspace_exists(&workspace) {
        // Selecting an already-launched worktree should focus it, not spawn a
        // second editor/agent/review layout into the same workspace.
        promise::spawn::spawn_into_main_thread(async move {
            crate::frontend::front_end().switch_workspace(&workspace, false);
            anyhow::Result::<()>::Ok(())
        })
        .detach();
        return Ok(());
    }
    let cwd = descriptor.worktree_path.clone();
    let cwd_string = cwd
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("worktree path is not Unicode: {cwd:?}"))?
        .to_string();
    let environment = [
        ("WEZTERM_DEV_WORKSPACE_ID", descriptor.id.0.clone()),
        ("WEZTERM_PROJECT_ID", descriptor.project_id.0.clone()),
        (
            "WEZTERM_PROJECT_ROOT",
            descriptor.project_root.to_string_lossy().into_owned(),
        ),
        (
            "WEZTERM_WORKTREE_ROOT",
            descriptor.worktree_path.to_string_lossy().into_owned(),
        ),
    ];
    let profile = descriptor.profile.clone();
    let layout = ProjectLayout::from_str(&profile.layout)
        .map_err(|error| anyhow::anyhow!("invalid project layout {:?}: {error}", profile.layout))?;
    let panes = profile
        .applications
        .iter()
        .map(|application| ProjectPaneSpec {
            command: Some(command_builder(
                &application.launch,
                &cwd,
                &environment,
                application.role.as_str(),
            )),
            command_dir: Some(cwd_string.clone()),
        })
        .collect();

    // The mux provisions the entire workspace in one operation. The GUI only
    // supplies module commands and the named topology; all split sizing and
    // pane geometry stay on the mux side.
    mux.spawn_project_layout(
        SpawnTabDomain::DomainName("persistent".to_string()),
        ProjectLayoutRequest {
            workspace: workspace.clone(),
            size: TerminalSize::default(),
            layout,
            panes,
        },
    )
    .await?;

    if let Ok(path) = default_registry_path() {
        let mut registry = Registry::load(&path).unwrap_or_default();
        registry.remember_workspace(RegistryWorkspace {
            id: descriptor.id.clone(),
            label: workspace.clone(),
            domain: descriptor.domain.clone(),
            project_id: descriptor.project_id.clone(),
            worktree_path: descriptor.worktree_path.clone(),
            branch: descriptor.branch.clone(),
            managed: descriptor.managed,
            layout_profile: profile.name.clone(),
            lifecycle: Lifecycle::Ready,
            last_opened_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        });
        if let Err(error) = registry.save_atomic(&path) {
            log::warn!("workspace launched but registry update failed: {error:#}");
        }
    }

    promise::spawn::spawn_into_main_thread(async move {
        crate::frontend::front_end().switch_workspace(&workspace, false);
        anyhow::Result::<()>::Ok(())
    })
    .detach();
    Ok(())
}

fn workspace_name_for_descriptor(descriptor: &WorkspaceDescriptor, mux: &Mux) -> String {
    let registry = default_registry_path()
        .ok()
        .and_then(|path| Registry::load(&path).ok());
    let base = descriptor.label.trim().to_string();
    let suffix = descriptor
        .id
        .0
        .strip_prefix("dev:")
        .unwrap_or(&descriptor.id.0);
    let suffix: String = suffix.chars().take(8).collect();
    let stable_collision_prefix = format!("{base} [{suffix}");

    if let Some(registry) = registry.as_ref() {
        // Reuse the actual live name for this stable worktree identity. This
        // preserves collision suffixes across restarts.
        if let Some(existing) = registry.workspaces.get(&descriptor.id) {
            if mux.workspace_exists(&existing.label)
                || existing.label == base
                || existing.label.starts_with(&stable_collision_prefix)
            {
                return existing.label.clone();
            }
        }
    }

    let label_taken = |candidate: &str| {
        mux.workspace_exists(candidate)
            || registry.as_ref().is_some_and(|registry| {
                registry
                    .workspaces
                    .values()
                    .any(|workspace| workspace.id != descriptor.id && workspace.label == candidate)
            })
    };
    if !label_taken(&base) {
        return base;
    }

    let mut candidate = format!("{base} [{suffix}]");
    let mut number = 2;
    while label_taken(&candidate) {
        candidate = format!("{base} [{suffix}-{number}]");
        number += 1;
    }
    candidate
}

fn update_registry_workspace_label(id: &WorkspaceId, label: &str) {
    let Ok(path) = default_registry_path() else {
        return;
    };
    let Ok(mut registry) = Registry::load(&path) else {
        return;
    };
    let Some(workspace) = registry.workspaces.get_mut(id) else {
        return;
    };
    if workspace.label == label {
        return;
    }
    workspace.label = label.to_string();
    if let Err(error) = registry.save_atomic(&path) {
        log::warn!("unable to update project workspace label: {error:#}");
    }
}

fn command_builder(
    argv: &[String],
    cwd: &Path,
    environment: &[(&str, String)],
    role: &str,
) -> CommandBuilder {
    // Keep the pane's normal shell as the long-lived process. The application
    // is a foreground child, so closing nvim/pi/tuicr returns to the shell
    // instead of causing the mux to remove the pane.
    let mut command = vec![
        "/bin/sh".to_string(),
        "-lc".to_string(),
        r#""$@"; exec "${SHELL:-/bin/sh}" -l"#.to_string(),
        "wezterm-project-app".to_string(),
    ];
    command.extend(argv.iter().cloned());
    let mut builder = CommandBuilder::from_argv(command.into_iter().map(Into::into).collect());
    builder.cwd(cwd);
    for (key, value) in environment {
        builder.env(key, value);
    }
    builder.env("WEZTERM_PANE_ROLE", role);
    builder
}

fn default_roots() -> Vec<PathBuf> {
    ["Dev", "Projects"]
        .iter()
        .map(|name| HOME_DIR.join(name))
        .filter(|path| path.is_dir())
        .collect()
}

fn load_recent_paths() -> Vec<PathBuf> {
    let Ok(path) = default_registry_path() else {
        return Vec::new();
    };
    let Ok(registry) = Registry::load(&path) else {
        return Vec::new();
    };
    registry
        .workspaces
        .values()
        .map(|workspace| workspace.worktree_path.clone())
        .collect()
}
