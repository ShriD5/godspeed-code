//! Pull request writes from the PR pane (review, threads, merge, state) and
//! the reads the PR watcher polls. Every write is one explicit user action:
//! structured `gh` arguments, bodies on stdin, never retried, and GitHub's
//! own reason shown when it refuses.
use super::*;
use serde_json::{Value, json};
use zeron_proto::pull_request_actions::{
    ChangeRequestAction, ChangeRequestMergeOptions, MergeMethod, ReviewEvent,
};

const GITHUB_ACTION_TIMEOUT: Duration = Duration::from_secs(60);
/// The failing log excerpt a woken chat receives, at most.
pub(crate) const FAILED_LOG_CAP: usize = 8 * 1024;

const MERGE_OPTIONS_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!) { repository(owner:$owner,name:$name) { mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed autoMergeAllowed viewerPermission pullRequest(number:$number) { autoMergeRequest { mergeMethod } } } }";

const THREAD_OWNER_QUERY: &str =
    "query($id:ID!) { node(id:$id) { ... on PullRequestReviewThread { pullRequest { url } } } }";
const REPLY_MUTATION: &str = "mutation($thread:ID!,$body:String!) { addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$thread,body:$body}) { comment { id url } } }";
const RESOLVE_MUTATION: &str =
    "mutation($thread:ID!) { resolveReviewThread(input:{threadId:$thread}) { thread { id isResolved } } }";
const UNRESOLVE_MUTATION: &str =
    "mutation($thread:ID!) { unresolveReviewThread(input:{threadId:$thread}) { thread { id isResolved } } }";

/// Everything the watcher diffs, in one GraphQL read per poll.
const WATCH_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!) { viewer { login } repository(owner:$owner,name:$name) { mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed autoMergeAllowed pullRequest(number:$number) { number title state isDraft mergeable reviewDecision headRefOid baseRefName autoMergeRequest { mergeMethod } commits(last:1) { nodes { commit { oid statusCheckRollup { state contexts(first:100) { nodes { __typename ... on CheckRun { databaseId name status conclusion detailsUrl checkSuite { workflowRun { databaseId } } } ... on StatusContext { context state targetUrl } } } } } } } reviewThreads(last:50) { nodes { id isResolved path line originalLine comments(last:20) { nodes { id author { login } body createdAt } } } } reviews(last:30) { nodes { id author { login } state body submittedAt } } comments(last:30) { nodes { id author { login } body createdAt } } } } }";

/// One `gh` invocation for an action: arguments plus optional stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionCommand {
    pub args: Vec<String>,
    pub stdin: Option<Vec<u8>>,
}

fn graphql_command(query: &str, variables: Value) -> ActionCommand {
    ActionCommand {
        args: vec!["api".into(), "graphql".into(), "--input".into(), "-".into()],
        stdin: Some(
            serde_json::to_vec(&json!({"query": query, "variables": variables}))
                .unwrap_or_default(),
        ),
    }
}

/// The exact `gh` command for `action` on the pull request at `url` (already
/// validated). Pure, so every command is tested without a network.
pub(crate) fn action_command(url: &str, action: &ChangeRequestAction) -> ActionCommand {
    let pr = |verb: &str| -> Vec<String> { vec!["pr".into(), verb.into(), url.into()] };
    match action {
        ChangeRequestAction::Review { event, body } => {
            let mut args = pr("review");
            args.push(
                match event {
                    ReviewEvent::Approve => "--approve",
                    ReviewEvent::RequestChanges => "--request-changes",
                    ReviewEvent::Comment => "--comment",
                }
                .into(),
            );
            let stdin = (!body.trim().is_empty()).then(|| {
                args.extend(["--body-file".into(), "-".into()]);
                body.clone().into_bytes()
            });
            ActionCommand { args, stdin }
        }
        ChangeRequestAction::ReplyToThread { thread_id, body } => graphql_command(
            REPLY_MUTATION,
            json!({"thread": thread_id, "body": body}),
        ),
        ChangeRequestAction::ResolveThread { thread_id } => {
            graphql_command(RESOLVE_MUTATION, json!({"thread": thread_id}))
        }
        ChangeRequestAction::UnresolveThread { thread_id } => {
            graphql_command(UNRESOLVE_MUTATION, json!({"thread": thread_id}))
        }
        ChangeRequestAction::Merge {
            method,
            head_ref_oid,
        } => {
            let mut args = pr("merge");
            args.extend([
                method.gh_flag().into(),
                "--match-head-commit".into(),
                head_ref_oid.clone(),
            ]);
            ActionCommand { args, stdin: None }
        }
        ChangeRequestAction::EnableAutoMerge { method } => {
            let mut args = pr("merge");
            args.extend(["--auto".into(), method.gh_flag().into()]);
            ActionCommand { args, stdin: None }
        }
        ChangeRequestAction::DisableAutoMerge => {
            let mut args = pr("merge");
            args.push("--disable-auto".into());
            ActionCommand { args, stdin: None }
        }
        ChangeRequestAction::MarkReady => ActionCommand {
            args: pr("ready"),
            stdin: None,
        },
        ChangeRequestAction::Close => ActionCommand {
            args: pr("close"),
            stdin: None,
        },
        ChangeRequestAction::Reopen => ActionCommand {
            args: pr("reopen"),
            stdin: None,
        },
    }
}

