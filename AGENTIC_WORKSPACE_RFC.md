# RFC: Native Agentic Project Workspaces

- **Status:** Proposed
- **Target:** WezTerm Herdr fork
- **Scope:** Native project/worktree selection, workspace lifecycle, and an integrated Neovim + Pi + tuicr workflow
- **Initial domain support:** Local execution with domain-aware contracts

## 1. Summary

Add a native WezTerm workspace wizard that lets a user:

1. choose a project from configured roots, recent workspace history, and optional zoxide results;
2. choose the main checkout, attach an existing worktree, create a worktree on a new branch, or create one for an existing branch;
3. focus an already-running workspace or restore a stopped one;
4. start a role-based development layout containing Neovim, Pi, and tuicr in the selected worktree; and
5. safely stop and clean up managed workspaces and worktrees.

WezTerm owns workspace orchestration because it exists before any editor or agent process, already owns terminal topology, and can preserve the workspace across GUI reconnects. Pi and tuicr integrate with the resulting workspace, but neither is responsible for creating it.

The system is not a shell script. Discovery, planning, state transitions, mux operations, and cleanup are typed and testable. External programs are executed as argv arrays without an implicit shell.

## 2. Decisions

The following decisions are accepted for this RFC:

- The primary entry point is a **native WezTerm picker**.
- The project catalog combines:
  - configured project roots;
  - native recent-workspace state; and
  - optional zoxide results.
- New worktrees live under a **central managed worktree directory** by default.
- The first complete workflow supports:
  - the main checkout and existing worktrees;
  - a new branch from a selected base ref;
  - an existing local or remote branch;
  - recent workspace resume; and
  - guarded workspace/worktree cleanup.
- Reopening first focuses a live workspace; otherwise it restores app sessions when possible.
- Contracts include a domain/host identity now, but the first implementation executes only on the local domain.
- A workspace requires three roles: `editor`, `agent`, and `review`.
- The exact pane topology and default split ratios remain an explicit follow-up decision. Layout is represented as a configurable profile so this does not block the core architecture.

## 3. Goals

### 3.1 User goals

- Reach any active project workspace with one shortcut and a fuzzy search.
- Start isolated work on a branch without manually composing `git worktree`, paths, workspace names, and split commands.
- Keep Neovim, Pi, and tuicr in the exact same worktree and domain.
- Resume the existing live processes rather than creating duplicates.
- Recover a stopped workspace without losing Pi or editor continuity where the applications support it.
- Review agent changes in tuicr and make saved comments available to Pi.
- Remove temporary worktrees without risking dirty files, running processes, or branch deletion.

### 3.2 Engineering goals

- Reuse the native mux, persistent mux process, workspace ownership model, pane tree, and session state.
- Make workspace creation idempotent and concurrency-safe.
- Separate discovery, planning, mutation, and UI.
- Preserve a path to native SSH-domain execution without putting SSH into the first slice.
- Provide typed state, errors, and protocol messages.
- Avoid parsing human-oriented WezTerm output or driving panes through synthetic key input.

## 4. Non-goals

The first implementation will not:

- implement remote project discovery or remote Git mutation;
- delete Git branches automatically;
- reset branches, force-check out a branch already active elsewhere, or use `git worktree add --force`;
- infer arbitrary project-specific setup commands from repository contents;
- require tmux, zellij, fzf, or a shell-script IPC layer;
- embed Pi or tuicr inside the WezTerm process;
- promise restoration beyond what each application can support;
- replace WezTerm's existing whole-mux crash/restart persistence; or
- standardize the final pane split topology in this RFC.

## 5. Terminology

