//! Pull request watch: poll a PR's checks, reviews and mergeable state and
//! wake the chat that owns it.
//!
//! - **Opt-in per PR.** On by default for a PR a Godspeed chat opened (the
//!   `CreatePullRequest` action, or a chat's own worktree branch gaining an
//!   open PR); anything else is turned on from the PR pane. A user's "off"
//!   is remembered and never overridden by automatic registration.
//! - **Events.** Newly failed checks (with the failing log tail, at most
//!   8 KB), new review comments from other people (author, file:line, text)
//!   and a newly conflicted branch each queue one message into the PR's chat
//!   and add a Needs-you item.
//! - **Never loops.** Dedupe by check run id (status contexts by name and
//!   head) and comment id; the viewer's own comments (the agent posts as the
//!   user) never wake anyone; at most [`MAX_WAKES_PER_HOUR`] wakes per PR.
//!   Never approves anything. Optional auto-merge uses GitHub's own
//!   auto-merge, only once checks are green and the PR is approved.
//! - **Polite polling.** One PR at a time every [`BASE_INTERVAL`], doubling
//!   on errors up to 15 minutes, 15 minutes on a rate limit; slower after
//!   half an hour without app activity and paused after two hours.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use zeron_proto::attention::{AttentionAction, AttentionItem, AttentionKind};
use zeron_proto::pull_request_actions::{
    ChangeRequestAction, ChangeRequestMergeOptions, MergeMethod, PullRequestWatch,
    PullRequestWatchStatus, SetPullRequestWatch,
};
use zeron_proto::{CheckoutChangeRequestStatus, ChangeRequestState};

use crate::EngineError;
use crate::device_settings;
use crate::doc_host::DocHost;
use crate::source_control::{ChangeRequestError, GitHubCli};
use crate::workspace_host::WorkspaceHost;

