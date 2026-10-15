//! MSP view events → normalized [`AgentEvent`]s. Items fold by id the way the
//! SDK's item store does (a re-emission replaces only at a higher revision,
//! `item/delta` appends without bumping it), so each chip is announced once and
//! settled once whatever mix of started/updated/completed frames carries it.
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use zeron_proto::{
    AgentEvent, DoneStatus, Model, ReasoningLevel, TodoItem, TodoStatus, ToolCall, ToolDiff,
    UserInputAnswer, UserInputQuestion,
};

/// Tools whose work already renders elsewhere: the todo list arrives as
/// `session/todoListChanged`, a question as `userInput/requested`.
const HIDDEN_TOOLS: [&str; 2] = ["write_todos", "request_user_input"];

#[derive(Default)]
struct Tracked {
    kind: String,
    revision: u64,
    /// Text already emitted per streamed field (`text`, `summary.N`, `output`).
    streamed: HashMap<String, String>,
    announced: bool,
    settled: bool,
}

#[derive(Default)]
pub(super) struct Normalizer {
    items: HashMap<String, Tracked>,
    /// Subagents, workflows and backgrounded tasks still running: work a
    /// finished turn leaves behind.
    background: HashSet<String>,
    /// An agent message already wrote text this turn, so the next one opens
    /// a paragraph instead of running on from the last sentence.
    wrote_text: bool,
    pub error: Option<String>,
    pub aborted: bool,
}

impl Normalizer {
    pub fn reset(&mut self) {
        self.error = None;
        self.aborted = false;
        self.wrote_text = false;
        // Settled items are never re-emitted; open ones keep folding into
        // whichever turn is current when they settle.
        self.items.retain(|_, item| !item.settled);
    }

    /// A steer boundary: the engine opens a new assistant entry here.
    pub fn segment(&mut self) {
        self.wrote_text = false;
    }

    pub fn status(&self) -> DoneStatus {
        if self.aborted {
            DoneStatus::Interrupted
        } else if self.error.is_some() {
            DoneStatus::Errored
        } else {
            DoneStatus::Completed
        }
    }

    pub fn background(&self) -> usize {
        self.background.len()
    }

    /// The last `turn/completed` decides how the engine's turn ends: a
    /// retried or follow-up turn that succeeds clears an earlier failure.
    pub fn turn_completed(&mut self, params: &Value) {
        let terminal = string(params, "terminal");
        self.aborted = terminal == "cancelled";
        self.error = (terminal == "failed").then(|| turn_error(params));
    }

    pub fn map(&mut self, method: &str, params: &Value) -> Vec<AgentEvent> {
        match method {
            "item/started" | "item/updated" | "item/completed" => self.item(&params["item"]),
            "item/delta" => self.delta(params),
            "session/tokenUsage" => vec![AgentEvent::Usage {
                input_tokens: params["promptTokens"].as_u64().unwrap_or(0),
                output_tokens: params["usage"]["outputTokens"].as_u64().unwrap_or(0),
            }],
            "session/contextUsage" => vec![AgentEvent::ContextUsage {
                tokens: params["usedTokens"].as_u64(),
                window: params["windowTokens"].as_u64(),
            }],
            "session/todoListChanged" => todo_events(params),
            _ => Vec::new(),
        }
    }

