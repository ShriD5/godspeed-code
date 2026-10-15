//! Generated commit messages, pull request text and branch names.
//!
//! Every generation runs through [`crate::titles::generate_text`], so the
//! device's one text-model choice (Settings → Thread naming) governs titles
//! and git text alike. Prompts carry the change itself (capped, summarized
//! per file when large) and the repository's recent commit subjects, so the
//! result matches the house style — conventional prefixes when the history
//! uses them. Agent credit trailers are stripped here too, for generated and
//! hand-written messages alike, when the device setting asks for it.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use zeron_proto::HarnessId;

use crate::registry::HarnessRegistry;

/// Characters of diff a prompt carries before it switches to a per-file
/// summary. Large enough for a typical commit verbatim, small enough for a
/// cheap model's context and latency.
pub const DIFF_PROMPT_BUDGET: usize = 24_000;
/// Per-file excerpt bounds once a diff is summarized.
const FILE_EXCERPT_MIN: usize = 400;
const FILE_EXCERPT_MAX: usize = 4_000;
/// Recent subjects shown as a style reference.
pub const STYLE_SUBJECTS: usize = 20;
/// Commits listed in a pull request prompt.
const PR_COMMITS_MAX: usize = 50;
const TEMPLATE_MAX_BYTES: usize = 8 * 1024;
const GENERATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Lockfiles and other machine-written files: their content never helps a
/// message, so a summary lists them by size only.
const GENERATED_FILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
    "Gemfile.lock",
    "poetry.lock",
    "uv.lock",
    "go.sum",
    "composer.lock",
    "Podfile.lock",
];

const CONVENTIONAL_TYPES: &[&str] = &[
    "feat", "fix", "chore", "docs", "refactor", "test", "tests", "perf", "build", "ci", "style",
    "revert",
];

/// Names and email tokens that identify a coding agent in a trailer.
const AGENT_TOKENS: &[&str] = &[
    "claude",
    "anthropic",
    "codex",
    "openai",
    "chatgpt",
    "copilot",
    "cursor",
    "cursoragent",
    "gemini",
    "jules",
    "aider",
    "devin",
    "opencode",
    "windsurf",
    "ampcode",
    "droid",
    "godspeed",
];

/// Inputs for a pull request prompt.
#[derive(Debug, Clone, Default)]
pub struct PullRequestPrompt {
    pub base: String,
    pub head: String,
    /// Commit subjects on the branch, oldest first.
    pub commits: Vec<String>,
    /// `base...head` patch (summarized by the prompt builder).
    pub diff: String,
    /// Recent subjects on the base, for title style.
    pub style_subjects: Vec<String>,
    /// The repository's pull request template, if any.
    pub template: Option<String>,
}

/// Commit, pull request and branch text on the device's text model.
#[derive(Clone)]
pub struct TextGenerator {
    registry: Arc<HarnessRegistry>,
    /// The agent of a chat on this checkout: used, like for titles, when no
    /// text harness is configured.
    session_harness: Option<HarnessId>,
}

impl TextGenerator {
    pub fn new(registry: Arc<HarnessRegistry>) -> Self {
        Self {
            registry,
            session_harness: None,
        }
    }

    pub fn with_session_harness(mut self, harness: Option<HarnessId>) -> Self {
        self.session_harness = harness;
        self
    }

    /// The device's "remove agent credits" preference (default on).
    pub fn remove_agent_credits(&self) -> bool {
        self.registry.title_settings().remove_agent_credits
    }

    /// Apply the credit preference to text bound for a commit or PR body.
    pub fn finish(&self, text: &str) -> String {
        if self.remove_agent_credits() {
            strip_agent_credits(text)
        } else {
            text.trim().to_string()
        }
    }

    /// A commit message for `diff` (the staged patch), or `None` when no text
    /// model is available or every attempt came back empty.
    pub async fn commit_message(&self, diff: &str, recent_subjects: &[String]) -> Option<String> {
        let prompt = commit_prompt(diff, recent_subjects);
        let message = crate::titles::generate_text(
            &self.registry,
            self.session_harness,
            &prompt,
            GENERATION_TIMEOUT,
            clean_commit_message,
        )
        .await?;
        Some(self.finish(&message)).filter(|m| !m.is_empty())
    }