const STORE_FILE: &str = "pr-watch.json";
pub(crate) const BASE_INTERVAL: Duration = Duration::from_secs(90);
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
const RATE_LIMIT_PAUSE: Duration = Duration::from_secs(15 * 60);
const SLOW_AFTER: Duration = Duration::from_secs(30 * 60);
const SLOW_INTERVAL: Duration = Duration::from_secs(5 * 60);
const PAUSE_AFTER: Duration = Duration::from_secs(2 * 60 * 60);
pub(crate) const MAX_WAKES_PER_HOUR: usize = 4;
const MAX_EVENTS_PER_PR: usize = 10;
const SEEN_CAP: usize = 1000;
pub(crate) const MESSAGE_CAP: usize = 16 * 1024;
const COMMENT_CHARS: usize = 1500;
const MAX_COMMENTS: usize = 20;
const LOOP_TICK: Duration = Duration::from_secs(15);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── what one poll sees ───────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct WatchCheck {
    /// Dedupe key: `run:{id}` for check runs, `status:{context}@{head}`.
    pub key: String,
    pub name: String,
    pub conclusion: String,
    pub url: String,
    pub run_id: Option<u64>,
    pub failed: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct WatchComment {
    pub id: String,
    pub author: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub body: String,
    /// "requested changes", "reviewed", or empty for plain comments.
    pub verdict: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PollSnapshot {
    pub number: u64,
    pub title: String,
    pub repository: String,
    pub state: String,
    pub is_draft: bool,
    pub mergeable: String,
    pub review_decision: String,
    pub head_oid: String,
    pub base_ref: String,
    pub auto_merge_on: bool,
    pub merge: ChangeRequestMergeOptions,
    /// The rollup is `SUCCESS` for the current head.
    pub checks_green: bool,
    pub checks: Vec<WatchCheck>,
    pub comments: Vec<WatchComment>,
}

/// Read the watch query's answer. `None` when the shape is not a PR.
pub(crate) fn parse_snapshot(url: &str, value: &Value) -> Option<PollSnapshot> {
    let repo = &value["data"]["repository"];
    let pr = &repo["pullRequest"];
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    let state = pr["state"].as_str()?.to_owned();
    let viewer = value["data"]["viewer"]["login"].as_str().unwrap_or_default();
    let head = text(&pr["headRefOid"]);
    let commit = &pr["commits"]["nodes"][0]["commit"];
    let rollup = &commit["statusCheckRollup"];
    let same_head = commit["oid"].as_str() == Some(head.as_str());
    let mut checks = Vec::new();
    if same_head {
        for node in rollup["contexts"]["nodes"].as_array().into_iter().flatten() {
            let check = if node["__typename"] == "StatusContext" {
                let context = text(&node["context"]);
                let state = text(&node["state"]);
                WatchCheck {
                    key: format!("status:{context}@{head}"),
                    failed: zeron_proto::change_request_assessment::check_failed(&state),
                    name: context,
                    conclusion: state,
                    url: text(&node["targetUrl"]),
                    run_id: None,
                }
            } else {
                let Some(id) = node["databaseId"].as_u64() else {
                    continue;
                };
                let conclusion = text(&node["conclusion"]).to_ascii_uppercase();
                WatchCheck {
                    key: format!("run:{id}"),
                    failed: zeron_proto::change_request_assessment::check_failed(&conclusion),
                    name: text(&node["name"]),
                    conclusion,
                    url: text(&node["detailsUrl"]),
                    run_id: node["checkSuite"]["workflowRun"]["databaseId"].as_u64(),
                }
            };
            checks.push(check);
        }
    }
    let foreign = |node: &Value| {
        let author = node["author"]["login"].as_str().unwrap_or_default();
        !author.is_empty() && !author.eq_ignore_ascii_case(viewer)
    };
    let mut comments = Vec::new();
    for thread in pr["reviewThreads"]["nodes"].as_array().into_iter().flatten() {
        if thread["isResolved"].as_bool() == Some(true) {
            continue;
        }
        let path = thread["path"].as_str().map(str::to_owned);
        let line = thread["line"].as_u64().or(thread["originalLine"].as_u64());
        for node in thread["comments"]["nodes"].as_array().into_iter().flatten() {
            if foreign(node) {
                comments.push(WatchComment {
                    id: text(&node["id"]),
                    author: text(&node["author"]["login"]),
                    path: path.clone(),
                    line,
                    body: text(&node["body"]),
                    verdict: String::new(),
                });
            }
        }
    }
    for node in pr["reviews"]["nodes"].as_array().into_iter().flatten() {
        let body = text(&node["body"]);
        let verdict = match node["state"].as_str() {
            Some("CHANGES_REQUESTED") => "requested changes",
            Some("APPROVED") => "approved",
            _ => "reviewed",
        };
        // An empty approval is not feedback; a bare "changes requested" is.
        if foreign(node) && (!body.trim().is_empty() || verdict == "requested changes") {
            comments.push(WatchComment {
                id: text(&node["id"]),
                author: text(&node["author"]["login"]),
                path: None,
                line: None,
                body,
                verdict: verdict.into(),
            });
        }
    }
    for node in pr["comments"]["nodes"].as_array().into_iter().flatten() {
        if foreign(node) {
            comments.push(WatchComment {
                id: text(&node["id"]),
                author: text(&node["author"]["login"]),
                path: None,
                line: None,
                body: text(&node["body"]),
                verdict: String::new(),
            });
        }
    }
    comments.retain(|comment| !comment.id.is_empty());
    let parts: Vec<_> = url.split('/').collect();
    Some(PollSnapshot {
        number: pr["number"].as_u64().unwrap_or_default(),
        title: text(&pr["title"]),
        repository: parts
            .get(3)
            .zip(parts.get(4))
            .map(|(owner, name)| format!("{owner}/{name}"))
            .unwrap_or_default(),
        state,
        is_draft: pr["isDraft"].as_bool().unwrap_or(false),
        mergeable: text(&pr["mergeable"]),
        review_decision: text(&pr["reviewDecision"]),
        head_oid: head,
        base_ref: text(&pr["baseRefName"]),
        auto_merge_on: pr["autoMergeRequest"].is_object(),
        merge: crate::source_control::parse_merge_options_value(repo),
        checks_green: same_head && rollup["state"] == "SUCCESS",
        checks,
        comments,
    })
}

// ── diffing ──────────────────────────────────────────────────────────────

/// What a watch remembers between polls, so nothing is reported twice.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct WatchMemory {
    /// The first poll records existing comments without waking anyone.
    pub primed: bool,
    pub seen_checks: VecDeque<String>,
    pub seen_comments: VecDeque<String>,
    pub conflicted: bool,
    pub wakes: Vec<DateTime<Utc>>,
    /// The head auto-merge was last requested for (once per head).
    pub auto_merge_head: Option<String>,
}

fn remember(seen: &mut VecDeque<String>, key: &str) -> bool {
    if seen.iter().any(|known| known == key) {
        return false;
    }
    seen.push_back(key.to_owned());
    while seen.len() > SEEN_CAP {
        seen.pop_front();
    }
    true
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WatchEvent {
    ChecksFailed(Vec<WatchCheck>),
    Feedback(Vec<WatchComment>),
    Conflicted,
    Ended { merged: bool },
}

/// The events in `snapshot` this watch has not reported yet.
pub(crate) fn diff(memory: &mut WatchMemory, snapshot: &PollSnapshot) -> Vec<WatchEvent> {
    if snapshot.state != "OPEN" {
        return vec![WatchEvent::Ended {
            merged: snapshot.state == "MERGED",
        }];
    }
    let mut events = Vec::new();
    let failed: Vec<WatchCheck> = snapshot
        .checks
        .iter()
        .filter(|check| check.failed)
        .filter(|check| remember(&mut memory.seen_checks, &check.key))
        .cloned()
        .collect();
    if !failed.is_empty() {
        events.push(WatchEvent::ChecksFailed(failed));
    }
    let fresh: Vec<WatchComment> = snapshot
        .comments
        .iter()
        .filter(|comment| remember(&mut memory.seen_comments, &comment.id))
        .cloned()
        .collect();
    if memory.primed && !fresh.is_empty() {
        events.push(WatchEvent::Feedback(fresh));
    }
    match snapshot.mergeable.as_str() {
        "CONFLICTING" if !memory.conflicted => {
            memory.conflicted = true;
            events.push(WatchEvent::Conflicted);
        }
        "MERGEABLE" => memory.conflicted = false,
        _ => {}
    }
    memory.primed = true;
    events
}

/// Record a chat wake if the hourly cap allows one.
pub(crate) fn allow_wake(memory: &mut WatchMemory, now: DateTime<Utc>) -> bool {
    memory
        .wakes
        .retain(|at| now.signed_duration_since(*at) < TimeDelta::hours(1));
    if memory.wakes.len() >= MAX_WAKES_PER_HOUR {
        return false;
    }
    memory.wakes.push(now);
    true
}

/// Delay before the next poll: the base interval, doubled per consecutive
/// failure up to [`MAX_BACKOFF`]; never faster than the idle interval.
pub(crate) fn next_delay(failures: u32, rate_limited: bool, idle: Idle) -> Duration {
    let mut delay = if rate_limited {
        RATE_LIMIT_PAUSE
    } else {
        BASE_INTERVAL
            .saturating_mul(1u32 << failures.min(10))
            .min(MAX_BACKOFF)
    };
    if idle == Idle::Slow {
        delay = delay.max(SLOW_INTERVAL);
    }
    delay
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Idle {
    Active,
    Slow,
    Paused,
}

pub(crate) fn idle_level(since_activity: Duration) -> Idle {
    if since_activity >= PAUSE_AFTER {
        Idle::Paused
    } else if since_activity >= SLOW_AFTER {
        Idle::Slow
    } else {
        Idle::Active
    }
}

// ── messages ─────────────────────────────────────────────────────────────

fn clip_chars(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(max).collect();
    clipped.push('…');
    clipped
}

fn cap_message(mut message: String) -> String {
    if message.len() <= MESSAGE_CAP {
        return message;
    }
    let mut end = MESSAGE_CAP - 64;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str("\n\n… (trimmed; open the pull request for the rest)");
    message
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

pub(crate) fn format_checks_message(
    snapshot: &PollSnapshot,
    url: &str,
    failed: &[WatchCheck],
    log: Option<&str>,
) -> String {
    let mut message = format!(
        "Godspeed PR watch: checks failed on #{} ({}) at {}.\n{url}\n\nFailing checks:\n",
        snapshot.number,
        snapshot.repository,
        short(&snapshot.head_oid)
    );
    for check in failed {
        message.push_str(&format!(
            "- {} ({})",
            clip_chars(&check.name, 200),
            check.conclusion.to_ascii_lowercase()
        ));
        if !check.url.is_empty() {
            message.push_str(&format!(" {}", check.url));
        }
        message.push('\n');
    }
    if let Some(log) = log.filter(|log| !log.trim().is_empty()) {
        let log = crate::source_control::trim_log_tail(log, crate::source_control::FAILED_LOG_CAP);
        message.push_str(&format!("\nFailing log (tail):\n```text\n{log}\n```\n"));
    }
    message.push_str("\nFind the cause, fix it, push, and say what you changed.");
    cap_message(message)
}

pub(crate) fn format_feedback_message(
    snapshot: &PollSnapshot,
    url: &str,
    comments: &[WatchComment],
) -> String {
    let header = format!(
        "Godspeed PR watch: new review feedback on #{} ({}).\n{url}\n",
        snapshot.number, snapshot.repository
    );
    let footer =
        "\nAddress what you agree with, push, and reply on the threads. Don't approve or merge.";
    // Room for the "…and N more" line, so the footer always survives.
    let budget = MESSAGE_CAP.saturating_sub(header.len() + footer.len() + 64);
    let mut body = String::new();
    let mut shown = 0;
    for comment in comments.iter().take(MAX_COMMENTS) {
        let location = match (&comment.path, comment.line) {
            (Some(path), Some(line)) => format!(" on {path}:{line}"),
            (Some(path), None) => format!(" on {path}"),
            _ => String::new(),
        };
        let verdict = if comment.verdict.is_empty() {
            String::new()
        } else {
            format!(" ({})", comment.verdict)
        };
        let text = clip_chars(&comment.body, COMMENT_CHARS);
        let entry = format!(
            "\n@{}{verdict}{location}:\n{}\n",
            comment.author,
            if text.is_empty() { "(no text)".into() } else { text }
        );
        if body.len() + entry.len() > budget {
            break;
        }
        body.push_str(&entry);
        shown += 1;
    }
    let mut message = header + &body;
    if comments.len() > shown {
        message.push_str(&format!(
            "\n…and {} more on GitHub.\n",
            comments.len() - shown
        ));
    }
    message.push_str(footer);
    cap_message(message)
}

pub(crate) fn format_conflict_message(snapshot: &PollSnapshot, url: &str) -> String {
    cap_message(format!(
        "Godspeed PR watch: #{} ({}) now conflicts with {}.\n{url}\n\nMerge or rebase onto the latest {}, resolve the conflicts, run the tests, and push.",
        snapshot.number,
        snapshot.repository,
        if snapshot.base_ref.is_empty() {
            "its base branch"
        } else {
            &snapshot.base_ref
        },
        if snapshot.base_ref.is_empty() {
            "base"
        } else {
            &snapshot.base_ref
        },
    ))
}

// ── one poll ─────────────────────────────────────────────────────────────

/// GitHub as the watcher sees it. A trait so polls are tested without `gh`.
#[async_trait]
pub(crate) trait WatchSource: Send + Sync {
    async fn snapshot(&self, url: &str) -> Result<PollSnapshot, ChangeRequestError>;
    async fn failed_log(&self, url: &str, run_id: u64) -> Option<String>;
    async fn enable_auto_merge(
        &self,
        url: &str,
        method: MergeMethod,
        head: &str,
    ) -> Result<(), String>;
}

#[async_trait]
impl WatchSource for GitHubCli {
    async fn snapshot(&self, url: &str) -> Result<PollSnapshot, ChangeRequestError> {
        let value = self.watch_snapshot(url).await?;
        parse_snapshot(url, &value).ok_or(ChangeRequestError::Decode)
    }

    async fn failed_log(&self, url: &str, run_id: u64) -> Option<String> {
        self.failed_run_log(url, run_id).await
    }

    async fn enable_auto_merge(
        &self,
        url: &str,
        method: MergeMethod,
        head: &str,
    ) -> Result<(), String> {
        match self
            .perform_action(url, &ChangeRequestAction::EnableAutoMerge { method })
            .await
        {
            Ok(_) => Ok(()),
            // Nothing left to wait for: GitHub won't queue auto-merge on a
            // clean PR. Merge now, pinned to the head that was approved.
            Err(error) if error.to_string().to_ascii_lowercase().contains("clean status") => self
                .perform_action(
                    url,
                    &ChangeRequestAction::Merge {
                        method,
                        head_ref_oid: head.to_owned(),
                    },
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// A Needs-you entry a watch produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WatchInboxEvent {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub dismissed: bool,
}

/// One watched pull request, persisted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct WatchEntry {
    pub url: String,
    pub chat_id: Option<String>,
    pub enabled: bool,
    pub auto_merge: bool,
    pub automatic: bool,
    pub memory: WatchMemory,
    pub events: Vec<WatchInboxEvent>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_event: Option<String>,
    pub failures: u32,
    pub stopped: bool,
    pub next_poll_at: Option<DateTime<Utc>>,
}

/// What a poll asks the service to do.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct PollOutcome {
    /// Messages to queue into the entry's chat, in order.
    pub wakes: Vec<String>,
    pub rate_limited: bool,
}

impl WatchEntry {
    fn push_event(&mut self, now: DateTime<Utc>, title: String, summary: String) {
        let id = format!(
            "{}:{}",
            now.timestamp_millis(),
            self.events.len() + self.memory.seen_checks.len() + self.memory.seen_comments.len()
        );
        self.last_event = Some(summary.clone());
        self.events.push(WatchInboxEvent {
            id,
            created_at: now,
            title,
            summary,
            dismissed: false,
        });
        if self.events.len() > MAX_EVENTS_PER_PR {
            self.events.remove(0);
        }
    }
}

/// Poll one watch: read, diff, format, gate wakes, maybe enable auto-merge.
pub(crate) async fn poll_once(
    entry: &mut WatchEntry,
    source: &dyn WatchSource,
    now: DateTime<Utc>,
) -> PollOutcome {
    let mut outcome = PollOutcome::default();
    let snapshot = match source.snapshot(&entry.url).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            entry.failures = entry.failures.saturating_add(1);
            outcome.rate_limited = matches!(error, ChangeRequestError::RateLimited);
            entry.last_error = Some(error.to_string());
            return outcome;
        }
    };
    entry.failures = 0;
    entry.last_error = None;
    entry.last_checked_at = Some(now);
    let title = format!("#{} {}", snapshot.number, clip_chars(&snapshot.title, 120));
    let url = entry.url.clone();
    for event in diff(&mut entry.memory, &snapshot) {
        let (summary, message) = match &event {
            WatchEvent::Ended { merged } => {
                entry.stopped = true;
                entry.last_event = Some(if *merged { "Merged" } else { "Closed" }.into());
                continue;
            }
            WatchEvent::ChecksFailed(failed) => {
                let log = match failed.iter().find_map(|check| check.run_id) {
                    Some(run_id) => source.failed_log(&url, run_id).await,
                    None => None,
                };
                let names: Vec<_> = failed.iter().map(|check| check.name.as_str()).collect();
                (
                    format!("Checks failed: {}", clip_chars(&names.join(", "), 160)),
                    format_checks_message(&snapshot, &url, failed, log.as_deref()),
                )
            }
            WatchEvent::Feedback(comments) => {
                let mut authors: Vec<_> = comments.iter().map(|c| c.author.as_str()).collect();
                authors.dedup();
                (
                    format!(
                        "{} new review comment{} from @{}",
                        comments.len(),
                        if comments.len() == 1 { "" } else { "s" },
                        authors.join(", @")
                    ),
                    format_feedback_message(&snapshot, &url, comments),
                )
            }
            WatchEvent::Conflicted => (
                "Merge conflict with the base branch".to_owned(),
                format_conflict_message(&snapshot, &url),
            ),
        };
        let summary = if entry.chat_id.is_none() {
            summary
        } else if allow_wake(&mut entry.memory, now) {
            outcome.wakes.push(message);
            format!("{summary} · chat woken")
        } else {
            format!("{summary} · chat not woken (hourly limit)")
        };
        entry.push_event(now, title.clone(), summary);
    }
    if entry.auto_merge
        && !entry.stopped
        && !snapshot.is_draft
        && snapshot.review_decision == "APPROVED"
        && snapshot.checks_green
        && !snapshot.auto_merge_on
        && entry.memory.auto_merge_head.as_deref() != Some(snapshot.head_oid.as_str())
    {
        entry.memory.auto_merge_head = Some(snapshot.head_oid.clone());
        let result = match snapshot.merge.preferred_method() {
            Some(method) if snapshot.merge.auto_merge_allowed => {
                source
                    .enable_auto_merge(&url, method, &snapshot.head_oid)
                    .await
            }
            Some(_) => Err("auto-merge is turned off for this repository".into()),
            None => Err("the repository allows no merge method".into()),
        };
        let summary = match result {
            Ok(()) => "Green and approved: auto-merge enabled".to_owned(),
            Err(error) => format!("Auto-merge not enabled: {}", clip_chars(&error, 300)),
        };
        entry.push_event(now, title, summary);
    }
    outcome
}

// ── the book of watches ──────────────────────────────────────────────────

/// Every watch on this engine (persisted as one JSON file).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct WatchBook {
    pub entries: Vec<WatchEntry>,
}

impl WatchBook {
    fn entry_mut(&mut self, url: &str) -> Option<&mut WatchEntry> {
        self.entries.iter_mut().find(|entry| entry.url == url)
    }

    /// The user's choice. Turning a watch back on resumes from its memory.
    pub fn set(&mut self, request: &SetPullRequestWatch) {
        if self.entry_mut(&request.url).is_none() {
            self.entries.push(WatchEntry {
                url: request.url.clone(),
                ..Default::default()
            });
        }
        let entry = self.entry_mut(&request.url).expect("just inserted");
        entry.enabled = request.enabled;
        entry.automatic = false;
        if let Some(chat_id) = &request.chat_id {
            entry.chat_id = Some(chat_id.clone());
        }
        if let Some(auto_merge) = request.auto_merge {
            entry.auto_merge = auto_merge;
        }
        if request.enabled {
            entry.stopped = false;
            entry.failures = 0;
            entry.next_poll_at = None;
        }
    }

    /// A Godspeed chat opened this PR: watch it unless the user said no.
    /// Returns whether anything changed.
    pub fn register_automatic(&mut self, url: &str, chat_id: &str) -> bool {
        if let Some(entry) = self.entry_mut(url) {
            if entry.chat_id.is_none() {
                entry.chat_id = Some(chat_id.to_owned());
                return true;
            }
            return false;
        }
        self.entries.push(WatchEntry {
            url: url.to_owned(),
            chat_id: Some(chat_id.to_owned()),
            enabled: true,
            automatic: true,
            ..Default::default()
        });
        true
    }

    pub fn due(&self, now: DateTime<Utc>) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| entry.enabled && !entry.stopped)
            .filter(|entry| entry.next_poll_at.is_none_or(|at| at <= now))
            .map(|entry| entry.url.clone())
            .collect()
    }

    pub fn views(&self, idle: Idle) -> Vec<PullRequestWatch> {
        self.entries
            .iter()
            .map(|entry| PullRequestWatch {
                url: entry.url.clone(),
                chat_id: entry.chat_id.clone(),
                enabled: entry.enabled,
                auto_merge: entry.auto_merge,
                automatic: entry.automatic,
                status: if !entry.enabled {
                    PullRequestWatchStatus::Off
                } else if entry.stopped {
                    PullRequestWatchStatus::Stopped
                } else if entry.failures > 0 {
                    PullRequestWatchStatus::BackingOff
                } else if idle == Idle::Paused {
                    PullRequestWatchStatus::Idle
                } else {
                    PullRequestWatchStatus::Watching
                },
                last_checked_at: entry.last_checked_at,
                last_error: entry.last_error.clone(),
                last_event: entry.last_event.clone(),
                wakes_last_hour: entry.memory.wakes.len() as u32,
            })
            .collect()
    }

    /// Undismissed events as Needs-you items, newest last per PR.
    pub fn inbox_items(&self) -> Vec<AttentionItem> {
        let mut items = Vec::new();
        for entry in &self.entries {
            for event in entry.events.iter().filter(|event| !event.dismissed) {
                let mut actions = vec![AttentionAction::Dismiss];
                if entry.chat_id.is_some() {
                    actions.insert(0, AttentionAction::Open);
                }
                items.push(AttentionItem {
                    id: format!("pr:{}#{}", entry.url, event.id),
                    kind: AttentionKind::PullRequest,
                    chat_id: entry.chat_id.clone(),
                    title: event.title.clone(),
                    space_id: None,
                    project: None,
                    harness: None,
                    created_at: event.created_at,
                    risk: None,
                    summary: event.summary.clone(),
                    actions,
                    request_id: None,
                    approval: None,
                    agent_run: None,
                    conductor: None,
                });
            }
        }
        items
    }

    pub fn dismiss(&mut self, item_id: &str) -> bool {
        let Some((url, event_id)) = item_id
            .strip_prefix("pr:")
            .and_then(|rest| rest.rsplit_once('#'))
        else {
            return false;
        };
        let Some(event) = self
            .entry_mut(url)
            .and_then(|entry| entry.events.iter_mut().find(|event| event.id == event_id))
        else {
            return false;
        };
        let changed = !event.dismissed;
        event.dismissed = true;
        changed
    }
}