- **Project:** A Git repository identity, based on its Git common directory rather than one checkout path.
- **Checkout:** The primary working tree discovered for a project.
- **Worktree:** Any linked Git working tree, including the primary checkout.
- **Managed worktree:** A linked worktree created below WezTerm's configured managed root and recorded in the registry.
- **Development workspace:** A WezTerm workspace bound to exactly one worktree and one domain.
- **Live workspace:** A development workspace with mux panes still present.
- **Stopped workspace:** A registry entry whose mux workspace no longer exists.
- **Layout profile:** A role-based pane tree and launch policy.
- **Role:** A semantic pane purpose. The initial required roles are `editor`, `agent`, and `review`.

## 6. User experience

### 6.1 Entry points

Add a native action and command-palette item:

```text
ShowProjectWorkspacePicker
```

A user can bind it like any other `KeyAssignment`. It opens a native modal and does not depend on Lua callbacks.

The workspace status UI and existing workspace launcher remain available. Existing live development workspaces should also appear in the ordinary workspace list.

### 6.2 Project picker

The first screen shows deduplicated project rows. Each row includes:

- display name;
- primary path;
- current branch when cheaply available;
- number of linked worktrees;
- live/stopped workspace count; and
- recency.

Search matches the project name, path, branch names already loaded, and optional aliases. Ranking is deterministic:

1. exact/prefix text match;
2. live workspace;
3. recent development workspace;
4. configured/pinned project;
5. zoxide score;
6. normalized path as a stable tie-breaker.

Zoxide enriches ranking and discovery but is never the source of truth. A zoxide path is accepted only after it is canonicalized and confirmed to belong to a Git repository.

### 6.3 Worktree action

After choosing a project, the wizard offers:

- **Open primary checkout**
- **Open existing worktree**
- **New branch + worktree**
- **Existing branch + worktree**
- **Resume recent workspace** when a stopped registry entry exists

A live workspace is marked explicitly. Selecting it focuses the owning GUI workspace and performs no Git or spawn operation.

### 6.4 Existing worktree

The worktree list is produced from `git worktree list --porcelain -z`. Rows show:

- path;
- branch or detached HEAD;
- short commit;
- lock/prunable state;
- dirty state when requested or already cached; and
- live/stopped workspace state.

The initial list should avoid running `git status` for every item. Dirty state can be loaded for the highlighted row or during launch/cleanup preflight.

### 6.5 New branch + worktree

The flow is:

1. choose a base ref;
2. enter a new branch name;
3. preview the managed path and workspace label;
4. run validation; and
5. confirm the plan.

Validation includes:

- Git ref syntax;
- branch nonexistence;
- resolvable base ref;
- destination containment under the managed root;
- destination nonexistence;
- no conflicting in-progress launch; and
- required application availability.

The Git operation is equivalent to:

```text
git -C <project> worktree add -b <branch> <path> <base-ref>
```

It is executed directly as argv, not through a shell.

### 6.6 Existing branch + worktree

Local and remote branches are presented separately.

- An unoccupied local branch uses `git worktree add <path> <branch>`.
- A branch already attached to a worktree redirects the user to that worktree; it is not force-attached.
- A remote-only branch creates a local tracking branch after the user selects the exact remote ref.
- Ambiguous remote branch names require an explicit remote choice.

The system does not rely on `--guess-remote` when more than one candidate exists.

### 6.7 Launch progress

After confirmation, the modal becomes a cancellable progress view with named stages:

```text
Validating project
Creating worktree
Reserving workspace
Starting editor
Starting agent
Starting review
Activating workspace
```

Cancellation is honored before a mutation starts and between completed stages. Once a child process or Git operation is in a non-interruptible commit window, cancellation waits for that operation and then applies the normal compensation policy.

### 6.8 Resume behavior

Resume follows this order:

1. If the workspace is live, focus it.
2. If whole-mux session restoration already recreated the workspace, reconcile the registry and focus it.
3. Otherwise, validate the worktree and recreate the layout.
4. Launch role-specific resume commands:
   - Neovim: profile-defined restore command when configured, otherwise the normal editor command;
   - Pi: `pi --continue` in the worktree by default;
   - tuicr: reopen working-tree review in the same repository.