    /// A pull request `(title, body)`.
    pub async fn pull_request(&self, input: &PullRequestPrompt) -> Option<(String, String)> {
        let prompt = pull_request_prompt(input);
        let raw = crate::titles::generate_text(
            &self.registry,
            self.session_harness,
            &prompt,
            GENERATION_TIMEOUT,
            |raw| {
                if parse_pull_request(raw).0.is_empty() {
                    String::new()
                } else {
                    raw.trim().to_string()
                }
            },
        )
        .await?;
        let (title, body) = parse_pull_request(&raw);
        Some((self.finish(&title), self.finish(&body)))
    }

    /// A `godspeed/<slug>` branch name describing `description` (a task, a
    /// diff summary, or commit subjects).
    pub async fn branch_name(&self, description: &str) -> Option<String> {
        let prompt = format!(
            "Task: name a git branch for the work described below. Return only 2-5 lowercase words separated by hyphens (for example fix-login-redirect), with no prefix, quotes or punctuation.\n\nWork description (JSON string):\n{}",
            serde_json::to_string(&truncate_chars(description, DIFF_PROMPT_BUDGET)).ok()?
        );
        crate::titles::generate_text(
            &self.registry,
            self.session_harness,
            &prompt,
            Duration::from_secs(30),
            |raw| {
                let first = raw.trim().lines().next().unwrap_or("").trim();
                if first.is_empty() {
                    String::new()
                } else {
                    let slug = first.trim_start_matches("godspeed/");
                    crate::repos::worktree_branch_from_title(slug)
                }
            },
        )
        .await
    }
}

/// The commit message prompt.
pub fn commit_prompt(diff: &str, recent_subjects: &[String]) -> String {
    let mut prompt = String::from(
        "Task: write a git commit message for the staged changes below.\n\
         Format: a subject line of at most 72 characters in the imperative mood, then optionally a blank line and a short body wrapped at 72 characters that explains why, only when the change is not obvious. No trailers, no co-author lines, no code fences, no quotes.\n",
    );
    prompt.push_str(&style_instruction(recent_subjects));
    push_subjects(&mut prompt, recent_subjects);
    prompt.push_str("\nStaged changes (git diff --cached):\n");
    prompt.push_str(&summarize_diff(diff, DIFF_PROMPT_BUDGET));
    prompt
}

/// The pull request prompt.
pub fn pull_request_prompt(input: &PullRequestPrompt) -> String {
    let mut prompt = format!(
        "Task: write a pull request title and description for merging branch `{}` into `{}`.\n\
         Format: the first line is the title, at most 72 characters, no trailing period. Then one blank line, then the description in GitHub Markdown. No co-author lines and no \"generated with\" credits.\n",
        input.head, input.base
    );
    prompt.push_str(&style_instruction(&input.style_subjects));
    match &input.template {
        Some(template) => {
            prompt.push_str(
                "The repository has a pull request template, quoted below. Write the description by filling in that template: keep its headings and checklists, replace its placeholder comments with real content, and leave unchecked any box you cannot verify from the changes.\n\nTemplate:\n",
            );
            prompt.push_str(template);
            prompt.push('\n');
        }
        None => prompt.push_str(
            "Description: a short summary paragraph, then a bulleted list of the notable changes. Mention testing only when the commits show it.\n",
        ),
    }
    if !input.commits.is_empty() {
        prompt.push_str("\nCommits on the branch (oldest first):\n");
        let skip = input.commits.len().saturating_sub(PR_COMMITS_MAX);
        for subject in input.commits.iter().skip(skip) {
            prompt.push_str("- ");
            prompt.push_str(subject);
            prompt.push('\n');
        }
    }
    prompt.push_str("\nChanges against the base:\n");
    prompt.push_str(&summarize_diff(&input.diff, DIFF_PROMPT_BUDGET));
    prompt
}

fn style_instruction(subjects: &[String]) -> String {
    if uses_conventional_commits(subjects) {
        "Style: this repository uses Conventional Commits; start the subject with a type prefix such as feat:, fix:, chore:, docs:, refactor: or test: (with a scope in parentheses when the history uses scopes), followed by a lowercase imperative summary.\n".into()
    } else if subjects.is_empty() {
        "Style: a capitalized imperative summary.\n".into()
    } else {
        "Style: match the capitalization, tense and prefixes of the recent subjects below.\n"
            .into()
    }
}