/// GitHub's refusal in plain words first, then `gh`'s own text. Branch
/// protection answers are the common case and deserve a clear first line.
pub(crate) fn explain_refusal(stderr: &str) -> String {
    let lower = stderr.to_ascii_lowercase();
    let lead = if lower.contains("approving review is required")
        || lower.contains("review required")
        || lower.contains("changes must be approved")
        || lower.contains("review is required")
    {
        Some("Review required by branch protection.")
    } else if lower.contains("required status check") || lower.contains("checks have not passed")
    {
        Some("Required checks haven't passed (branch protection).")
    } else if lower.contains("base branch policy prohibits the merge") {
        Some(
            "Branch protection blocks this merge: a required review or check is missing. Enable auto-merge to merge once it's met.",
        )
    } else if lower.contains("head branch was modified")
        || lower.contains("head commit")
        || lower.contains("expected head sha")
    {
        Some("The branch changed since you loaded it. Refresh and review the new commits first.")
    } else if lower.contains("merge conflict") || lower.contains("not mergeable") {
        Some("GitHub can't merge this pull request (conflicts or blocked).")
    } else if lower.contains("auto-merge") && lower.contains("not allowed")
        || lower.contains("auto merge is not allowed")
        || lower.contains("enablepullrequestautomerge")
    {
        Some("Auto-merge isn't available here: turn it on in the repository settings.")
    } else if lower.contains("merge method") && lower.contains("not allowed")
        || lower.contains("merge commits are not allowed")
        || lower.contains("squash merges are not allowed")
        || lower.contains("rebase merges are not allowed")
    {
        Some("This repository doesn't allow that merge method.")
    } else if lower.contains("can not approve your own")
        || lower.contains("cannot approve your own")
        || lower.contains("can not request changes on your own")
    {
        Some("GitHub doesn't let you approve or request changes on your own pull request.")
    } else if lower.contains("resource not accessible")
        || lower.contains("must have push access")
        || lower.contains("permission")
    {
        Some("Your GitHub account doesn't have permission to do this here.")
    } else {
        None
    };
    let detail = gh_failure_detail(stderr);
    match lead {
        Some(lead) => format!("{lead}\ngh: {detail}"),
        None => format!("gh: {detail}"),
    }
}

/// `(owner, name, number)` of a validated pull request URL.
fn pr_parts(url: &str) -> (String, String, u64) {
    let parts: Vec<_> = url.split('/').collect();
    (
        parts[3].to_owned(),
        parts[4].to_owned(),
        parts[6].parse().unwrap_or_default(),
    )
}

impl GitHubCli {
    /// One explicit write. Never retried: a timeout may still have landed.
    pub async fn perform_action(
        &self,
        url: &str,
        action: &ChangeRequestAction,
    ) -> Result<String, GitHubWriteError> {
        let url = validated_pull_request_url(url)
            .map_err(|_| GitHubWriteError::Failed("not a GitHub pull request URL".into()))?;
        action.validate().map_err(GitHubWriteError::Refused)?;
        if let ChangeRequestAction::ReplyToThread { thread_id, .. }
        | ChangeRequestAction::ResolveThread { thread_id }
        | ChangeRequestAction::UnresolveThread { thread_id } = action
        {
            self.check_thread_owner(&url, thread_id).await?;
        }
        let command = action_command(&url, action);
        let result = self.run_github_write(command).await;
        // Evict after any answer from GitHub: a refusal can still mean the
        // state moved (someone else merged), and the pane refreshes next.
        self.evict_pull_request(&url).await;
        let output = result?;
        if output.stdout_truncated {
            return Ok(action.done_label().into());
        }
        if matches!(
            action,
            ChangeRequestAction::ReplyToThread { .. }
                | ChangeRequestAction::ResolveThread { .. }
                | ChangeRequestAction::UnresolveThread { .. }
        ) && let Ok(value) = serde_json::from_slice::<Value>(&output.stdout)
            && value["errors"].as_array().is_some_and(|e| !e.is_empty())
        {
            let message = value["errors"][0]["message"]
                .as_str()
                .unwrap_or("GitHub refused the request");
            return Err(GitHubWriteError::Refused(explain_refusal(message)));
        }
        Ok(action.done_label().into())
    }