5. If application-specific restore fails, keep the workspace and show the failure in that role's pane rather than destroying unrelated panes.

Persistent mux survival remains the strongest resume mechanism: closing and reopening the GUI should normally reattach to the same Neovim, Pi, and tuicr processes without relaunching anything.

### 6.9 Cleanup

Cleanup has two distinct actions:

- **Stop workspace:** close the mux workspace but retain the checkout/worktree.
- **Remove managed worktree:** stop the workspace and remove only a clean, managed linked worktree.

The cleanup screen previews every effect. It must refuse destructive cleanup when:

- any pane has a protected running foreground process;
- the worktree has modified, staged, conflicted, or untracked files;
- the worktree is locked;
- the path is outside the configured managed root;
- the registry does not match Git's worktree metadata;
- the target is the primary checkout; or
- another launch/cleanup transaction owns the workspace.

A user may explicitly terminate workspace processes after confirmation. Dirty-state refusal is not bypassed in the normal UI. Advanced manual recovery remains a Git operation outside this feature.

Removing a managed worktree does not delete its branch. Branch cleanup must be a separate future action with its own merged/unpushed checks.

## 7. Architecture

### 7.1 Ownership

The system has four layers:

1. **Native GUI wizard** — rendering, input, previews, and progress.
2. **Project workspace service** — catalog, Git model, plans, state machine, and local executor.
3. **Mux lifecycle adapter** — workspace reservation, pane creation, role metadata, focus, and shutdown.
4. **Application adapters** — Neovim, Pi, and tuicr launch/resume behavior.

WezTerm owns the first three. Applications remain independent processes.

### 7.2 Recommended source layout

Introduce a focused crate rather than putting Git and registry logic into the GUI modal:

```text
wezterm-project-workspace/
  src/
    lib.rs
    catalog.rs
    config.rs
    error.rs
    git.rs
    identity.rs
    layout.rs
    plan.rs
    registry.rs
    service.rs

wezterm-gui/src/termwindow/project_workspace.rs  # native modal/wizard
```

The crate contains no rendering code. Git command execution and filesystem access sit behind traits so unit tests can use fixtures and fake executors.

The mux adapter may initially live beside existing mux workspace code, but its request/response types belong in `codec` so the same operation can later execute in a remote mux domain.

### 7.3 Domain-aware service contract

Every project and development workspace carries a `DomainKey`:

```rust
struct DomainKey {
    kind: DomainKind,      // Local in v1; Ssh and UnixMux reserved
    stable_id: String,
}
```

All service requests include the domain. The v1 executor rejects non-local domains with a typed `unsupported_domain` error. This prevents local paths and remote paths from being conflated and leaves room for the same request to be routed to a remote mux endpoint later.

### 7.4 Project identity

Do not use a checkout path as project identity. Resolve:

```text
git -C <candidate> rev-parse --path-format=absolute --git-common-dir
git -C <candidate> rev-parse --show-toplevel
```

Then compute a stable project ID from:

```text
version + domain stable ID + canonical Git common directory
```

The human label is separate from the ID. This deduplicates a repository reached through configured roots, recent state, zoxide, and linked worktrees.

Canonicalization failures are reported and the candidate is skipped. Paths are never interpolated into shell command strings.

### 7.5 Development workspace identity

The internal workspace ID is stable and opaque:

```text
dev:<short-hash(domain, canonical-worktree-path)>
```

The display label is mutable metadata, for example `wezterm · feature/agent-picker`. Existing mux workspace names currently act as both identity and label; this feature should add metadata rather than encoding all state into the workspace name.

At minimum, a development workspace descriptor contains:

```rust
struct DevelopmentWorkspace {
    id: WorkspaceId,
    label: String,
    domain: DomainKey,
    project_id: ProjectId,
    worktree_path: PathBuf,
    branch: Option<String>,
    head: Option<String>,
    managed: bool,
    layout_profile: String,
    last_opened_at: Timestamp,
    lifecycle: WorkspaceLifecycle,
}
```

