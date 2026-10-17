//! Copy (or symlink) untracked files from the main checkout into a new
//! worktree: `.env*`, local config, big dependency folders.
//!
//! Candidates come from `git ls-files --others --directory`, so only
//! untracked paths (ignored ones included) are considered, `.git` never is,
//! and untracked folders arrive collapsed (`node_modules/`). A pattern
//! reaches inside such a folder only by naming it (`apps/web/.env`), so
//! `**/.env` never walks a dependency tree. Existing paths in the worktree
//! are never overwritten.

use std::path::{Path, PathBuf};
use std::process::Command;

use globset::{Glob, GlobBuilder, GlobMatcher};
use zeron_proto::WorktreeCopyRule;

/// Copies done for one worktree (relative paths, `/`-separated).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CopyReport {
    pub copied: Vec<String>,
    pub linked: Vec<String>,
}

const MAX_MATCHES: usize = 2_000;
const MAX_NESTED_ENTRIES: usize = 20_000;

struct Rule {
    matcher: GlobMatcher,
    pattern: String,
    symlink: bool,
}

pub fn copy_untracked(
    main: &Path,
    worktree: &Path,
    rules: &[WorktreeCopyRule],
) -> Result<CopyReport, String> {
    if rules.is_empty() {
        return Ok(CopyReport::default());
    }
    let rules = rules
        .iter()
        .map(|rule| {
            let matcher = glob(&rule.glob)?.compile_matcher();
            Ok(Rule {
                matcher,
                pattern: rule.glob.trim_end_matches('/').to_string(),
                symlink: rule.symlink,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut matches: Vec<(String, bool)> = Vec::new();
    for entry in untracked(main)? {
        let (relative, is_dir) = match entry.strip_suffix('/') {
            Some(dir) => (dir.to_string(), true),
            None => (entry, false),
        };
        if is_git_metadata(&relative) {
            continue;
        }
        if let Some(rule) = rules.iter().find(|rule| rule.matcher.is_match(&relative)) {
            matches.push((relative, rule.symlink));
        } else if is_dir {
            for rule in rules
                .iter()
                .filter(|rule| names_inside(&rule.pattern, &relative))
            {
                for nested in nested_matches(main, &relative, &rule.matcher)? {
                    matches.push((nested, rule.symlink));
                }
            }
        }
        if matches.len() > MAX_MATCHES {
            return Err(format!(
                "Copy patterns match more than {MAX_MATCHES} paths; narrow them or copy a folder"
            ));
        }
    }
    let mut report = CopyReport::default();
    for (relative, symlink) in matches {
        let source = main.join(&relative);
        let target = worktree.join(&relative);
        if target.symlink_metadata().is_ok() {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        if symlink {
            link(&source, &target)
                .map_err(|error| format!("could not link {relative}: {error}"))?;
            report.linked.push(relative);
        } else {
            copy_path(&source, &target)
                .map_err(|error| format!("could not copy {relative}: {error}"))?;
            report.copied.push(relative);
        }
    }
    Ok(report)
}

fn glob(pattern: &str) -> Result<Glob, String> {
    GlobBuilder::new(pattern.trim_end_matches('/'))
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map_err(|error| format!("invalid copy pattern \"{pattern}\": {error}"))
}

fn is_git_metadata(relative: &str) -> bool {
    relative
        .split('/')
        .any(|part| part.eq_ignore_ascii_case(".git"))
}

/// The pattern's literal leading folders enter `folder`.
fn names_inside(pattern: &str, folder: &str) -> bool {
    let literal: Vec<&str> = pattern
        .split('/')
        .take_while(|part| !part.contains(['*', '?', '[', '{']))
        .collect();
    let folder: Vec<&str> = folder.split('/').collect();
    literal.len() > folder.len() && literal[..folder.len()] == folder[..]
}

fn untracked(main: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args(["ls-files", "--others", "--directory", "-z"])
        .current_dir(main)
        .stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("could not list untracked files: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "could not list untracked files: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .collect())
}

fn nested_matches(main: &Path, folder: &str, matcher: &GlobMatcher) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    let mut pending = vec![folder.to_string()];
    let mut visited = 0usize;
    while let Some(relative) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(main.join(&relative)) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_NESTED_ENTRIES {
                return Err(format!("{folder} is too large to search for copy patterns"));
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.eq_ignore_ascii_case(".git") {
                continue;
            }
            let child = format!("{relative}/{name}");
            if matcher.is_match(&child) {
                found.push(child);
            } else if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(child);
            }
        }
    }
    Ok(found)
}

fn copy_path(source: &Path, target: &Path) -> std::io::Result<()> {
    let metadata = source.symlink_metadata()?;
    if metadata.file_type().is_symlink() {
        return link(&std::fs::read_link(source)?, target);
    }
    if !metadata.is_dir() {
        std::fs::copy(source, target)?;
        return Ok(());
    }
    std::fs::create_dir(target)?;
    let mut pending: Vec<(PathBuf, PathBuf)> = vec![(source.to_path_buf(), target.to_path_buf())];
    while let Some((from, to)) = pending.pop() {
        for entry in std::fs::read_dir(&from)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_string_lossy().eq_ignore_ascii_case(".git") {
                continue;
            }
            let (child_from, child_to) = (entry.path(), to.join(&name));
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                link(&std::fs::read_link(&child_from)?, &child_to)?;
            } else if kind.is_dir() {
                std::fs::create_dir(&child_to)?;
                pending.push((child_from, child_to));
            } else {
                std::fs::copy(&child_from, &child_to)?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn link(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(windows)]
fn link(source: &Path, target: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(source, target)
    } else {
        std::os::windows::fs::symlink_file(source, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn rule(glob: &str, symlink: bool) -> WorktreeCopyRule {
        WorktreeCopyRule {
            glob: glob.into(),
            symlink,
        }
    }

    #[test]
    fn copies_untracked_matches_links_folders_and_never_overwrites() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let worktree = temp.path().join("worktree");
        std::fs::create_dir_all(main.join("apps/web")).unwrap();
        std::fs::create_dir_all(main.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(main.join("scratch")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        git(&main, &["init", "-q"]);
        std::fs::write(main.join(".gitignore"), "node_modules\n.env*\n").unwrap();
        std::fs::write(main.join("tracked.txt"), "tracked").unwrap();
        std::fs::write(main.join("apps/web/index.js"), "x").unwrap();
        git(&main, &["add", "."]);
        std::fs::write(main.join(".env"), "SECRET=1").unwrap();
        std::fs::write(main.join(".env.local"), "LOCAL=1").unwrap();
        std::fs::write(main.join("apps/web/.env"), "WEB=1").unwrap();
        std::fs::write(main.join("node_modules/pkg/index.js"), "module").unwrap();
        std::fs::write(main.join("node_modules/pkg/.env"), "nested").unwrap();
        std::fs::write(main.join("scratch/.env"), "scratch").unwrap();
        std::fs::write(worktree.join(".env.local"), "KEEP").unwrap();

        let report = copy_untracked(
            &main,
            &worktree,
            &[
                rule(".env*", false),
                rule("**/.env", false),
                rule("node_modules", true),
                rule("tracked.txt", false),
            ],
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(worktree.join(".env")).unwrap(),
            "SECRET=1"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join(".env.local")).unwrap(),
            "KEEP"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join("apps/web/.env")).unwrap(),
            "WEB=1"
        );
        assert!(
            !worktree.join("tracked.txt").exists(),
            "tracked files are not copied"
        );
        assert!(
            !worktree.join("scratch").exists(),
            "** stays out of untracked folders"
        );
        let link = worktree.join("node_modules");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(link.join("pkg/index.js")).unwrap(),
            "module"
        );
        assert_eq!(report.linked, ["node_modules"]);
        assert!(report.copied.contains(&".env".to_string()));
    }

    #[test]
    fn a_named_folder_pattern_reaches_inside_untracked_folders() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let worktree = temp.path().join("worktree");
        std::fs::create_dir_all(main.join("config/local")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        git(&main, &["init", "-q"]);
        std::fs::write(main.join("config/local/secrets.json"), "{}").unwrap();
        std::fs::write(main.join("config/local/other.txt"), "no").unwrap();
        copy_untracked(&main, &worktree, &[rule("config/local/*.json", false)]).unwrap();
        assert!(worktree.join("config/local/secrets.json").is_file());
        assert!(!worktree.join("config/local/other.txt").exists());

        copy_untracked(&main, &worktree, &[rule("config", false)]).unwrap();
        assert!(
            !worktree.join("config/local/other.txt").exists(),
            "existing folders are kept"
        );
    }
}