    /// A thread id must belong to this pull request before anything writes
    /// to it: a stale pane must never reply on another PR's thread.
    async fn check_thread_owner(&self, url: &str, thread_id: &str) -> Result<(), GitHubWriteError> {
        let output = self
            .run_github_write(graphql_command(THREAD_OWNER_QUERY, json!({"id": thread_id})))
            .await?;
        let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
            GitHubWriteError::Failed("GitHub's answer about the thread couldn't be read".into())
        })?;
        let owner = value["data"]["node"]["pullRequest"]["url"]
            .as_str()
            .and_then(|owner| validated_pull_request_url(owner).ok());
        if owner.as_deref().map(str::to_ascii_lowercase) == Some(url.to_ascii_lowercase()) {
            Ok(())
        } else {
            Err(GitHubWriteError::Refused(
                "That review thread isn't on this pull request. Refresh and try again.".into(),
            ))
        }
    }

    async fn run_github_write(
        &self,
        command: ActionCommand,
    ) -> Result<ProcessOutput, GitHubWriteError> {
        let output = self
            .runner
            .run(ProcessRequest {
                args: command.args,
                stdin: command.stdin,
                timeout: GITHUB_ACTION_TIMEOUT,
                ..github_request()
            })
            .await
            .map_err(|error| match error {
                ProcessRunError::Spawn(io::ErrorKind::NotFound) => GitHubWriteError::CliUnavailable,
                ProcessRunError::Timeout => GitHubWriteError::Timeout,
                ProcessRunError::Spawn(_) | ProcessRunError::Io => {
                    GitHubWriteError::Failed("could not run the GitHub CLI".into())
                }
            })?;
        if output.success {
            return Ok(output);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(match classify_github_failure(&output.stderr) {
            ChangeRequestError::Authentication => {
                GitHubWriteError::Authentication(gh_failure_detail(&stderr))
            }
            _ => GitHubWriteError::Refused(explain_refusal(&stderr)),
        })
    }

    /// Drop every cached read of this pull request, in flight ones included.
    pub(crate) async fn evict_pull_request(&self, url: &str) {
        let needle = format!(":{url}:");
        let mut cache = self.pr_cache.lock().await;
        cache.entries.retain(|entry| !entry.0.contains(&needle));
        cache.in_flight.retain(|entry, _| !entry.contains(&needle));
    }

    pub async fn merge_options(
        &self,
        url: &str,
    ) -> Result<ChangeRequestMergeOptions, ChangeRequestError> {
        let url = validated_pull_request_url(url)?;
        let (owner, name, number) = pr_parts(&url);
        let value = self
            .graphql_read(
                MERGE_OPTIONS_QUERY,
                json!({"owner": owner, "name": name, "number": number}),
            )
            .await?;
        Ok(parse_merge_options(&value["data"]["repository"]))
    }

    /// The watcher's one read per poll.
    pub(crate) async fn watch_snapshot(&self, url: &str) -> Result<Value, ChangeRequestError> {
        let url = validated_pull_request_url(url)?;
        let (owner, name, number) = pr_parts(&url);
        self.graphql_read(
            WATCH_QUERY,
            json!({"owner": owner, "name": name, "number": number}),
        )
        .await
    }

    /// The tail of a workflow run's failed-step logs, at most `cap` bytes.
    pub(crate) async fn failed_run_log(&self, url: &str, run_id: u64) -> Option<String> {
        let url = validated_pull_request_url(url).ok()?;
        let (owner, name, _) = pr_parts(&url);
        let output = self
            .runner
            .run(ProcessRequest {
                args: failed_log_args(&owner, &name, run_id),
                ..github_request()
            })
            .await
            .ok()?;
        if !output.success {
            return None;
        }
        // The cause sits at the end of a failed step; capped output keeps the head.
        let text = String::from_utf8_lossy(&output.stdout);
        Some(trim_log_tail(&text, FAILED_LOG_CAP))
    }

    async fn graphql_read(&self, query: &str, variables: Value) -> Result<Value, ChangeRequestError> {
        let command = graphql_command(query, variables);
        let output = self
            .runner
            .run(ProcessRequest {
                args: command.args,
                stdin: command.stdin,
                ..github_request()
            })
            .await
            .map_err(classify_run_error)?;
        if !output.success {
            return Err(classify_github_failure(&output.stderr));
        }
        if output.stdout_truncated {
            return Err(ChangeRequestError::Decode);
        }
        let value: Value =
            serde_json::from_slice(&output.stdout).map_err(|_| ChangeRequestError::Decode)?;
        if value["errors"].as_array().is_some_and(|e| !e.is_empty()) {
            return Err(ChangeRequestError::Decode);
        }
        Ok(value)
    }
}