### 7.6 Project catalog

Catalog refresh runs asynchronously and emits incremental results to the modal.

Sources:

1. Registry recents are loaded immediately.
2. Configured roots are scanned with bounded depth and concurrency.
3. Zoxide is queried when enabled and available.
4. Git candidates are canonicalized and deduplicated by `ProjectId`.

Root scanning rules:

- roots and maximum depth are explicit configuration;
- `.git` directories and `.git` files are recognized;
- nested repositories are supported but scanning does not descend into `.git`, managed worktree storage, build output exclusions, or user-configured exclusions;
- symlink traversal is disabled by default;
- scanning has a time budget and can continue updating the picker after it opens;
- errors are diagnostics, not fatal to the entire catalog.

The catalog is cached in memory. On-disk state stores recency and user aliases, not an unbounded mirror of zoxide history.

### 7.7 Git adapter

Use the installed `git` executable through a structured command runner. Machine-readable operations use NUL-delimited formats where available:

- `git worktree list --porcelain -z`
- `git for-each-ref --format=...` with unambiguous delimiters
- `git status --porcelain=v2 -z --branch`
- `git check-ref-format --branch`

The parser must preserve arbitrary valid paths and reject malformed/truncated output. Human-oriented Git text is shown only as diagnostics and is not used for state decisions.

### 7.8 Managed worktree paths

State and managed checkout data are separate:

- registry/state: the platform's WezTerm state directory;
- managed worktrees: the platform's WezTerm data directory, unless overridden.

Conceptually:

```text
<wezterm-data>/worktrees/<project-id>/<branch-slug>-<short-hash>/
```

The hash is authoritative for uniqueness; the slug is for readability. A path must canonicalize beneath the configured managed root before it can be marked managed or removed.

Branch names are not used directly as path fragments. Slugs remove separators and unsafe characters, have a length limit, and cannot produce `.` or `..`.

### 7.9 Registry

Add a versioned registry, written atomically with user-only permissions:

```json
{
  "version": 1,
  "projects": {},
  "workspaces": {},
  "aliases": {},
  "lastOpenedWorkspace": null
}
```

The registry is an index, not the source of truth for Git or the mux. On load:

- Git confirms project/worktree relationships;
- the mux confirms live workspace state;
- missing worktrees become stale records;
- live unregistered development workspaces may be adopted when their metadata is complete; and
- migrations are explicit per version.

Writes use the same durability approach as native session state: temporary file, file sync, atomic rename, and directory sync. A failed registry write does not silently report launch success; the workspace is marked recoverable and surfaced to the user.

### 7.10 Concurrency and idempotency

A launch key is derived from domain + project + requested worktree/branch. Only one mutation for a key may run at a time.

Required behavior:

- repeated selection of a live workspace focuses it;
- repeated launch while provisioning joins or observes the existing operation;
- two clients cannot reserve the same workspace name;
- branch/path validation is repeated immediately before `git worktree add`;
- registry mutation is serialized; and
- Git success followed by mux failure records an incomplete managed worktree rather than losing it.

Use the mux/service request queue as the in-process serialization boundary and a process-visible registry lock for commands that can originate from multiple WezTerm processes.

### 7.11 Plan/apply model

Every mutating action is built as an immutable plan first:

```rust
enum WorkspacePlan {
    FocusLive { workspace_id: WorkspaceId },
    LaunchExisting { descriptor: ..., profile: ... },
    CreateAndLaunch { git: GitWorktreePlan, descriptor: ..., profile: ... },
    Stop { workspace_id: WorkspaceId },
    RemoveManaged { workspace_id: WorkspaceId, expected_git_state: ... },
}
```

A plan includes preconditions and a user-readable effect list. `apply` revalidates the preconditions to prevent time-of-check/time-of-use errors.

### 7.12 Mutation and compensation

Creation order:

1. acquire operation lock;
2. revalidate project, ref, destination, tools, and workspace state;
3. create the worktree when required;
4. persist an `incomplete` registry record;
5. reserve the mux workspace;
6. create the root pane and remaining role panes;
7. attach role/workspace metadata;
8. activate the workspace;
9. mark the registry record `ready`;
10. release the lock.

Compensation policy:

- Before Git mutation: no cleanup is required.
- Git succeeds but registry/mux setup fails: keep the worktree, record or report its path, and offer retry/remove. Do not delete it automatically after applications had a chance to start.
- Workspace reservation succeeds but no pane starts: release the reservation.
- Some role panes start: keep the workspace as `degraded` and show retry controls for failed roles.
- Registry finalization fails: keep the live workspace, display a persistent warning, and retry registry reconciliation later.

This favors preserving user data over pretending the operation was atomic.

## 8. Mux and protocol changes

### 8.1 Native action

Add:

```rust
KeyAssignment::ShowProjectWorkspacePicker
```

It appears in the command palette and can receive an optional initial profile or project filter in a later extension of the action.

### 8.2 Typed service messages

The GUI should use typed requests rather than directly coordinating multiple spawn calls. Proposed operations:

```text
project_workspace.catalog
project_workspace.project
project_workspace.refs
project_workspace.plan
project_workspace.apply
project_workspace.status
project_workspace.cleanup_plan
project_workspace.cleanup_apply
project_workspace.subscribe
```

For the native mux protocol these should be Rust request/response types in `codec`, not JSON internally. The existing automation endpoint may expose JSON-RPC adapters later so Pi or other trusted clients can request the same plans.

### 8.3 Events

Long operations emit bounded events:

```text
catalog.updated
operation.started
operation.stage
operation.completed
operation.failed
workspace.focused
workspace.degraded
workspace.stopped
workspace.removed
```

Each operation has an opaque ID. Events include a monotonic sequence so reconnecting clients can request current status rather than polling.

### 8.4 Workspace and pane metadata

Add generic metadata rather than Pi-specific mux fields:

```json
{
  "workspace.kind": "development",
  "workspace.id": "dev:...",
  "workspace.project_id": "project:...",
  "workspace.worktree": "/path/to/worktree",
  "workspace.profile": "agentic",
  "pane.role": "editor|agent|review"
}
```

Each role process also receives environment variables:

```text
WEZTERM_DEV_WORKSPACE_ID
WEZTERM_PROJECT_ID
WEZTERM_PROJECT_ROOT
WEZTERM_WORKTREE_ROOT
WEZTERM_PANE_ROLE
```

These make integrations deterministic without requiring a process to infer identity from pane titles.

## 9. Layout and application profiles

### 9.1 Layout tree

Represent layouts as a recursive tree independent of the eventual default:

```rust
enum LayoutNode {
    Pane { role: String },
    Split {
        direction: SplitDirection,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}
```

Validation requires exactly one pane for each required role in the initial `agentic` profile. Ratios are bounded away from zero and one. The planner converts the tree into deterministic mux split operations.

### 9.2 Application adapter contract

Each role declares:

```rust
struct AppAdapter {
    role: String,
    launch_argv: Vec<String>,
    resume_argv: Option<Vec<String>>,
    required: bool,
    readiness: ReadinessPolicy,
    environment: Map<String, String>,
}
```

Defaults are conceptually:

```text
editor: nvim .
agent:  pi --continue
review: tuicr --working-tree --no-update-check
```

Commands remain configurable as argv arrays. Project-local profiles may be considered later, but must pass the existing project trust boundary before they can supply executable commands.

### 9.3 Readiness

The initial implementation should treat successful PTY spawn as ready. It must not scrape terminal output to detect application prompts.

Future adapters may use explicit application IPC or process metadata. Readiness failures affect only the role pane and mark the workspace degraded.

### 9.4 Pane titles and status

Pane titles include semantic role plus application-provided title. Workspace status can summarize:

```text
editor: running
agent: idle | thinking | waiting | error
review: reviewing | comments-ready | idle
```

Agent state already fits the generic automation client metadata model. Review state should use a stable tuicr interface when one exists; it should not be inferred from pixels or screen text.

## 10. Pi integration

### 10.1 Startup

Pi starts in the selected worktree with the workspace environment above. `pi --continue` uses Pi's cwd-scoped session lookup. A profile can choose a fresh Pi session instead.

The `pi-wezterm` extension should publish:

- workspace ID;
- pane role;
- cwd/worktree;
- Pi session ID/name when available; and
- idle/thinking/tool/waiting/error state.

The extension must continue using the typed automation socket and must not invoke `wezterm cli` per operation.

### 10.2 Workspace-aware tools

After the native workflow is stable, expose narrow Pi tools backed by the same service:

- list development workspaces;
- focus a workspace;
- report current workspace descriptor;
- request a launch/cleanup plan;
- apply only after the appropriate user confirmation.

The picker remains the primary launcher. Pi tools are useful for agent-assisted handoff, not for bootstrapping Pi's own initial pane.

### 10.3 Session naming

On the first new Pi session, the extension may set a descriptive session name derived from project + branch. It must not overwrite an existing user-defined name when continuing a session.

## 11. tuicr integration

### 11.1 Startup and session discovery

tuicr starts in the same worktree, initially reviewing working-tree changes. Pi discovers the active persisted review session using the stable CLI interface:

```text
tuicr review list --repo <worktree>
tuicr review comments --repo <worktree> --session <slug>
```

The review session slug is associated with the development workspace in Pi extension memory or session state, not treated as a permanent Git identity.

### 11.2 Review loop

The supported initial loop is:

1. Pi changes code.
2. The user reviews in the always-visible tuicr pane.
3. The user saves comments and signals Pi that comments are ready.
4. Pi reads comments through `tuicr review comments`.
5. Pi addresses `issue` comments first, then suggestions/notes.
6. Pi rereads comments before claiming completion if review may have continued.

No agent-authored tuicr comments are added unless the user explicitly requests agent review. User comments and agent comments retain distinct usernames and ownership.

### 11.3 Event integration path

tuicr currently exposes persisted-session CLI reads rather than a push stream. The first implementation must not bind to undocumented session-file internals.

A later tuicr integration can add a small stable event endpoint for:

```text
review.session_started
review.comments_saved
review.session_finished
```

Until then, the user signal or an explicit Pi command triggers a CLI read. Periodic polling is optional only while Pi is explicitly waiting for a review; it is not a permanent background loop.

## 12. Configuration

Proposed configuration shape:

```lua
config.project_workspaces = {
  enabled = true,
  roots = { "~/Dev", "~/Projects" },
  scan_depth = 3,
  use_zoxide = true,
  worktree_root = nil, -- platform WezTerm data directory
  default_profile = "agentic",
  excluded_directories = { "node_modules", "target", ".cache" },

  profiles = {
    agentic = {
      -- Exact layout tree is intentionally deferred.
      layout = { ... },
      apps = {
        editor = { argv = { "nvim", "." } },
        agent = {
          argv = { "pi", "--continue" },
          fresh_argv = { "pi" },
        },
        review = {
          argv = { "tuicr", "--working-tree", "--no-update-check" },
        },
      },
    },
  },
}
```

Configuration rules:

- user paths expand `~` and environment variables using existing WezTerm conventions;
- roots and worktree root are canonicalized;
- argv arrays are mandatory; string shell commands are rejected;
- required app executables are resolved before Git mutation;
- duplicate roles or missing required roles are configuration errors;
- the feature is inactive when disabled or no valid roots/recents exist; and
- zoxide absence is a non-fatal diagnostic.

## 13. Security and safety

### 13.1 Trust boundaries

