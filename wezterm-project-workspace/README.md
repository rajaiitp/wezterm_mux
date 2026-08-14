# wezterm-project-workspace

Typed local project/worktree primitives for the native WezTerm development
workspace picker.

The crate provides:

- Git repository/worktree discovery using machine-readable porcelain output;
- configured-root, recent, and optional zoxide project catalog merging;
- stable domain-aware project/workspace IDs;
- central managed-worktree path generation and containment checks;
- validated new-branch and existing-branch worktree plans;
- role-based editor/agent/review layout profiles; and
- a versioned, atomic, private workspace registry.

It intentionally contains no GUI or mux code. `wezterm-gui` owns native modal
interaction and launches the provisional `agentic` profile (`nvim`, `pi
--continue`, and `tuicr --working-tree`) in the selected worktree.