    fn item(&mut self, item: &Value) -> Vec<AgentEvent> {
        let id = string(item, "itemId");
        if id.is_empty() {
            return Vec::new();
        }
        let revision = item["revision"].as_u64().unwrap_or(0);
        let tracked = self.items.entry(id.to_owned()).or_default();
        if tracked.settled || (tracked.revision > 0 && revision < tracked.revision) {
            return Vec::new();
        }
        tracked.revision = revision;
        tracked.kind = string(item, "kind").to_owned();
        let terminal = item["status"].as_str().is_some_and(|s| s != "inProgress");
        let mut events = Vec::new();
        match tracked.kind.as_str() {
            "agentMessage" => {
                let text = string(item, "text");
                if let Some(suffix) = unstreamed(tracked, "text", text) {
                    let first = !tracked.streamed.contains_key("text");
                    events.push(AgentEvent::TextDelta {
                        text: paragraph(&mut self.wrote_text, first, suffix),
                    });
                    tracked.streamed.insert("text".into(), text.into());
                }
            }
            "reasoning" => {
                let parts = item["summary"].as_array().cloned().unwrap_or_default();
                for (index, part) in parts.iter().enumerate() {
                    let field = format!("summary.{index}");
                    let part = part.as_str().unwrap_or("");
                    if let Some(suffix) = unstreamed(tracked, &field, part) {
                        let lead = if index > 0 && !tracked.streamed.contains_key(&field) {
                            "\n\n"
                        } else {
                            ""
                        };
                        events.push(AgentEvent::ReasoningDelta {
                            text: format!("{lead}{suffix}"),
                        });
                        tracked.streamed.insert(field, part.into());
                    }
                }
                // Raw reasoning is never streamed; show it when no summary did.
                let raw = string(item, "text");
                if terminal && tracked.streamed.is_empty() && !raw.is_empty() {
                    events.push(AgentEvent::ReasoningDelta { text: raw.into() });
                }
            }
            "toolCall" | "userShell" | "webSearch" | "workflow" => {
                let hidden = HIDDEN_TOOLS.contains(&string(item, "tool"));
                if !tracked.announced && !hidden {
                    tracked.announced = true;
                    events.push(AgentEvent::ToolCall {
                        id: id.into(),
                        call: tool_call(item),
                    });
                }
                if terminal && tracked.announced {
                    let output = tracked.streamed.get("output").cloned().unwrap_or_default();
                    events.push(tool_result(id, item, output));
                }
            }
            "subagent" => {
                if !tracked.announced {
                    tracked.announced = true;
                    events.extend(subagent_spawn(id, item));
                }
                if terminal {
                    events.extend(subagent_settled(id, item));
                }
            }
            "compaction" if terminal => {
                if let Some(tokens) = item["tokensAfter"].as_u64() {
                    events.push(AgentEvent::ContextUsage {
                        tokens: Some(tokens),
                        window: None,
                    });
                }
            }
            // userMessage echoes prompts the engine already wrote; reminder
            // children, hook runs and side chats are Muse's own bookkeeping.
            _ => {}
        }
        let background = matches!(tracked.kind.as_str(), "subagent" | "workflow")
            || (tracked.kind == "toolCall" && item["background"] == true);
        if terminal {
            tracked.settled = true;
            self.background.remove(id);
        } else if background {
            self.background.insert(id.to_owned());
        }
        events
    }