- Global user configuration may define roots and commands.
- Repository contents do not define executable workspace commands in v1.
- Any future project-local profile is loaded only after project trust is explicit.
- Local service requests use the existing same-user/native mux security model.
- Remote support must authenticate and execute on the owning remote mux, never reinterpret a remote path locally.

### 13.2 Process execution

- Commands use argv arrays and explicit cwd.
- No implicit `bash -lc` is used by the orchestrator.
- Environment additions are explicit and do not log secrets.
- Output is bounded and retained as operation diagnostics, not injected into agent context automatically.
- Operations have timeouts and cancellation tokens.

### 13.3 Filesystem deletion

Before removal, resolve and compare:

1. configured managed root;
2. registry path;
3. Git-reported worktree path; and
4. canonical filesystem path.

All must agree. The deletion operation is `git worktree remove <path>` after a clean-status check; it is not a recursive filesystem delete implemented by the GUI.

### 13.4 Git safeguards

Normal flows never use:

- `git worktree add --force`;
- `git worktree remove --force`;
- `git branch -D`;
- branch reset; or
- implicit deletion of untracked files.

## 14. Error model

Use stable machine-readable codes with user-facing context:

```text
unsupported_domain
project_not_found
not_a_git_repository
git_unavailable
git_command_failed
invalid_branch
branch_exists
branch_in_use
ambiguous_remote_branch
base_ref_not_found
worktree_path_conflict
path_outside_managed_root
workspace_already_live
workspace_reserved
workspace_degraded
app_not_found
spawn_failed
worktree_dirty
worktree_locked
worktree_unmanaged
running_processes
registry_conflict
registry_io
operation_cancelled
operation_timed_out
```

Errors carry:

- operation/stage;
- safe summary;
- relevant project/worktree/workspace IDs;
- bounded stderr or source error;
- whether retry is safe; and
- suggested recovery actions.

## 15. Recovery and reconciliation

Run reconciliation:

- when the service starts;
- after native mux restore;
- when the picker opens if state is stale;
- after a failed mutation; and
- before cleanup.

Reconciliation compares registry, Git, and mux state and classifies records:

```text
ready-live
ready-stopped
incomplete-worktree
incomplete-workspace
missing-worktree
unregistered-live
conflict
```

The UI offers only safe actions for each class: focus, retry launch, forget stale record, stop workspace, or remove a verified clean managed worktree.

## 16. Observability

Provide:

- structured logs with operation, project, workspace, and stage IDs;
- per-stage durations;
- catalog source counts and scan diagnostics;
- bounded Git/app stderr;
- registry reconciliation results;
- active operation status in the native modal; and
- a diagnostic CLI/API view that does not become the application transport.

Do not log full environment maps, prompt contents, Pi conversation data, or tuicr comment contents by default.

## 17. Testing

### 17.1 Unit tests

- parse valid and malformed NUL-delimited Git worktree/status/ref output;
- normalize candidate repositories and deduplicate identities;
- slug and managed-path containment, including symlinks and traversal attempts;
- deterministic catalog ranking;
- registry versioning, migration, atomic write failure, and reconciliation;
- plan preconditions and error codes;
- layout validation and deterministic split planning; and
- state-machine cancellation/compensation transitions.

### 17.2 Git integration tests

Use temporary repositories to cover:

- primary checkout discovery;
- multiple linked worktrees;
- new branch creation from a base ref;
- existing local branch attachment;
- remote-only tracking branch creation;
- branch already in use;
- path collision;
- detached HEAD;
- locked/prunable worktrees;
- dirty, staged, conflicted, and untracked cleanup refusal;
- clean managed removal without branch deletion; and
- concurrent duplicate launch requests.

### 17.3 Mux tests

- reserve/focus idempotency;
- workspace ownership conflicts;
- exact role metadata and cwd/env propagation;
- deterministic layout application;
- missing executable preflight;
- partial pane-spawn degradation;
- persistent mux GUI reconnect without process duplication; and
- registry reconciliation after native session restore.