// ── the service ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct PrWatchService {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    book: Mutex<WatchBook>,
    github: GitHubCli,
    doc_host: DocHost,
    workspace: WorkspaceHost,
    inbox: watch::Sender<Vec<AttentionItem>>,
    last_activity: Mutex<Instant>,
    wake: Notify,
    cancel: CancellationToken,
    tasks: TaskTracker,
}

impl PrWatchService {
    pub fn open(data_dir: &Path, doc_host: DocHost, workspace: WorkspaceHost) -> Self {
        let path = data_dir.join(STORE_FILE);
        let book: WatchBook = device_settings::load(&path);
        let items = book.inbox_items();
        Self {
            inner: Arc::new(Inner {
                path,
                book: Mutex::new(book),
                github: GitHubCli::new(),
                doc_host,
                workspace,
                inbox: watch::channel(items).0,
                last_activity: Mutex::new(Instant::now()),
                wake: Notify::new(),
                cancel: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        }
    }

    /// Start polling. Needs a tokio runtime.
    pub fn start(&self) {
        let inner = self.inner.clone();
        self.inner
            .tasks
            .spawn(async move { inner.run_loop().await });
    }

    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        self.inner.tasks.close();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.inner.tasks.wait()).await;
    }

    /// Someone is using the app: keep polling at full speed.
    pub fn note_activity(&self) {
        let was_paused = idle_level(lock(&self.inner.last_activity).elapsed()) == Idle::Paused;
        *lock(&self.inner.last_activity) = Instant::now();
        if was_paused {
            self.inner.wake.notify_one();
        }
    }