    fn delta(&mut self, params: &Value) -> Vec<AgentEvent> {
        let Some(tracked) = self.items.get_mut(string(params, "itemId")) else {
            return Vec::new();
        };
        let field = params["field"].as_str().unwrap_or("text");
        let delta = string(params, "delta");
        if tracked.settled || delta.is_empty() {
            return Vec::new();
        }
        match (tracked.kind.as_str(), field) {
            ("agentMessage", "text") => {
                let first = !tracked.streamed.contains_key(field);
                let text = paragraph(&mut self.wrote_text, first, delta);
                tracked
                    .streamed
                    .entry(field.into())
                    .or_default()
                    .push_str(delta);
                vec![AgentEvent::TextDelta { text }]
            }
            ("reasoning", field) if field.starts_with("summary.") => {
                let lead = if field != "summary.0" && !tracked.streamed.contains_key(field) {
                    "\n\n"
                } else {
                    ""
                };
                tracked
                    .streamed
                    .entry(field.into())
                    .or_default()
                    .push_str(delta);
                vec![AgentEvent::ReasoningDelta {
                    text: format!("{lead}{delta}"),
                }]
            }
            // Tool output rides the result once the call settles.
            (_, "output" | "visibleOutput") => {
                tracked
                    .streamed
                    .entry("output".into())
                    .or_default()
                    .push_str(delta);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

/// What a re-emitted field adds beyond the deltas already shown. A value that
/// no longer extends them (a saturated surface was cut) adds nothing.
fn unstreamed<'a>(tracked: &Tracked, field: &str, value: &'a str) -> Option<&'a str> {
    let previous = tracked.streamed.get(field).map_or("", String::as_str);
    value.strip_prefix(previous).filter(|s| !s.is_empty())
}

/// Prefix a paragraph break to a message's first chunk when an earlier
/// message this turn already wrote text.
fn paragraph(wrote: &mut bool, first: bool, chunk: &str) -> String {
    let text = if first && *wrote {
        format!("\n\n{chunk}")
    } else {
        chunk.into()
    };
    *wrote = true;
    text
}

fn turn_error(params: &Value) -> String {
    let error = &params["error"];
    if error["kind"] == "authRequired" {
        return "Muse Code is not logged in. Run `muse login`, then try again.".into();
    }
    error["message"]
        .as_str()
        .or_else(|| params["reason"].as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Muse turn failed")
        .into()
}

fn todo_events(params: &Value) -> Vec<AgentEvent> {
    let items = params["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| {
            TodoItem::new(
                string(item, "text"),
                TodoStatus::parse(string(item, "status")),
            )
        })
        .collect();
    vec![
        AgentEvent::ToolCall {
            id: zeron_proto::LIVE_PLAN_TOOL_ID.into(),
            call: ToolCall::Todo { items },
        },
        AgentEvent::ToolResult {
            id: zeron_proto::LIVE_PLAN_TOOL_ID.into(),
            is_error: false,
            output: None,
            diff: None,
        },
    ]
}

/// The model-authored arguments, which MSP carries verbatim as a JSON string.
fn args(item: &Value) -> Value {
    serde_json::from_str(string(item, "args")).unwrap_or(Value::Null)
}

fn arg(args: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match &args[*key] {
        Value::String(s) => Some(s.clone()),
        // An argv-shaped command.
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    })
}

const PATH: &[&str] = &["path", "file_path", "filePath", "target_path"];
const OLD: &[&str] = &["old_string", "old_str", "oldText", "oldStr"];
const NEW: &[&str] = &["new_string", "new_str", "newText", "newStr"];

/// Muse's tool names are not part of MSP, so calls are recognized by name
/// family (as T3 Code's presentation does) with tolerant argument spellings.
fn tool_call(item: &Value) -> ToolCall {
    let kind = string(item, "kind");
    let args = args(item);
    match kind {
        "userShell" => {
            return ToolCall::Exec {
                command: string(item, "commandText").into(),
            };
        }
        "webSearch" => {
            return ToolCall::WebSearch {
                query: arg(&args, &["query"])
                    .unwrap_or_else(|| string(item, "fallbackText").into()),
            };
        }
        "workflow" => {
            let name = [string(item, "entryId"), string(item, "scriptId")]
                .into_iter()
                .find(|s| !s.is_empty())
                .map_or_else(|| "Workflow".to_owned(), |s| format!("Workflow: {s}"));
            return ToolCall::Unknown {
                name,
                input: Some(json!({ "workflowRunId": item["workflowRunId"] })),
            };
        }
        _ => {}
    }
    let name = string(item, "tool");
    let lower = name.to_ascii_lowercase();
    let path = || arg(&args, PATH).unwrap_or_default();
    if let Some((server, tool)) = lower
        .strip_prefix("mcp__")
        .and_then(|_| name[5..].split_once("__"))
    {
        return ToolCall::Mcp {
            server: server.into(),
            tool: tool.into(),
            input: Some(args),
        };
    }
    if ["shell", "bash", "exec_command", "unified_exec"]
        .iter()
        .any(|family| lower.contains(family))
    {
        return ToolCall::Exec {
            command: arg(&args, &["command", "cmd", "argv"]).unwrap_or_default(),
        };
    }
    match lower.as_str() {
        "read_file" | "read" | "view" => ToolCall::ReadFile { path: path() },
        "write_file" | "write" | "create_file" => ToolCall::WriteFile {
            path: path(),
            content: arg(&args, &["content", "contents", "text"]),
        },
        "edit_file" | "edit" | "str_replace" | "replace" => ToolCall::EditFile {
            path: path(),
            old_string: arg(&args, OLD),
            new_string: arg(&args, NEW),
        },
        "apply_patch" | "patch" => ToolCall::ApplyPatch {
            path: arg(&args, PATH),
        },
        "grep" | "search" | "rg" => ToolCall::Search {
            pattern: arg(&args, &["pattern", "query", "regex"]).unwrap_or_default(),
            path: arg(&args, PATH),
        },
        "glob" | "find" | "list_files" | "ls" => ToolCall::Glob {
            pattern: arg(&args, &["pattern", "glob", "path"]).unwrap_or_default(),
        },
        "web_search" => ToolCall::WebSearch {
            query: arg(&args, &["query"]).unwrap_or_default(),
        },
        "web_fetch" | "fetch" => ToolCall::WebFetch {
            url: arg(&args, &["url"]).unwrap_or_default(),
            prompt: arg(&args, &["prompt"]),
        },
        _ => ToolCall::Unknown {
            name: if name.is_empty() {
                kind.into()
            } else {
                name.into()
            },
            input: (!args.is_null()).then_some(args),
        },
    }
}

fn tool_result(id: &str, item: &Value, streamed: String) -> AgentEvent {
    let ok = item["status"] == "completed" && item["exitCode"].as_i64().is_none_or(|c| c == 0);
    let mut output = [string(item, "visibleOutput"), &streamed]
        .into_iter()
        .find(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            [string(item, "finalSummary"), string(item, "message")]
                .into_iter()
                .find(|s| !s.is_empty())
                .map(str::to_owned)
        });
    if !ok {
        let reason = string(item, "failureReason");
        let detail = if reason.is_empty() {
            format!("Muse reported {}.", string(item, "status"))
        } else {
            reason.to_owned()
        };
        output = Some(match output {
            Some(text) => format!("{detail}\n{text}"),
            None => detail,
        });
    }
    if string(item, "kind") == "workflow" {
        let children: Vec<String> = item["children"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|child| {
                let label = [string(child, "label"), string(child, "childId")]
                    .into_iter()
                    .find(|s| !s.is_empty())
                    .unwrap_or("child");
                let state = child["terminal"]
                    .as_str()
                    .unwrap_or(string(child, "status"));
                format!("- {label}: {state}")
            })
            .collect();
        if !children.is_empty() {
            let mut text = output.unwrap_or_default();
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&children.join("\n"));
            output = Some(text);
        }
    }
    let diff = ok.then(|| diff(item)).flatten();
    AgentEvent::ToolResult {
        id: id.into(),
        is_error: !ok,
        output: output.map(|s| cap(&s, 16384)),
        diff,
    }
}