fn push_subjects(prompt: &mut String, subjects: &[String]) {
    if subjects.is_empty() {
        return;
    }
    prompt.push_str("\nRecent commit subjects (style reference only, newest first):\n");
    for subject in subjects.iter().take(STYLE_SUBJECTS) {
        prompt.push_str("- ");
        prompt.push_str(subject);
        prompt.push('\n');
    }
}

/// Whether the history follows Conventional Commits: at least two subjects,
/// and at least 40% of them, carry a `type(scope)!:` prefix.
pub fn uses_conventional_commits(subjects: &[String]) -> bool {
    let matching = subjects
        .iter()
        .filter(|subject| is_conventional(subject))
        .count();
    matching >= 2 && matching * 5 >= subjects.len() * 2
}

fn is_conventional(subject: &str) -> bool {
    let Some((head, rest)) = subject.split_once(':') else {
        return false;
    };
    if !rest.starts_with(' ') {
        return false;
    }
    let head = head.strip_suffix('!').unwrap_or(head);
    let kind = match head.split_once('(') {
        Some((kind, scope)) => {
            if !scope.ends_with(')') || scope.len() < 2 {
                return false;
            }
            kind
        }
        None => head,
    };
    CONVENTIONAL_TYPES.contains(&kind.to_ascii_lowercase().as_str())
}

/// The patch verbatim when it fits `budget`; otherwise a per-file summary:
/// counts for every file, plus an excerpt of each file's hunks while the
/// budget lasts. Lockfiles are only ever counted.
pub fn summarize_diff(patch: &str, budget: usize) -> String {
    let patch = patch.trim_end();
    if patch.is_empty() {
        return "(no textual changes)\n".into();
    }
    let sections = split_sections(patch);
    let all_generated_fit = sections.iter().all(|s| !s.generated);
    if patch.len() <= budget && all_generated_fit {
        return format!("{patch}\n");
    }
    let additions: usize = sections.iter().map(|s| s.additions).sum();
    let deletions: usize = sections.iter().map(|s| s.deletions).sum();
    let mut out = format!(
        "(large diff summarized: {} files, +{additions} -{deletions})\n",
        sections.len()
    );
    let per_file = (budget / sections.len().max(1)).clamp(FILE_EXCERPT_MIN, FILE_EXCERPT_MAX);
    let mut remaining = budget;
    let mut listed_only = Vec::new();
    for section in &sections {
        if section.generated || remaining < FILE_EXCERPT_MIN {
            listed_only.push(section);
            continue;
        }
        let excerpt = excerpt_lines(section.body, per_file.min(remaining));
        remaining = remaining.saturating_sub(excerpt.len());
        out.push_str(&format!(
            "\n### {} (+{} -{})\n{}",
            section.path, section.additions, section.deletions, excerpt
        ));
        if excerpt.len() < section.body.len() {
            out.push_str("\n… (excerpt)\n");
        } else {
            out.push('\n');
        }
    }
    if !listed_only.is_empty() {
        out.push_str("\nOther changed files:\n");
        for section in listed_only {
            out.push_str(&format!(
                "- {} (+{} -{})\n",
                section.path, section.additions, section.deletions
            ));
        }
    }
    out
}

struct DiffSection<'a> {
    path: String,
    body: &'a str,
    additions: usize,
    deletions: usize,
    generated: bool,
}

fn split_sections(patch: &str) -> Vec<DiffSection<'_>> {
    let mut starts: Vec<usize> = patch
        .match_indices("diff --git ")
        .filter(|(ix, _)| *ix == 0 || patch.as_bytes()[ix - 1] == b'\n')
        .map(|(ix, _)| ix)
        .collect();
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    let mut sections = Vec::with_capacity(starts.len());
    for (n, start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(patch.len());
        let body = &patch[*start..end];
        let header = body.lines().next().unwrap_or("");
        let path = header
            .strip_prefix("diff --git ")
            .and_then(|rest| rest.rsplit_once(" b/").map(|(_, b)| b.to_string()))
            .unwrap_or_else(|| "(patch)".to_string());
        let (mut additions, mut deletions) = (0, 0);
        for line in body.lines() {
            if line.starts_with('+') && !line.starts_with("+++") {
                additions += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                deletions += 1;
            }
        }
        let file_name = path.rsplit('/').next().unwrap_or(&path);
        let generated = GENERATED_FILES.contains(&file_name);
        sections.push(DiffSection {
            path,
            body,
            additions,
            deletions,
            generated,
        });
    }
    sections
}