    pub fn list(&self) -> Vec<PullRequestWatch> {
        lock(&self.inner.book).views(self.inner.idle())
    }

    pub fn set(&self, request: &SetPullRequestWatch) -> Result<Vec<PullRequestWatch>, EngineError> {
        crate::source_control::validated_pull_request_url_public(&request.url)
            .ok_or_else(|| EngineError::Other("not a GitHub pull request URL".into()))?;
        lock(&self.inner.book).set(request);
        self.inner.persist();
        self.inner.wake.notify_one();
        Ok(self.list())
    }

    /// A Godspeed chat opened `url`: watch it by default.
    pub fn register_automatic(&self, url: &str, chat_id: &str) {
        if crate::source_control::validated_pull_request_url_public(url).is_none() {
            return;
        }
        if lock(&self.inner.book).register_automatic(url, chat_id) {
            tracing::info!(%url, chat = %chat_id, "pr watch: watching a PR a chat opened");
            self.inner.persist();
            self.inner.wake.notify_one();
        }
    }

    /// A checkout's PR resolved: when a chat on this device works in that
    /// checkout's own worktree on that branch, it opened the PR.
    pub fn observe_checkout(&self, status: &CheckoutChangeRequestStatus) {
        let Some(pr) = status
            .change_request
            .as_ref()
            .filter(|pr| pr.state == ChangeRequestState::Open && pr.provider == "github")
        else {
            return;
        };
        let chats = self.inner.workspace.read_chats().unwrap_or_default();
        let spaces = self.inner.workspace.read_spaces().unwrap_or_default();
        let owner = chats
            .iter()
            .filter(|chat| chat.device_id == status.device_id && !chat.archived)
            .filter(|chat| chat.cwd.as_deref() == Some(status.cwd.as_str()))
            .filter(|chat| chat.branch.as_deref() == Some(status.branch.as_str()))
            // A worktree of its own, not the project's main checkout.
            .filter(|_| !spaces.iter().any(|space| space.path == status.cwd))
            .max_by_key(|chat| chat.created_at);
        if let Some(chat) = owner {
            self.register_automatic(&pr.url, &chat.id);
        }
    }