fn diff(item: &Value) -> Option<ToolDiff> {
    match tool_call(item) {
        ToolCall::EditFile {
            path,
            old_string,
            new_string: Some(new),
        } if !path.is_empty() => Some(ToolDiff {
            path,
            old_text: old_string.map(|s| cap(&s, 65536)),
            new_text: cap(&new, 65536),
        }),
        ToolCall::WriteFile {
            path,
            content: Some(content),
        } if !path.is_empty() => Some(ToolDiff {
            path,
            old_text: None,
            new_text: cap(&content, 65536),
        }),
        _ => None,
    }
}

/// The parent chip plus the child transcript's opening prompt. Muse cannot
/// stream an inline child's own session over this connection (its
/// `childSessionId` is not addressable), so the child shows its objective
/// and, once settled, its result.
fn subagent_spawn(id: &str, item: &Value) -> Vec<AgentEvent> {
    let objective = string(item, "objective");
    let label = [string(item, "role"), objective.lines().next().unwrap_or("")]
        .into_iter()
        .find(|s| !s.trim().is_empty())
        .map(|s| cap_chars(s.trim(), 80));
    let mut events = vec![AgentEvent::ToolCall {
        id: id.into(),
        call: ToolCall::Unknown {
            name: label.map_or_else(|| "Agent".to_owned(), |label| format!("Agent: {label}")),
            input: Some(json!({ "role": item["role"], "agentPath": item["agentPath"] })),
        },
    }];
    if !objective.is_empty() {
        events.push(tagged(
            id,
            AgentEvent::UserMessage {
                text: objective.into(),
            },
        ));
    }
    events
}

