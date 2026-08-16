# wezterm-project-workspace

Typed local project/worktree primitives for the native WezTerm development
workspace picker.

The crate provides:

- Git repository/worktree discovery using machine-readable porcelain output;
- configured-root, recent, and optional zoxide project catalog merging;
- stable domain-aware project/workspace IDs;
- central managed-worktree path generation and containment checks;
- validated new-branch and existing-branch worktree plans;
- role-based module profiles with named `single`, `columns`, `rows`,
  `three-pane`, and `grid` layouts; and
- a versioned, atomic, private workspace registry.

It intentionally contains no GUI or mux code. `wezterm-gui` owns native modal
interaction and submits module commands plus the named layout to the mux. The
mux owns all split sizing and pane geometry. The provided dotfile configuration
opens `pi --continue` and `nvim .` in the `columns` layout; tuicr is available
as a separate app toggle unless explicitly configured as a module.