    pub fn inbox_items(&self) -> Vec<AttentionItem> {
        self.inner.inbox.borrow().clone()
    }

    pub fn watch_inbox(&self) -> watch::Receiver<Vec<AttentionItem>> {
        self.inner.inbox.subscribe()
    }

    pub fn dismiss(&self, item_id: &str) -> bool {
        let changed = lock(&self.inner.book).dismiss(item_id);
        if changed {
            self.inner.persist();
        }
        changed
    }
}

impl Inner {
    fn idle(&self) -> Idle {
        idle_level(lock(&self.last_activity).elapsed())
    }

    fn persist(&self) {
        let book = lock(&self.book).clone();
        if let Err(err) = device_settings::save(&self.path, &book) {
            tracing::warn!(%err, "pr watch: not saved");
        }
        let items = book.inbox_items();
        self.inbox.send_if_modified(|current| {
            let changed = *current != items;
            if changed {
                *current = items;
            }
            changed
        });
    }

    async fn run_loop(self: Arc<Self>) {
        loop {
            let idle = self.idle();
            if idle != Idle::Paused {
                let due = lock(&self.book).due(Utc::now());
                // One PR at a time keeps well inside gh's rate limits.
                for url in due {
                    if self.cancel.is_cancelled() {
                        return;
                    }
                    self.poll(&url, idle).await;
                }
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(LOOP_TICK) => {}
            }
        }
    }