fn subagent_settled(id: &str, item: &Value) -> Vec<AgentEvent> {
    let result = &item["result"];
    let text = [string(result, "text"), string(result, "summary")]
        .into_iter()
        .find(|s| !s.is_empty());
    let failed = !matches!(item["status"].as_str(), Some("completed"));
    let status = match item["status"].as_str() {
        Some("completed") => DoneStatus::Completed,
        Some("cancelled") => DoneStatus::Interrupted,
        _ => DoneStatus::Errored,
    };
    let error = failed.then(|| string(item, "failureReason").to_owned());
    let mut events = Vec::new();
    if let Some(text) = text {
        events.push(tagged(id, AgentEvent::TextDelta { text: text.into() }));
    }
    events.push(tagged(
        id,
        AgentEvent::Done {
            status,
            result: None,
            error: error.clone().filter(|e| !e.is_empty()),
            session_id: item["childSessionId"].as_str().map(str::to_owned),
        },
    ));
    events.push(AgentEvent::ToolResult {
        id: id.into(),
        is_error: failed,
        output: text
            .map(|s| cap(s, 16384))
            .or(error.filter(|e| !e.is_empty())),
        diff: None,
    });
    events
}

fn tagged(spawn: &str, event: AgentEvent) -> AgentEvent {
    AgentEvent::Subagent {
        parent_tool_use_id: spawn.into(),
        event: Box::new(event),
    }
}

/// `model/list` rows → the picker's models. Efforts come from each row's
/// `variants`; a row that reports none gets Muse's own CLI ladder.
pub(super) fn models(list: &Value) -> Vec<Model> {
    let mut seen = HashSet::new();
    list["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| seen.insert(string(row, "modelId").to_owned()))
        .filter(|row| !string(row, "modelId").is_empty())
        .map(|row| {
            let levels = match row["variants"].as_array() {
                Some(variants) => variants
                    .iter()
                    .filter_map(|v| effort_level(v.as_str()?))
                    .collect(),
                None => vec![
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                    ReasoningLevel::XHigh,
                    ReasoningLevel::Max,
                ],
            };
            let label = string(row, "displayLabel").trim();
            Model {
                id: string(row, "modelId").into(),
                label: if label.is_empty() {
                    string(row, "modelId").into()
                } else {
                    label.into()
                },
                description: row["description"].as_str().map(str::to_owned),
                reasoning_levels: levels,
                options: vec![],
            }
        })
        .collect()
}

fn effort_level(tier: &str) -> Option<ReasoningLevel> {
    Some(match tier {
        "minimal" => ReasoningLevel::Minimal,
        "low" => ReasoningLevel::Low,
        "medium" => ReasoningLevel::Medium,
        "high" => ReasoningLevel::High,
        "xhigh" => ReasoningLevel::XHigh,
        "max" => ReasoningLevel::Max,
        "ultra" => ReasoningLevel::Ultra,
        _ => return None,
    })
}

/// The MSP effort tier for a picked level; Claude-only levels have none.
pub(super) fn effort(level: ReasoningLevel) -> Option<&'static str> {
    Some(match level {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max => "max",
        ReasoningLevel::Ultra => "ultra",
        ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => return None,
    })
}