/// Whole lines from the start of `text`, at most `limit` bytes.
fn excerpt_lines(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text.trim_end();
    }
    let mut end = 0;
    for (ix, _) in text.match_indices('\n') {
        if ix > limit {
            break;
        }
        end = ix;
    }
    if end == 0 {
        // One very long line: cut on a char boundary.
        let mut cut = limit;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        end = cut;
    }
    &text[..end]
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Strip wrapping code fences, quotes and a "Commit message:" label.
pub fn clean_commit_message(raw: &str) -> String {
    let text = strip_fences(raw.trim());
    let mut lines: Vec<&str> = text.lines().collect();
    if let Some(first) = lines.first() {
        let lower = first.trim().to_ascii_lowercase();
        if lower == "commit message:" || lower == "commit message" {
            lines.remove(0);
        }
    }
    let joined = lines.join("\n");
    let mut message = joined.trim();
    if message.len() >= 2
        && ((message.starts_with('"') && message.ends_with('"'))
            || (message.starts_with('\'') && message.ends_with('\''))
            || (message.starts_with('`') && message.ends_with('`')))
    {
        message = message[1..message.len() - 1].trim();
    }
    message
        .strip_prefix("Commit message:")
        .unwrap_or(message)
        .trim()
        .to_string()
}

fn strip_fences(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split a generated reply into `(title, body)`: the first non-empty line,
/// minus `Title:`/heading dressing, then the rest.
pub fn parse_pull_request(raw: &str) -> (String, String) {
    let text = strip_fences(raw.trim());
    let mut lines = text.lines().skip_while(|line| line.trim().is_empty());
    let Some(first) = lines.next() else {
        return (String::new(), String::new());
    };
    let mut title = first.trim();
    for prefix in ["Title:", "title:", "TITLE:"] {
        if let Some(rest) = title.strip_prefix(prefix) {
            title = rest.trim();
        }
    }
    let title = title
        .trim_start_matches(['#', ' '])
        .trim_matches(['"', '\'', '`', '*'])
        .trim()
        .to_string();
    let rest: Vec<&str> = lines.collect();
    let mut body = rest.join("\n").trim().to_string();
    for label in ["Description:", "Body:", "description:", "body:"] {
        if let Some(stripped) = body.strip_prefix(label) {
            body = stripped.trim_start().to_string();
        }
    }
    (title, body)
}

/// Remove agent attribution: `Co-authored-by:` trailers naming a coding agent
/// (human co-authors stay) and "Generated with …"/"Made with …" credit lines
/// naming one. Everything else is preserved byte-for-byte apart from the
/// blank lines the removal leaves at the end.
pub fn strip_agent_credits(text: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut removed = false;
    for line in text.lines() {
        if is_agent_credit(line) {
            removed = true;
            continue;
        }
        // A removed credit paragraph would leave two blank lines in a row.
        let blank = line.trim().is_empty();
        if blank && removed && kept.last().is_some_and(|prev| prev.trim().is_empty()) {
            continue;
        }
        if !blank {
            removed = false;
        }
        kept.push(line);
    }
    let out = kept.join("\n");
    out.trim_end().trim_start_matches('\n').to_string()
}

fn is_agent_credit(line: &str) -> bool {
    let trimmed = line.trim();
    let lower = trimmed.to_lowercase();
    if let Some(rest) = lower.strip_prefix("co-authored-by:") {
        return mentions_agent(rest);
    }
    // Drop leading emoji/punctuation ("🤖 Generated with [Claude Code](…)").
    let words = lower.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let credit = [
        "generated with",
        "generated by",
        "created with",
        "created by",
        "made with",
        "written with",
        "written by",
    ]
    .iter()
    .any(|lead| words.starts_with(lead));
    credit && mentions_agent(words)
}

fn mentions_agent(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .any(|token| AGENT_TOKENS.contains(&token))
}

/// The repository's pull request template: `pull_request_template.md` (any
/// case, `.md`/`.txt`/bare) in `.github/`, the root, or `docs/`, else the
/// first file in a `PULL_REQUEST_TEMPLATE/` directory there. Returns the
/// repository-relative path and the (capped) contents.
pub fn find_pr_template(root: &Path) -> Option<(String, String)> {
    const NAMES: &[&str] = &[
        "pull_request_template.md",
        "pull_request_template.txt",
        "pull_request_template",
    ];
    let dirs = [".github", "", "docs"];
    for dir in dirs {
        let base = if dir.is_empty() {
            root.to_path_buf()
        } else {
            match find_entry(root, dir, true) {
                Some(name) => root.join(name),
                None => continue,
            }
        };
        for name in NAMES {
            if let Some(found) = find_entry(&base, name, false) {
                let path = base.join(&found);
                if let Some(contents) = read_template(&path) {
                    return Some((relative(root, &path), contents));
                }
            }
        }
    }
    for dir in dirs {
        let base = if dir.is_empty() {
            root.to_path_buf()
        } else {
            match find_entry(root, dir, true) {
                Some(name) => root.join(name),
                None => continue,
            }
        };
        let Some(templates) = find_entry(&base, "pull_request_template", true) else {
            continue;
        };
        let templates = base.join(templates);
        let mut files: Vec<_> = std::fs::read_dir(&templates)
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            })
            .collect();
        files.sort();
        if let Some(path) = files.first()
            && let Some(contents) = read_template(path)
        {
            return Some((relative(root, path), contents));
        }
    }
    None
}