    async fn poll(&self, url: &str, idle: Idle) {
        let Some(mut entry) = lock(&self.book)
            .entries
            .iter()
            .find(|entry| entry.url == url)
            .cloned()
        else {
            return;
        };
        let now = Utc::now();
        let outcome = poll_once(&mut entry, &self.github, now).await;
        let delay = next_delay(entry.failures, outcome.rate_limited, idle);
        entry.next_poll_at = Some(now + TimeDelta::from_std(delay).unwrap_or(TimeDelta::zero()));
        let mut undelivered = Vec::new();
        if let Some(chat_id) = entry.chat_id.clone() {
            let live = self
                .workspace
                .chat(&chat_id)
                .ok()
                .flatten()
                .is_some_and(|chat| !chat.archived);
            for message in &outcome.wakes {
                let queued = live
                    && self
                        .doc_host
                        .queue_message_with_behavior(&chat_id, message, Vec::new(), true)
                        .inspect_err(|err| tracing::warn!(%err, chat = %chat_id, "pr watch: wake not queued"))
                        .is_ok();
                if !queued {
                    undelivered.push(());
                }
            }
        }
        if !undelivered.is_empty()
            && let Some(event) = entry.events.last_mut()
        {
            event.summary = event
                .summary
                .replace(" · chat woken", " · chat unavailable, not woken");
        }
        {
            let mut book = lock(&self.book);
            // The user may have changed the watch while the poll ran: keep
            // their switches, take the poll's findings.
            if let Some(current) = book.entry_mut(url) {
                entry.enabled = current.enabled;
                entry.auto_merge = current.auto_merge;
                entry.chat_id = current.chat_id.clone().or(entry.chat_id);
                for event in &mut entry.events {
                    if current
                        .events
                        .iter()
                        .any(|known| known.id == event.id && known.dismissed)
                    {
                        event.dismissed = true;
                    }
                }
                *current = entry;
            }
        }
        self.persist();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    const URL: &str = "https://github.com/acme/widgets/pull/7";
    const HEAD: &str = "89abcdef0123456789abcdef0123456789abcdef";

    fn now() -> DateTime<Utc> {
        "2026-10-28T10:00:00Z".parse().unwrap()
    }

    fn check(id: u64, name: &str, conclusion: &str) -> WatchCheck {
        WatchCheck {
            key: format!("run:{id}"),
            name: name.into(),
            conclusion: conclusion.into(),
            url: format!("https://github.com/acme/widgets/actions/runs/{id}"),
            run_id: Some(100 + id),
            failed: zeron_proto::change_request_assessment::check_failed(conclusion),
        }
    }

    fn comment(id: &str, author: &str, body: &str) -> WatchComment {
        WatchComment {
            id: id.into(),
            author: author.into(),
            path: Some("src/lib.rs".into()),
            line: Some(42),
            body: body.into(),
            verdict: String::new(),
        }
    }

    fn snapshot() -> PollSnapshot {
        PollSnapshot {
            number: 7,
            title: "Fix login".into(),
            repository: "acme/widgets".into(),
            state: "OPEN".into(),
            mergeable: "MERGEABLE".into(),
            head_oid: HEAD.into(),
            base_ref: "main".into(),
            merge: ChangeRequestMergeOptions {
                allowed_methods: vec![MergeMethod::Squash],
                auto_merge_allowed: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn diff_reports_each_failure_and_comment_once() {
        let mut memory = WatchMemory::default();
        let mut snap = snapshot();
        snap.checks = vec![check(1, "build", "FAILURE"), check(2, "lint", "SUCCESS")];
        snap.comments = vec![comment("c1", "rev", "old comment")];
        let first = diff(&mut memory, &snap);
        assert_eq!(
            first,
            vec![WatchEvent::ChecksFailed(vec![check(1, "build", "FAILURE")])],
            "existing comments prime silently; a failing check is actionable"
        );
        assert!(diff(&mut memory, &snap).is_empty(), "nothing twice");
        snap.comments.push(comment("c2", "rev", "please rename"));
        snap.checks.push(check(3, "build", "FAILURE"));
        let second = diff(&mut memory, &snap);
        assert_eq!(
            second,
            vec![
                WatchEvent::ChecksFailed(vec![check(3, "build", "FAILURE")]),
                WatchEvent::Feedback(vec![comment("c2", "rev", "please rename")]),
            ],
            "a re-run is a new check run id"
        );
    }

    #[test]
    fn conflicts_report_on_the_transition_and_rearm_after_mergeable() {
        let mut memory = WatchMemory::default();
        let mut snap = snapshot();
        snap.mergeable = "CONFLICTING".into();
        assert_eq!(diff(&mut memory, &snap), vec![WatchEvent::Conflicted]);
        assert!(diff(&mut memory, &snap).is_empty());
        snap.mergeable = "UNKNOWN".into();
        assert!(diff(&mut memory, &snap).is_empty(), "unknown keeps the state");
        snap.mergeable = "MERGEABLE".into();
        assert!(diff(&mut memory, &snap).is_empty());
        snap.mergeable = "CONFLICTING".into();
        assert_eq!(diff(&mut memory, &snap), vec![WatchEvent::Conflicted]);
        snap.state = "MERGED".into();
        assert_eq!(
            diff(&mut memory, &snap),
            vec![WatchEvent::Ended { merged: true }]
        );
    }

    #[test]
    fn wakes_are_capped_per_hour() {
        let mut memory = WatchMemory::default();
        for _ in 0..MAX_WAKES_PER_HOUR {
            assert!(allow_wake(&mut memory, now()));
        }
        assert!(!allow_wake(&mut memory, now() + TimeDelta::minutes(59)));
        assert!(allow_wake(&mut memory, now() + TimeDelta::minutes(61)));
    }

    #[test]
    fn backoff_doubles_to_a_ceiling_and_idle_slows_polling() {
        assert_eq!(next_delay(0, false, Idle::Active), BASE_INTERVAL);
        assert_eq!(next_delay(1, false, Idle::Active), BASE_INTERVAL * 2);
        assert_eq!(next_delay(2, false, Idle::Active), BASE_INTERVAL * 4);
        assert_eq!(next_delay(30, false, Idle::Active), MAX_BACKOFF);
        assert_eq!(next_delay(0, true, Idle::Active), RATE_LIMIT_PAUSE);
        assert_eq!(next_delay(0, false, Idle::Slow), SLOW_INTERVAL);
        assert_eq!(idle_level(Duration::from_secs(60)), Idle::Active);
        assert_eq!(idle_level(SLOW_AFTER), Idle::Slow);
        assert_eq!(idle_level(PAUSE_AFTER), Idle::Paused);
    }

    #[test]
    fn messages_carry_names_locations_and_stay_capped() {
        let snap = snapshot();
        let log = "x".repeat(50_000) + "\nerror[E0425]: cannot find value `y`";
        let message = format_checks_message(
            &snap,
            URL,
            &[check(1, "build", "FAILURE")],
            Some(&log),
        );
        assert!(message.contains("checks failed on #7 (acme/widgets) at 89abcde"));
        assert!(message.contains("- build (failure) https://github.com/acme/widgets/actions/runs/1"));
        assert!(message.contains("error[E0425]"), "the log tail survives");
        assert!(message.len() <= crate::source_control::FAILED_LOG_CAP + 1024);

        let many: Vec<_> = (0..30)
            .map(|i| comment(&format!("c{i}"), "rev", &"long ".repeat(1000)))
            .collect();
        let feedback = format_feedback_message(&snap, URL, &many);
        assert!(feedback.contains("@rev on src/lib.rs:42:"));
        assert!(feedback.contains(" more on GitHub."));
        assert!(feedback.len() <= MESSAGE_CAP);
        assert!(feedback.contains("Don't approve or merge."));

        let conflict = format_conflict_message(&snap, URL);
        assert!(conflict.contains("now conflicts with main"));
    }

    struct FakeSource {
        snapshot: StdMutex<Result<PollSnapshot, ChangeRequestError>>,
        logs: StdMutex<Vec<u64>>,
        merges: StdMutex<Vec<(MergeMethod, String)>>,
    }

    impl FakeSource {
        fn new(snapshot: PollSnapshot) -> Self {
            Self {
                snapshot: StdMutex::new(Ok(snapshot)),
                logs: Default::default(),
                merges: Default::default(),
            }
        }
    }

    #[async_trait]
    impl WatchSource for FakeSource {
        async fn snapshot(&self, _: &str) -> Result<PollSnapshot, ChangeRequestError> {
            self.snapshot.lock().unwrap().clone()
        }
        async fn failed_log(&self, _: &str, run_id: u64) -> Option<String> {
            self.logs.lock().unwrap().push(run_id);
            Some("step failed: exit 1".into())
        }
        async fn enable_auto_merge(
            &self,
            _: &str,
            method: MergeMethod,
            head: &str,
        ) -> Result<(), String> {
            self.merges.lock().unwrap().push((method, head.into()));
            Ok(())
        }
    }

    fn watched() -> WatchEntry {
        WatchEntry {
            url: URL.into(),
            chat_id: Some("chat-1".into()),
            enabled: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_failed_check_wakes_the_chat_once_with_its_log() {
        let mut snap = snapshot();
        snap.checks = vec![check(1, "build", "FAILURE")];
        let source = FakeSource::new(snap);
        let mut entry = watched();
        let outcome = poll_once(&mut entry, &source, now()).await;
        assert_eq!(outcome.wakes.len(), 1);
        assert!(outcome.wakes[0].contains("step failed: exit 1"));
        assert_eq!(*source.logs.lock().unwrap(), vec![101]);
        assert_eq!(entry.events.len(), 1);
        assert_eq!(entry.events[0].summary, "Checks failed: build · chat woken");
        let again = poll_once(&mut entry, &source, now()).await;
        assert!(again.wakes.is_empty(), "deduped by check run id");
        assert_eq!(entry.events.len(), 1);
    }

    #[tokio::test]
    async fn the_hourly_cap_holds_wakes_but_keeps_inbox_items() {
        let source = FakeSource::new(snapshot());
        let mut entry = watched();
        poll_once(&mut entry, &source, now()).await;
        let mut woken = 0;
        for i in 0..6u64 {
            let mut snap = snapshot();
            snap.comments = vec![comment(&format!("c{i}"), "rev", "fix")];
            *source.snapshot.lock().unwrap() = Ok(snap);
            woken += poll_once(&mut entry, &source, now()).await.wakes.len();
        }
        assert_eq!(woken, MAX_WAKES_PER_HOUR);
        assert_eq!(entry.events.len(), 6);
        assert!(entry.events[5].summary.ends_with("chat not woken (hourly limit)"));
    }

    #[tokio::test]
    async fn errors_back_off_and_rate_limits_pause() {
        let source = FakeSource::new(snapshot());
        *source.snapshot.lock().unwrap() = Err(ChangeRequestError::RateLimited);
        let mut entry = watched();
        let outcome = poll_once(&mut entry, &source, now()).await;
        assert!(outcome.rate_limited);
        assert_eq!(entry.failures, 1);
        assert!(entry.last_error.is_some());
        *source.snapshot.lock().unwrap() = Ok(snapshot());
        poll_once(&mut entry, &source, now()).await;
        assert_eq!(entry.failures, 0);
        assert!(entry.last_error.is_none());
    }

    #[tokio::test]
    async fn auto_merge_waits_for_green_and_approved_and_never_approves() {
        let mut snap = snapshot();
        snap.checks_green = true;
        snap.review_decision = "REVIEW_REQUIRED".into();
        let source = FakeSource::new(snap.clone());
        let mut entry = watched();
        entry.auto_merge = true;
        poll_once(&mut entry, &source, now()).await;
        assert!(source.merges.lock().unwrap().is_empty(), "not approved yet");
        snap.review_decision = "APPROVED".into();
        *source.snapshot.lock().unwrap() = Ok(snap);
        poll_once(&mut entry, &source, now()).await;
        poll_once(&mut entry, &source, now()).await;
        assert_eq!(
            *source.merges.lock().unwrap(),
            vec![(MergeMethod::Squash, HEAD.to_owned())],
            "once per head"
        );
        assert!(
            entry
                .events
                .last()
                .unwrap()
                .summary
                .contains("auto-merge enabled")
        );
    }

    #[test]
    fn automatic_registration_respects_the_users_off_switch() {
        let mut book = WatchBook::default();
        assert!(book.register_automatic(URL, "chat-1"));
        assert!(book.entries[0].enabled && book.entries[0].automatic);
        book.set(&SetPullRequestWatch {
            url: URL.into(),
            chat_id: None,
            enabled: false,
            auto_merge: None,
        });
        assert!(!book.register_automatic(URL, "chat-2"));
        assert!(!book.entries[0].enabled);
        assert_eq!(book.entries[0].chat_id.as_deref(), Some("chat-1"));
        assert!(book.due(now()).is_empty());
        book.set(&SetPullRequestWatch {
            url: URL.into(),
            chat_id: None,
            enabled: true,
            auto_merge: Some(true),
        });
        assert_eq!(book.due(now()), vec![URL.to_owned()]);
        assert!(book.entries[0].auto_merge);
        assert_eq!(book.views(Idle::Paused)[0].status, PullRequestWatchStatus::Idle);
    }

    #[tokio::test]
    async fn inbox_items_open_the_chat_and_dismiss_once() {
        let mut snap = snapshot();
        snap.mergeable = "CONFLICTING".into();
        let source = FakeSource::new(snap);
        let mut book = WatchBook {
            entries: vec![watched()],
        };
        poll_once(&mut book.entries[0], &source, now()).await;
        let items = book.inbox_items();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.kind, AttentionKind::PullRequest);
        assert_eq!(item.chat_id.as_deref(), Some("chat-1"));
        assert_eq!(item.title, "#7 Fix login");
        assert!(item.summary.starts_with("Merge conflict"));
        assert_eq!(
            item.actions,
            vec![AttentionAction::Open, AttentionAction::Dismiss]
        );
        assert!(!item.batch_approvable(), "never approvable");
        assert!(book.dismiss(&item.id));
        assert!(!book.dismiss(&item.id));
        assert!(book.inbox_items().is_empty());
    }

    #[test]
    fn snapshots_parse_checks_comments_and_skip_the_viewer() {
        let value = json!({"data": {"viewer": {"login": "me"}, "repository": {
            "squashMergeAllowed": true, "mergeCommitAllowed": false, "rebaseMergeAllowed": false, "autoMergeAllowed": true,
            "pullRequest": {
                "number": 7, "title": "Fix login", "state": "OPEN", "isDraft": false,
                "mergeable": "MERGEABLE", "reviewDecision": "APPROVED", "headRefOid": HEAD, "baseRefName": "main",
                "autoMergeRequest": null,
                "commits": {"nodes": [{"commit": {"oid": HEAD, "statusCheckRollup": {"state": "FAILURE", "contexts": {"nodes": [
                    {"__typename": "CheckRun", "databaseId": 11, "name": "build", "status": "COMPLETED", "conclusion": "FAILURE", "detailsUrl": "https://x/11", "checkSuite": {"workflowRun": {"databaseId": 900}}},
                    {"__typename": "StatusContext", "context": "ci/legacy", "state": "ERROR", "targetUrl": "https://x/legacy"}
                ]}}}}]},
                "reviewThreads": {"nodes": [
                    {"id": "T1", "isResolved": false, "path": "src/a.rs", "line": null, "originalLine": 9, "comments": {"nodes": [
                        {"id": "RC1", "author": {"login": "rev"}, "body": "rename this", "createdAt": "x"},
                        {"id": "RC2", "author": {"login": "me"}, "body": "done", "createdAt": "x"}
                    ]}},
                    {"id": "T2", "isResolved": true, "path": "src/b.rs", "line": 1, "comments": {"nodes": [
                        {"id": "RC3", "author": {"login": "rev"}, "body": "old", "createdAt": "x"}
                    ]}}
                ]},
                "reviews": {"nodes": [
                    {"id": "R1", "author": {"login": "rev"}, "state": "CHANGES_REQUESTED", "body": "", "submittedAt": "x"},
                    {"id": "R2", "author": {"login": "rev"}, "state": "APPROVED", "body": "", "submittedAt": "x"}
                ]},
                "comments": {"nodes": [{"id": "IC1", "author": {"login": "bot"}, "body": "coverage -1%", "createdAt": "x"}]}
            }
        }}});
        let snap = parse_snapshot(URL, &value).unwrap();
        assert_eq!(snap.repository, "acme/widgets");
        assert_eq!(snap.checks.len(), 2);
        assert_eq!(snap.checks[0].key, "run:11");
        assert_eq!(snap.checks[0].run_id, Some(900));
        assert!(snap.checks[0].failed && snap.checks[1].failed);
        assert_eq!(snap.checks[1].key, format!("status:ci/legacy@{HEAD}"));
        let ids: Vec<_> = snap.comments.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["RC1", "R1", "IC1"]);
        assert_eq!(snap.comments[0].line, Some(9));
        assert_eq!(snap.comments[1].verdict, "requested changes");
        assert!(!snap.checks_green);
        assert_eq!(snap.merge.allowed_methods, [MergeMethod::Squash]);
    }
}