/// An approval as a question: one option per native choice label, so the
/// user sees Muse's own once/session/always scopes. Returns the question and
/// the choice id behind each option.
pub(super) fn approval_question(params: &Value) -> (UserInputQuestion, Vec<(String, String)>) {
    let subject = &params["subject"];
    let tool = [string(subject, "toolName"), string(params, "toolName")]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or("a tool");
    let (header, question) = match string(subject, "kind") {
        "shell" => (
            "Approve command",
            match string(subject, "command") {
                "" => "Muse wants to run a command. Allow it?".to_owned(),
                command => format!("Muse wants to run `{command}`. Allow it?"),
            },
        ),
        "fileAccess" => (
            "Approve file access",
            format!(
                "Muse wants to {} {}. Allow it?",
                match string(subject, "access") {
                    "" => "access",
                    access => access,
                },
                string(subject, "path")
            ),
        ),
        "network" | "unixSocket" => {
            let target = [
                string(subject, "host"),
                string(subject, "target"),
                string(subject, "path"),
            ]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("the network");
            let port = subject["port"]
                .as_u64()
                .map(|p| format!(":{p}"))
                .unwrap_or_default();
            (
                "Approve network access",
                format!("Muse wants to reach {target}{port}. Allow it?"),
            )
        }
        _ => (
            "Approve tool",
            format!("Muse wants to use {tool}. Allow it?"),
        ),
    };
    let mut choices: Vec<(String, String)> = Vec::new();
    for choice in params["availableChoices"].as_array().into_iter().flatten() {
        let label = string(choice, "label");
        if !label.is_empty() && !choices.iter().any(|(l, _)| l == label) {
            choices.push((label.into(), string(choice, "choiceId").into()));
        }
    }
    let question = UserInputQuestion {
        id: uuid::Uuid::new_v4().to_string(),
        header: header.into(),
        question,
        options: choices.iter().map(|(label, _)| label.clone()).collect(),
        multi_select: false,
        prefill: None,
        multiline: false,
        approval: None,
    };
    (question, choices)
}

/// The choice to answer with when nobody picked one: an approval for
/// auto-approve, otherwise a denial — never a silent allow. Muse shell
/// approvals offer no `denied` choice, only `abort`.
pub(super) fn default_choice(params: &Value, approve: bool) -> Option<String> {
    let choices: Vec<&Value> = params["availableChoices"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let pick = |decisions: &[&str]| {
        decisions.iter().find_map(|decision| {
            let matching = choices.iter().filter(|c| c["decision"] == *decision);
            matching
                .clone()
                .find(|c| c["scope"] == "once")
                .or_else(|| matching.into_iter().next())
                .map(|c| string(c, "choiceId").to_owned())
        })
    };
    if approve {
        pick(&["approved", "approvedForSession"])
    } else {
        pick(&["denied", "abort"])
    }
}

/// `userInput/requested` questions, keyed by their wire ids.
pub(super) fn input_questions(params: &Value) -> Vec<(String, UserInputQuestion)> {
    params["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| {
            let header = string(q, "header");
            (
                string(q, "id").to_owned(),
                UserInputQuestion {
                    id: uuid::Uuid::new_v4().to_string(),
                    header: if header.is_empty() {
                        "Muse question".into()
                    } else {
                        header.into()
                    },
                    question: string(q, "question").into(),
                    options: q["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|o| string(o, "label").to_owned())
                        .collect(),
                    multi_select: q["selection"]["mode"] == "multiple",
                    prefill: None,
                    multiline: false,
                    approval: None,
                },
            )
        })
        .collect()
}

/// `userInput/answer` entries: picked options travel as labels, anything
/// typed as free text. `None` when a question went unanswered.
pub(super) fn input_answers(
    questions: &[(String, UserInputQuestion)],
    answers: &[UserInputAnswer],
) -> Option<Value> {
    let mut wire = Vec::new();
    for (wire_id, question) in questions {
        let labels = &answers
            .iter()
            .find(|a| a.question_id == question.id)
            .filter(|a| !a.labels.is_empty())?
            .labels;
        let picked = labels.iter().all(|l| question.options.contains(l));
        wire.push(match (picked, question.multi_select) {
            (true, true) => json!({ "questionId": wire_id, "selectedLabels": labels }),
            (true, false) => json!({ "questionId": wire_id, "selectedLabel": labels[0] }),
            (false, _) => json!({ "questionId": wire_id, "freeText": labels.join("\n") }),
        });
    }
    Some(Value::Array(wire))
}

pub(super) fn string<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

fn cap(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let mut at = limit;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    format!("{}\n… [truncated]", &text[..at])
}

fn cap_chars(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.into(),
    }
}