### 17.4 GUI tests

- keyboard and mouse navigation;
- fuzzy filtering with incremental catalog updates;
- multi-step back/cancel behavior;
- plan preview and confirmation;
- progress/event reconnection; and
- cleanup refusal/recovery actions.

No screenshot-based assertion is required. Test modal state and computed elements directly where possible.

### 17.5 Integration validation

The first complete workflow is validated when a user can:

1. open the picker;
2. find a project from each enabled source;
3. attach its primary or existing worktree;
4. create a new branch worktree;
5. attach an existing local and unambiguous remote branch;
6. receive one workspace with editor/agent/review role panes in the selected cwd;
7. reopen the picker and focus the same live workspace without duplicates;
8. restart the GUI and retain live processes through the persistent mux;
9. recreate a stopped workspace with Pi continuation;
10. review changes and have Pi retrieve saved tuicr comments; and
11. stop the workspace and remove only a clean managed worktree without deleting its branch.

## 18. Implementation phases

### Phase 0: Contract and unresolved layout decision

- Approve this RFC.
- Select the default layout topology and ratios.
- Confirm configuration naming and default keybinding.

### Phase 1: Project/worktree core

- Add the project-workspace crate and typed errors.
- Add config, platform state/data paths, registry, and locks.
- Implement configured-root, recent, and zoxide catalog sources.
- Implement Git identity, worktree, refs, and status adapters.
- Add unit and temporary-repository tests.

### Phase 2: Native picker and plans

- Add `ShowProjectWorkspacePicker`.
- Implement incremental project selection and worktree/ref flows.
- Add plan previews, confirmation, progress, and cancellation.
- Keep mutation behind a fake service until UI flows are testable.

### Phase 3: Workspace provisioning

- Add typed codec/service operations.
- Implement worktree create/attach and mux workspace reservation.
- Implement configured layout application and role metadata/env.
- Add live focus idempotency and degraded-workspace recovery.

### Phase 4: Resume and cleanup

- Reconcile registry, Git, mux, and native session restore.
- Add role-specific resume commands.
- Add stop and guarded clean managed-worktree removal.
- Add concurrency and failure-injection tests.

### Phase 5: Pi and tuicr integration

- Publish workspace/Pi lifecycle metadata through `pi-wezterm`.
- Name new Pi sessions without overwriting continued names.
- Add explicit review-session discovery/comment retrieval helpers.
- Validate user-led review end to end.

### Phase 6: Remote domain executor

- Route catalog/Git/app operations to the owning native SSH mux.
- Store remote paths only with their domain identity.
- Reuse plans, events, layout, and application adapters unchanged.

## 19. Definition of done

The complete local feature is done when:

1. the picker is fully native and works before Pi starts;
2. project results merge roots, recents, and optional zoxide without duplicates;
3. all selected worktree operations are supported without force semantics;
4. each worktree maps idempotently to one development workspace;
5. Neovim, Pi, and tuicr launch with the same domain and cwd;
6. GUI reconnect focuses existing processes instead of relaunching them;
7. stopped workspaces restore with role-specific continuation where available;
8. cleanup cannot remove dirty, running, primary, locked, or unmanaged worktrees;
9. branch deletion is never an implicit side effect;
10. Pi can identify its workspace and retrieve user-authored tuicr comments through stable interfaces;
11. state writes are versioned, private, atomic, and recoverable;
12. operations are typed, cancellable, observable, and tested; and
13. the design can add a remote executor without changing project/workspace identity or UI contracts.

## 20. Open decisions before pane provisioning

These choices are intentionally deferred and must be resolved before Phase 3:

- default pane topology and split ratios;
- default keybinding for the picker;
- whether the review pane starts visible at all terminal widths or may begin stacked/hidden on narrow windows;
- default Neovim session command, if anything beyond `nvim .` is desired; and
- whether cleanup closes tuicr/Pi gracefully with an application callback before PTY termination.