/// A directory entry of `dir` whose name equals `name` ignoring ASCII case.
fn find_entry(dir: &Path, name: &str, want_dir: bool) -> Option<std::ffi::OsString> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_name().to_string_lossy().eq_ignore_ascii_case(name)
                && entry
                    .file_type()
                    .is_ok_and(|t| if want_dir { t.is_dir() } else { t.is_file() })
        })
        .map(|entry| entry.file_name())
}

fn read_template(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(TEMPLATE_MAX_BYTES)])
        .into_owned();
    if text.ends_with('\u{FFFD}') {
        text.pop();
    }
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subjects(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn strips_agent_trailers_and_credit_lines_but_keeps_humans() {
        let message = "feat: add sync button\n\nWires pull then push.\n\n🤖 Generated with [Claude Code](https://claude.com/claude-code)\n\nCo-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>\nCo-authored-by: Jane Doe <jane@example.com>\nCo-authored-by: Cursor Agent <cursoragent@cursor.com>\n";
        assert_eq!(
            strip_agent_credits(message),
            "feat: add sync button\n\nWires pull then push.\n\nCo-authored-by: Jane Doe <jane@example.com>"
        );
        let body = "Summary\n\nGenerated with Codex\nGenerated by the build script\nMade with Cursor";
        assert_eq!(
            strip_agent_credits(body),
            "Summary\n\nGenerated by the build script"
        );
        // A human whose name merely contains an agent substring stays.
        assert_eq!(
            strip_agent_credits("fix: x\n\nCo-authored-by: Claudette Smith <c@example.com>"),
            "fix: x\n\nCo-authored-by: Claudette Smith <c@example.com>"
        );
        assert_eq!(
            strip_agent_credits("Fix\n\nCo-Authored-By: Claude <noreply@anthropic.com>\n\n"),
            "Fix"
        );
    }

    #[test]
    fn detects_conventional_history() {
        assert!(uses_conventional_commits(&subjects(&[
            "feat(ui): add commit box",
            "fix: handle detached head",
            "Merge branch 'main'",
        ])));
        assert!(uses_conventional_commits(&subjects(&[
            "chore!: drop node 16",
            "docs: readme",
        ])));
        assert!(!uses_conventional_commits(&subjects(&[
            "Add commit box",
            "Fix detached head",
            "feat: one",
        ])));
        assert!(!uses_conventional_commits(&subjects(&["feature: x", "fixes: y"])));
        assert!(!uses_conventional_commits(&[]));
    }

    #[test]
    fn prompts_carry_style_and_diff() {
        let conventional = subjects(&["feat: a", "fix: b"]);
        let prompt = commit_prompt("diff --git a/x b/x\n+hi\n", &conventional);
        assert!(prompt.contains("Conventional Commits"));
        assert!(prompt.contains("- feat: a"));
        assert!(prompt.contains("+hi"));
        let plain = commit_prompt("diff --git a/x b/x\n+hi\n", &subjects(&["Add x", "Fix y"]));
        assert!(plain.contains("match the capitalization"));
        let pr = pull_request_prompt(&PullRequestPrompt {
            base: "main".into(),
            head: "feature".into(),
            commits: subjects(&["Add x"]),
            diff: "diff --git a/x b/x\n+hi\n".into(),
            style_subjects: vec![],
            template: Some("## Summary\n<!-- what -->".into()),
        });
        assert!(pr.contains("`feature` into `main`"));
        assert!(pr.contains("## Summary"));
        assert!(pr.contains("- Add x"));
    }

    #[test]
    fn large_diffs_are_summarized_per_file_with_lockfiles_listed_only() {
        let mut patch = String::new();
        for n in 0..30 {
            patch.push_str(&format!(
                "diff --git a/src/f{n}.rs b/src/f{n}.rs\n--- a/src/f{n}.rs\n+++ b/src/f{n}.rs\n@@ -1 +1,200 @@\n"
            ));
            for line in 0..200 {
                patch.push_str(&format!("+line {line} of file {n}\n"));
            }
        }
        patch.push_str("diff --git a/Cargo.lock b/Cargo.lock\n+lock\n");
        let summary = summarize_diff(&patch, 6_000);
        assert!(summary.starts_with("(large diff summarized: 31 files"));
        assert!(summary.contains("### src/f0.rs (+200 -0)"));
        assert!(summary.contains("- Cargo.lock (+1 -0)"));
        assert!(!summary.contains("+lock"));
        assert!(summary.len() < 9_000, "summary stays near budget");
        let small = "diff --git a/a b/a\n+x";
        assert_eq!(summarize_diff(small, 6_000), format!("{small}\n"));
    }

    #[test]
    fn cleans_generated_text() {
        assert_eq!(
            clean_commit_message("```\nfeat: add x\n\nBecause.\n```"),
            "feat: add x\n\nBecause."
        );
        assert_eq!(clean_commit_message("\"Fix bug\""), "Fix bug");
        assert_eq!(clean_commit_message("Commit message:\nFix bug"), "Fix bug");
        let (title, body) =
            parse_pull_request("Title: \"Add sync button\"\n\nDescription:\n- pulls\n- pushes");
        assert_eq!(title, "Add sync button");
        assert_eq!(body, "- pulls\n- pushes");
        assert_eq!(parse_pull_request("  \n"), (String::new(), String::new()));
    }

    #[test]
    fn finds_pull_request_templates_in_any_supported_location() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_pr_template(dir.path()).is_none());

        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/pull_request_template.md"), "docs one").unwrap();
        assert_eq!(
            find_pr_template(dir.path()),
            Some(("docs/pull_request_template.md".into(), "docs one".into()))
        );

        std::fs::write(dir.path().join("PULL_REQUEST_TEMPLATE.md"), "root one").unwrap();
        assert_eq!(find_pr_template(dir.path()).unwrap().1, "root one");

        std::fs::create_dir_all(dir.path().join(".github")).unwrap();
        std::fs::write(
            dir.path().join(".github/PULL_REQUEST_TEMPLATE.md"),
            "## Summary\n",
        )
        .unwrap();
        assert_eq!(
            find_pr_template(dir.path()),
            Some((".github/PULL_REQUEST_TEMPLATE.md".into(), "## Summary".into()))
        );

        let multi = tempfile::tempdir().unwrap();
        let templates = multi.path().join(".github/PULL_REQUEST_TEMPLATE");
        std::fs::create_dir_all(&templates).unwrap();
        std::fs::write(templates.join("z_feature.md"), "feature").unwrap();
        std::fs::write(templates.join("a_bugfix.md"), "bugfix").unwrap();
        std::fs::write(templates.join("notes.txt"), "ignored").unwrap();
        assert_eq!(
            find_pr_template(multi.path()),
            Some((
                ".github/PULL_REQUEST_TEMPLATE/a_bugfix.md".into(),
                "bugfix".into()
            ))
        );
    }
}
