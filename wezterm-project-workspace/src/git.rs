use crate::types::{DomainKey, ProjectId, ProjectIdentity};
use anyhow::{bail, Context};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug, Clone)]
pub struct GitRepository {
    pub identity: ProjectIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub locked: Option<String>,
    pub prunable: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitWorktreeState {
    pub clean: bool,
    pub has_staged_changes: bool,
    pub has_unstaged_changes: bool,
    pub has_untracked_files: bool,
    pub has_conflicts: bool,
}

impl GitRepository {
    pub fn discover(path: &Path) -> anyhow::Result<Self> {
        let domain = DomainKey::local();
        let common_dir = git_text(
            path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let primary_root = git_text(path, &["rev-parse", "--show-toplevel"])?;
        let common_dir = canonicalize_or_absolute(Path::new(&common_dir))?;
        let primary_root = canonicalize_or_absolute(Path::new(&primary_root))?;
        let id = ProjectId::from_common_dir(&domain, &common_dir);

        Ok(Self {
            identity: ProjectIdentity {
                id,
                domain,
                common_dir,
                primary_root,
            },
        })
    }

    pub fn worktrees(&self) -> anyhow::Result<Vec<GitWorktree>> {
        let output = git_output(
            &self.identity.primary_root,
            &["worktree", "list", "--porcelain", "-z"],
        )?;
        parse_worktree_list(&output.stdout)
    }

    pub fn status(&self, worktree: &Path) -> anyhow::Result<GitWorktreeState> {
        let output = git_output(worktree, &["status", "--porcelain=v2", "-z", "--branch"])?;
        parse_status(&output.stdout)
    }

    pub fn validate_branch_name(&self, branch: &str) -> anyhow::Result<()> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.identity.primary_root)
            .args(["check-ref-format", "--branch", branch])
            .output()
            .context("run git check-ref-format")?;
        if output.status.success() {
            Ok(())
        } else {
            bail!("invalid branch name {:?}", branch)
        }
    }

    pub fn local_branch_exists(&self, branch: &str) -> anyhow::Result<bool> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.identity.primary_root)
            .args(["show-ref", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}"))
            .output()
            .context("run git show-ref")?;
        if output.status.success() {
            Ok(true)
        } else if output.status.code() == Some(1) {
            Ok(false)
        } else {
            bail!("git show-ref failed with status {}", output.status)
        }
    }

    pub fn add_worktree(
        &self,
        destination: &Path,
        branch: &str,
        base_ref: &str,
    ) -> anyhow::Result<()> {
        self.validate_branch_name(branch)?;
        if self.local_branch_exists(branch)? {
            bail!("local branch {branch:?} already exists");
        }
        if destination.exists() {
            bail!(
                "worktree destination {} already exists",
                destination.display()
            );
        }
        let parent = destination
            .parent()
            .context("worktree destination has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create worktree parent {}", parent.display()))?;
        run_git(
            &self.identity.primary_root,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("-b"),
                OsStr::new(branch),
                destination.as_os_str(),
                OsStr::new(base_ref),
            ],
        )
    }
}

pub fn parse_worktree_list(output: &[u8]) -> anyhow::Result<Vec<GitWorktree>> {
    let mut worktrees = Vec::new();
    let mut current: Option<GitWorktree> = None;

    for field in output.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            continue;
        }

        if let Some(path) = field.strip_prefix(b"worktree ") {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            current = Some(GitWorktree {
                path: path_to_pathbuf(path),
                head: None,
                branch: None,
                locked: None,
                prunable: None,
            });
            continue;
        }

        let worktree = current
            .as_mut()
            .context("Git worktree record starts before a worktree path")?;
        if let Some(head) = field.strip_prefix(b"HEAD ") {
            worktree.head = Some(text(head)?);
        } else if let Some(branch) = field.strip_prefix(b"branch refs/heads/") {
            worktree.branch = Some(text(branch)?);
        } else if field == b"locked" {
            worktree.locked = Some(String::new());
        } else if let Some(reason) = field.strip_prefix(b"locked ") {
            worktree.locked = Some(text(reason)?);
        } else if field == b"prunable" {
            worktree.prunable = Some(String::new());
        } else if let Some(reason) = field.strip_prefix(b"prunable ") {
            worktree.prunable = Some(text(reason)?);
        }
    }
    if let Some(worktree) = current {
        worktrees.push(worktree);
    }

    if worktrees.is_empty() && !output.is_empty() {
        bail!("Git worktree output contained no worktree records");
    }
    Ok(worktrees)
}

