use crate::git::GitRepository;
use crate::types::{ProjectId, ProjectIdentity};
use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CatalogSource {
    ConfiguredRoot,
    Recent,
    Zoxide,
}

#[derive(Debug, Clone)]
pub struct CatalogCandidate {
    pub project: ProjectIdentity,
    pub sources: BTreeSet<CatalogSource>,
    pub score: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectCatalog {
    entries: Vec<CatalogCandidate>,
}

impl ProjectCatalog {
    /// Discover projects from configured roots, recent workspace paths, and
    /// optional zoxide paths. Invalid or stale candidates are skipped; the
    /// caller can separately surface source diagnostics if desired.
    pub fn discover(
        configured_roots: &[PathBuf],
        recent_paths: &[PathBuf],
        zoxide_paths: &[PathBuf],
        max_depth: usize,
        excluded_directories: &[String],
    ) -> Self {
        let mut candidates = BTreeMap::<ProjectId, CatalogCandidate>::new();
        let exclusions = excluded_directories.iter().collect::<BTreeSet<_>>();

        for root in configured_roots {
            let mut paths = Vec::new();
            collect_repositories(root, 0, max_depth, &exclusions, &mut paths);
            for path in paths {
                add_candidate(&mut candidates, &path, CatalogSource::ConfiguredRoot, 300);
            }
        }
        for path in recent_paths {
            add_candidate(&mut candidates, path, CatalogSource::Recent, 500);
        }
        for (index, path) in zoxide_paths.iter().enumerate() {
            let score = 200u32.saturating_sub(index.min(199) as u32);
            add_candidate(&mut candidates, path, CatalogSource::Zoxide, score);
        }

        let mut entries: Vec<_> = candidates.into_values().collect();
        entries.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.project.primary_root.cmp(&b.project.primary_root))
        });
        Self { entries }
    }

    pub fn entries(&self) -> &[CatalogCandidate] {
        &self.entries
    }

    pub fn get(&self, id: &ProjectId) -> Option<&CatalogCandidate> {
        self.entries.iter().find(|entry| &entry.project.id == id)
    }

    /// Query zoxide without invoking a shell. A missing zoxide executable or a
    /// failed query is treated as an empty optional source by the picker.
    pub fn query_zoxide(limit: usize) -> anyhow::Result<Vec<PathBuf>> {
        let output = Command::new("zoxide")
            .args(["query", "--list"])
            .output()
            .context("run zoxide query")?;
        if !output.status.success() {
            return Ok(Vec::new());
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .take(limit)
            .map(PathBuf::from)
            .collect())
    }
}

fn add_candidate(
    candidates: &mut BTreeMap<ProjectId, CatalogCandidate>,
    path: &Path,
    source: CatalogSource,
    score: u32,
) {
    let Ok(repository) = GitRepository::discover(path) else {
        return;
    };
    let id = repository.identity.id.clone();
    if let Some(existing) = candidates.get_mut(&id) {
        existing.sources.insert(source);
        existing.score = existing.score.max(score);
        return;
    }
    let mut sources = BTreeSet::new();
    sources.insert(source);
    candidates.insert(
        id,
        CatalogCandidate {
            project: repository.identity,
            sources,
            score,
        },
    );
}

fn collect_repositories(
    root: &Path,
    depth: usize,
    max_depth: usize,
    exclusions: &BTreeSet<&String>,
    output: &mut Vec<PathBuf>,
) {
    if is_git_checkout(root) {
        output.push(root.to_path_buf());
        return;
    }
    if depth >= max_depth {
        return;
    }

    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| exclusions.iter().any(|excluded| excluded.as_str() == name))
        {
            continue;
        }
        collect_repositories(&path, depth + 1, max_depth, exclusions, output);
    }
}

fn is_git_checkout(path: &Path) -> bool {
    path.join(".git").is_dir() || path.join(".git").is_file()
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

    #[test]
    fn merges_sources_by_git_common_directory() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q", "-b", "main"]);
        run_git(&repo, &["config", "user.email", "test@example.com"]);
        run_git(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("README"), "hello").unwrap();
        run_git(&repo, &["add", "README"]);
        run_git(&repo, &["commit", "-qm", "initial"]);

        let catalog = ProjectCatalog::discover(
            &[dir.path().to_path_buf()],
            std::slice::from_ref(&repo),
            std::slice::from_ref(&repo),
            2,
            &["node_modules".to_string()],
        );
        assert_eq!(catalog.entries().len(), 1);
        let entry = &catalog.entries()[0];
        assert!(entry.sources.contains(&CatalogSource::ConfiguredRoot));
        assert!(entry.sources.contains(&CatalogSource::Recent));
        assert!(entry.sources.contains(&CatalogSource::Zoxide));
    }

    #[test]
    fn repository_scan_does_not_descend_into_excluded_directories() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("node_modules/nested/.git")).unwrap();
        let mut found = Vec::new();
        let exclusion_names = ["node_modules".to_string()];
        let exclusions = exclusion_names.iter().collect::<BTreeSet<_>>();
        collect_repositories(&root, 0, 5, &exclusions, &mut found);
        assert!(found.is_empty());
    }
}