pub(crate) fn failed_log_args(owner: &str, name: &str, run_id: u64) -> Vec<String> {
    vec![
        "run".into(),
        "view".into(),
        run_id.to_string(),
        "--repo".into(),
        format!("{owner}/{name}"),
        "--log-failed".into(),
    ]
}

/// Keep the last `cap` bytes on a character boundary, marking the cut.
pub(crate) fn trim_log_tail(text: &str, cap: usize) -> String {
    let text = text.trim_end();
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut start = text.len() - cap.saturating_sub(4);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    // Start on a fresh line when one is near, so the excerpt reads cleanly.
    if let Some(newline) = text[start..].find('\n').filter(|offset| *offset < 400) {
        start += newline + 1;
    }
    format!("…\n{}", &text[start..])
}

pub(crate) fn parse_merge_options(repository: &Value) -> ChangeRequestMergeOptions {
    let allowed = |field: &str| repository[field].as_bool().unwrap_or(false);
    let allowed_methods = MergeMethod::ALL
        .into_iter()
        .filter(|method| match method {
            MergeMethod::Squash => allowed("squashMergeAllowed"),
            MergeMethod::Merge => allowed("mergeCommitAllowed"),
            MergeMethod::Rebase => allowed("rebaseMergeAllowed"),
        })
        .collect();
    ChangeRequestMergeOptions {
        allowed_methods,
        auto_merge_allowed: allowed("autoMergeAllowed"),
        auto_merge_method: repository["pullRequest"]["autoMergeRequest"]["mergeMethod"]
            .as_str()
            .and_then(MergeMethod::from_graphql),
        viewer_permission: repository["viewerPermission"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const URL: &str = "https://github.com/acme/widgets/pull/12";
    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    #[derive(Default)]
    struct FakeGh {
        requests: Mutex<Vec<ProcessRequest>>,
        responses: Mutex<VecDeque<Result<ProcessOutput, ProcessRunError>>>,
    }

    #[async_trait]
    impl ProcessRunner for FakeGh {
        async fn run(&self, request: ProcessRequest) -> Result<ProcessOutput, ProcessRunError> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected gh call")
        }
    }

    fn fake(responses: Vec<Result<ProcessOutput, ProcessRunError>>) -> (Arc<FakeGh>, GitHubCli) {
        let runner = Arc::new(FakeGh {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
        });
        (runner.clone(), GitHubCli::with_runner(runner))
    }

    fn ok(stdout: &str) -> Result<ProcessOutput, ProcessRunError> {
        Ok(ProcessOutput {
            success: true,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            stdout_truncated: false,
        })
    }

    fn refused(stderr: &str) -> Result<ProcessOutput, ProcessRunError> {
        Ok(ProcessOutput {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
            stdout_truncated: false,
        })
    }

    fn args(command: &ActionCommand) -> Vec<&str> {
        command.args.iter().map(String::as_str).collect()
    }

    fn stdin_json(command: &ActionCommand) -> Value {
        serde_json::from_slice(command.stdin.as_ref().unwrap()).unwrap()
    }

    #[test]
    fn review_commands_pick_the_verdict_and_send_the_body_on_stdin() {
        let approve = action_command(
            URL,
            &ChangeRequestAction::Review {
                event: ReviewEvent::Approve,
                body: String::new(),
            },
        );
        assert_eq!(args(&approve), ["pr", "review", URL, "--approve"]);
        assert!(approve.stdin.is_none());
        let changes = action_command(
            URL,
            &ChangeRequestAction::Review {
                event: ReviewEvent::RequestChanges,
                body: "$(rm -rf /) please fix".into(),
            },
        );
        assert_eq!(
            args(&changes),
            ["pr", "review", URL, "--request-changes", "--body-file", "-"]
        );
        assert_eq!(
            changes.stdin.as_deref(),
            Some("$(rm -rf /) please fix".as_bytes())
        );
        let comment = action_command(
            URL,
            &ChangeRequestAction::Review {
                event: ReviewEvent::Comment,
                body: "nit".into(),
            },
        );
        assert_eq!(
            args(&comment),
            ["pr", "review", URL, "--comment", "--body-file", "-"]
        );
    }

    #[test]
    fn merge_and_state_commands_are_structured_arguments() {
        let merge = action_command(
            URL,
            &ChangeRequestAction::Merge {
                method: MergeMethod::Squash,
                head_ref_oid: HEAD.into(),
            },
        );
        assert_eq!(
            args(&merge),
            ["pr", "merge", URL, "--squash", "--match-head-commit", HEAD]
        );
        assert!(
            !merge.args.iter().any(|arg| arg == "--admin" || arg == "--delete-branch"),
            "never bypass protection or delete branches"
        );
        let rebase = action_command(
            URL,
            &ChangeRequestAction::Merge {
                method: MergeMethod::Rebase,
                head_ref_oid: HEAD.into(),
            },
        );
        assert_eq!(rebase.args[3], "--rebase");
        assert_eq!(
            args(&action_command(
                URL,
                &ChangeRequestAction::EnableAutoMerge {
                    method: MergeMethod::Merge
                }
            )),
            ["pr", "merge", URL, "--auto", "--merge"]
        );
        assert_eq!(
            args(&action_command(URL, &ChangeRequestAction::DisableAutoMerge)),
            ["pr", "merge", URL, "--disable-auto"]
        );
        assert_eq!(
            args(&action_command(URL, &ChangeRequestAction::MarkReady)),
            ["pr", "ready", URL]
        );
        assert_eq!(
            args(&action_command(URL, &ChangeRequestAction::Close)),
            ["pr", "close", URL]
        );
        assert_eq!(
            args(&action_command(URL, &ChangeRequestAction::Reopen)),
            ["pr", "reopen", URL]
        );
    }

    #[test]
    fn thread_mutations_send_graphql_payloads_on_stdin() {
        let resolve = action_command(
            URL,
            &ChangeRequestAction::ResolveThread {
                thread_id: "PRRT_kwDOA1".into(),
            },
        );
        assert_eq!(args(&resolve), ["api", "graphql", "--input", "-"]);
        let payload = stdin_json(&resolve);
        assert!(
            payload["query"]
                .as_str()
                .unwrap()
                .contains("resolveReviewThread(input:{threadId:$thread})")
        );
        assert_eq!(payload["variables"], json!({"thread": "PRRT_kwDOA1"}));
        let unresolve = stdin_json(&action_command(
            URL,
            &ChangeRequestAction::UnresolveThread {
                thread_id: "PRRT_kwDOA1".into(),
            },
        ));
        assert!(
            unresolve["query"]
                .as_str()
                .unwrap()
                .contains("unresolveReviewThread")
        );
        let reply = stdin_json(&action_command(
            URL,
            &ChangeRequestAction::ReplyToThread {
                thread_id: "PRRT_kwDOA1".into(),
                body: "Fixed in abc".into(),
            },
        ));
        assert!(
            reply["query"]
                .as_str()
                .unwrap()
                .contains("addPullRequestReviewThreadReply")
        );
        assert_eq!(
            reply["variables"],
            json!({"thread": "PRRT_kwDOA1", "body": "Fixed in abc"})
        );
    }

    #[tokio::test]
    async fn merge_runs_once_and_explains_branch_protection() {
        let (runner, github) = fake(vec![refused(
            "GraphQL: At least 1 approving review is required by reviewers with write access. (mergePullRequest)",
        )]);
        let error = github
            .perform_action(
                URL,
                &ChangeRequestAction::Merge {
                    method: MergeMethod::Squash,
                    head_ref_oid: HEAD.into(),
                },
            )
            .await
            .unwrap_err();
        let text = error.to_string();
        assert!(
            text.starts_with("Review required by branch protection.\ngh: "),
            "{text}"
        );
        assert!(text.contains("approving review is required"));
        let requests = runner.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "a refused write is never retried");
        assert_eq!(requests[0].program, "gh");
        assert!(requests[0].cwd.is_none());
        assert!(
            requests[0]
                .env
                .contains(&("GH_PROMPT_DISABLED".into(), "1".into()))
        );
    }

    #[tokio::test]
    async fn invalid_input_never_reaches_gh() {
        let (runner, github) = fake(vec![]);
        for action in [
            ChangeRequestAction::Merge {
                method: MergeMethod::Merge,
                head_ref_oid: "stale".into(),
            },
            ChangeRequestAction::Review {
                event: ReviewEvent::RequestChanges,
                body: " ".into(),
            },
            ChangeRequestAction::ResolveThread {
                thread_id: "a b".into(),
            },
        ] {
            assert!(github.perform_action(URL, &action).await.is_err());
        }
        assert!(
            github
                .perform_action(
                    "https://example.com/acme/widgets/pull/12",
                    &ChangeRequestAction::Close
                )
                .await
                .is_err()
        );
        assert!(runner.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn thread_writes_check_the_thread_belongs_to_this_pull_request() {
        let other = json!({"data": {"node": {"pullRequest": {"url": "https://github.com/acme/widgets/pull/99"}}}});
        let (runner, github) = fake(vec![ok(&other.to_string())]);
        let error = github
            .perform_action(
                URL,
                &ChangeRequestAction::ResolveThread {
                    thread_id: "PRRT_x".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("isn't on this pull request"));
        assert_eq!(runner.requests.lock().unwrap().len(), 1, "no mutation ran");

        let mine = json!({"data": {"node": {"pullRequest": {"url": URL}}}});
        let resolved = json!({"data": {"resolveReviewThread": {"thread": {"id": "PRRT_x", "isResolved": true}}}});
        let (runner, github) = fake(vec![ok(&mine.to_string()), ok(&resolved.to_string())]);
        let message = github
            .perform_action(
                URL,
                &ChangeRequestAction::ResolveThread {
                    thread_id: "PRRT_x".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(message, "Thread resolved");
        let requests = runner.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        let payload: Value = serde_json::from_slice(requests[1].stdin.as_ref().unwrap()).unwrap();
        assert_eq!(payload["variables"]["thread"], "PRRT_x");
    }

    #[test]
    fn refusals_lead_with_plain_words() {
        for (stderr, lead) in [
            (
                "X Pull request acme/widgets#12 is not mergeable: the base branch policy prohibits the merge.",
                "Branch protection blocks this merge",
            ),
            (
                "Required status check \"build\" is expected.",
                "Required checks haven't passed",
            ),
            (
                "GraphQL: Head branch was modified. Review and try the merge again.",
                "The branch changed",
            ),
            (
                "GraphQL: Pull request Auto merge is not allowed for this repository (enablePullRequestAutoMerge)",
                "Auto-merge isn't available",
            ),
            (
                "failed to create review: GraphQL: Can not approve your own pull request",
                "GitHub doesn't let you approve",
            ),
        ] {
            let text = explain_refusal(stderr);
            assert!(text.starts_with(lead), "{stderr} -> {text}");
            assert!(text.contains(stderr));
        }
        assert_eq!(explain_refusal("boom"), "gh: boom");
    }

    #[test]
    fn merge_options_follow_repository_settings() {
        let options = parse_merge_options(&json!({
            "mergeCommitAllowed": false, "squashMergeAllowed": true, "rebaseMergeAllowed": true,
            "autoMergeAllowed": true, "viewerPermission": "WRITE",
            "pullRequest": {"autoMergeRequest": {"mergeMethod": "SQUASH"}}
        }));
        assert_eq!(
            options.allowed_methods,
            [MergeMethod::Squash, MergeMethod::Rebase]
        );
        assert!(options.auto_merge_allowed);
        assert_eq!(options.auto_merge_method, Some(MergeMethod::Squash));
        let none = parse_merge_options(&json!({"pullRequest": {"autoMergeRequest": null}}));
        assert!(none.allowed_methods.is_empty() && none.auto_merge_method.is_none());
    }

    #[test]
    fn failed_logs_keep_the_tail_within_the_cap() {
        assert_eq!(
            failed_log_args("acme", "widgets", 42),
            ["run", "view", "42", "--repo", "acme/widgets", "--log-failed"]
        );
        let log: String = (0..2000).map(|i| format!("line {i} ✓\n")).collect();
        let trimmed = trim_log_tail(&log, FAILED_LOG_CAP);
        assert!(trimmed.len() <= FAILED_LOG_CAP);
        assert!(trimmed.starts_with("…\nline "));
        assert!(trimmed.ends_with("line 1999 ✓"));
        assert_eq!(trim_log_tail("short\n", FAILED_LOG_CAP), "short");
    }
}