fn parse_status(output: &[u8]) -> anyhow::Result<GitWorktreeState> {
    let mut state = GitWorktreeState {
        clean: true,
        has_staged_changes: false,
        has_unstaged_changes: false,
        has_untracked_files: false,
        has_conflicts: false,
    };

    for field in output.split(|byte| *byte == 0) {
        if field.is_empty() || field.starts_with(b"# ") {
            continue;
        }
        state.clean = false;
        if field.starts_with(b"? ") {
            state.has_untracked_files = true;
        } else if field.starts_with(b"u ") {
            state.has_conflicts = true;
            state.has_staged_changes = true;
            state.has_unstaged_changes = true;
        } else if let Some(payload) = field
            .strip_prefix(b"1 ")
            .or_else(|| field.strip_prefix(b"2 "))
        {
            let xy = payload
                .split(|byte| *byte == b' ')
                .next()
                .unwrap_or_default();
            if xy.first().is_some_and(|byte| *byte != b'.') {
                state.has_staged_changes = true;
            }
            if xy.get(1).is_some_and(|byte| *byte != b'.') {
                state.has_unstaged_changes = true;
            }
        } else {
            state.has_unstaged_changes = true;
        }
    }
    Ok(state)
}

fn git_text(path: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = git_output(path, args)?;
    String::from_utf8(output.stdout)
        .context("Git returned non-UTF-8 metadata")
        .map(|text| text.trim_end_matches(['\r', '\n']).to_string())
}

fn git_output(path: &Path, args: &[&str]) -> anyhow::Result<Output> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .with_context(|| format!("run git in {}", path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            path.display(),
            stderr.trim()
        );
    }
    Ok(output)
}

fn run_git<I, S>(path: &Path, args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .with_context(|| format!("run git in {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "git worktree add failed in {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn text(bytes: &[u8]) -> anyhow::Result<String> {
    String::from_utf8(bytes.to_vec()).context("Git metadata contained non-UTF-8 text")
}

fn path_to_pathbuf(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        return PathBuf::from(OsString::from_vec(bytes.to_vec()));
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(OsString::from(String::from_utf8_lossy(bytes).into_owned()))
    }
}

fn canonicalize_or_absolute(path: &Path) -> anyhow::Result<PathBuf> {
    if path.exists() {
        return Ok(std::fs::canonicalize(path)?);
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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

    #[test]
    fn parses_nul_delimited_worktree_records() {
        let input = b"worktree /tmp/main\0HEAD abc\0branch refs/heads/main\0\0worktree /tmp/feature one\0HEAD def\0branch refs/heads/feature/x\0locked reason\0\0";
        let worktrees = parse_worktree_list(input).unwrap();
        assert_eq!(worktrees.len(), 2);
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert_eq!(worktrees[1].path, PathBuf::from("/tmp/feature one"));
        assert_eq!(worktrees[1].locked.as_deref(), Some("reason"));
    }

    #[test]
    fn parses_dirty_status_categories() {
        let input = b"# branch.oid abc\0# branch.head main\01 M. N... 100644 100644 100644 a b file\0? untracked\0u UU 100644 100644 100644 100644 a b c d conflict\0";
        let status = parse_status(input).unwrap();
        assert!(!status.clean);
        assert!(status.has_staged_changes);
        assert!(status.has_unstaged_changes);
        assert!(status.has_untracked_files);
        assert!(status.has_conflicts);
    }

    #[test]
    fn discovers_real_repository_and_worktree_state() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q", "-b", "main"]);
        run_git(&repo, &["config", "user.email", "test@example.com"]);
        run_git(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("README"), "hello").unwrap();
        run_git(&repo, &["add", "README"]);
        run_git(&repo, &["commit", "-qm", "initial"]);

        let repository = GitRepository::discover(&repo).unwrap();
        assert_eq!(
            repository.identity.primary_root,
            fs::canonicalize(&repo).unwrap()
        );
        let worktrees = repository.worktrees().unwrap();
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert!(repository.status(&repo).unwrap().clean);

        fs::write(repo.join("README"), "changed").unwrap();
        let status = repository.status(&repo).unwrap();
        assert!(!status.clean);
        assert!(status.has_unstaged_changes);
    }
}
