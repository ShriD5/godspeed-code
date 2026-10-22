//! Finds the git repos a conductor covers inside its folder.

use std::path::{Path, PathBuf};

/// The folder itself when it holds a `.git` (directory, or the file a linked
/// worktree has), else its direct children that do. Sorted, one level deep;
/// hidden directories and symlinks are skipped.
pub fn discover_repos(folder: &Path) -> Vec<PathBuf> {
    if is_repo(folder) {
        return vec![folder.to_path_buf()];
    }
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut repos: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            let hidden = entry.file_name().to_string_lossy().starts_with('.');
            let plain_dir = entry.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink());
            !hidden && plain_dir
        })
        .map(|entry| entry.path())
        .filter(|path| is_repo(path))
        .collect();
    repos.sort();
    repos
}

fn is_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(parent: &Path, name: &str) -> PathBuf {
        let path = parent.join(name);
        std::fs::create_dir_all(path.join(".git")).unwrap();
        path
    }

    #[test]
    fn a_folder_that_is_a_repo_is_the_only_repo() {
        let dir = tempfile::tempdir().unwrap();
        let root = repo(dir.path(), "solo");
        repo(&root, "nested");
        assert_eq!(discover_repos(&root), vec![root]);
    }

    #[test]
    fn a_parent_folder_lists_its_repos_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let web = repo(dir.path(), "web");
        let api = repo(dir.path(), "api");
        std::fs::create_dir(dir.path().join("docs")).unwrap();
        assert_eq!(discover_repos(dir.path()), vec![api, web]);
    }

    #[test]
    fn an_empty_or_missing_folder_has_no_repos() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover_repos(dir.path()).is_empty());
        assert!(discover_repos(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn hidden_directories_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path(), ".cache");
        assert!(discover_repos(dir.path()).is_empty());
    }

    #[test]
    fn a_git_file_from_a_linked_worktree_counts() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /elsewhere").unwrap();
        assert_eq!(discover_repos(dir.path()), vec![wt]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        let target = repo(real.path(), "linked");
        std::os::unix::fs::symlink(&target, dir.path().join("link")).unwrap();
        assert!(discover_repos(dir.path()).is_empty());
    }
}
