//! The demo workspace: devices, projects, chats (pinned, sectioned,
//! projectless, child, archived), live statuses, and PRs in every state.

use chrono::Utc;
use zeron_doc::RegistryDoc;
use zeron_proto::{
    ChangeRequestState, ChangeRequestSummary, Chat, ChatConfig, CheckoutChangeRequestStatus,
    ConversationSourceContext, Device, HarnessId, ReasoningLevel, SandboxLevel, Session,
    SessionStatus, SidebarPinChange, SidebarSectionChange, Space,
};

use crate::client::ms;
use crate::config::DemoFixture;
use crate::rpc::capability;

pub(crate) const MAC: &str = "dev-mac";
pub(crate) const VPS: &str = "dev-vps";
pub(crate) const STUDIO: &str = "dev-studio";

pub(crate) struct DemoChat {
    pub id: &'static str,
    pub space: Option<&'static str>,
    pub device: &'static str,
    pub title: &'static str,
    pub preview: &'static str,
    pub harness: HarnessId,
    pub model: &'static str,
    pub reasoning: ReasoningLevel,
    pub branch: Option<&'static str>,
    pub pr: Option<(u64, ChangeRequestState, &'static str)>,
    pub status: Option<SessionStatus>,
    pub last_ago_ms: i64,
    pub created_ago_ms: i64,
    pub seen: bool,
    pub archived: bool,
    pub parent: Option<&'static str>,
}

const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;
const DAY: i64 = 24 * HOUR;

pub(crate) fn chats() -> Vec<DemoChat> {
    use ChangeRequestState::*;
    use HarnessId::*;
    use ReasoningLevel::*;
    let chat = |id, space, device, title, preview| DemoChat {
        id,
        space,
        device,
        title,
        preview,
        harness: ClaudeCode,
        model: "claude-fable-5",
        reasoning: XHigh,
        branch: None,
        pr: None,
        status: None,
        last_ago_ms: HOUR,
        created_ago_ms: DAY,
        seen: true,
        archived: false,
        parent: None,
    };
    vec![
        DemoChat {
            branch: Some("veil-fade"),
            pr: Some((90, Open, "Stream pull request status on every client")),
            status: Some(SessionStatus::Working),
            last_ago_ms: 40_000,
            created_ago_ms: HOUR,
            ..chat(
                "chat-veil",
                Some("space-godspeed"),
                MAC,
                "Streaming veil on transcript rows",
                "Opening the PR now. Running the checks first:",
            )
        },
        DemoChat {
            branch: Some("catalog-sync"),
            pr: Some((84, Merged, "Synchronize model catalogs")),
            status: Some(SessionStatus::AwaitingInput),
            last_ago_ms: 2 * MIN,
            created_ago_ms: 2 * HOUR,
            seen: false,
            model: "claude-opus-5",
            ..chat(
                "chat-picker",
                Some("space-godspeed"),
                MAC,
                "Model picker catalog sync",
                "Before I wire the RPC, two decisions:",
            )
        },
        DemoChat {
            harness: Codex,
            model: "gpt-5.6-terra",
            reasoning: High,
            branch: Some("fix/tool-colors"),
            pr: Some((77, Closed, "Refine tool group colors")),
            last_ago_ms: 15 * MIN,
            seen: false,
            ..chat(
                "chat-tabs",
                Some("space-godspeed"),
                MAC,
                "Tool group header colors",
                "Done — failed children stay quiet.",
            )
        },
        DemoChat {
            status: Some(SessionStatus::Errored),
            last_ago_ms: 33 * MIN,
            seen: false,
            ..chat(
                "chat-errored",
                Some("space-edge"),
                VPS,
                "Durable Object hibernation flush",
                "Harness exited with status 1: rate limited",
            )
        },
        DemoChat {
            last_ago_ms: DAY,
            created_ago_ms: 2 * DAY,
            ..chat(
                "chat-deploy",
                Some("space-edge"),
                VPS,
                "Wrangler deploy hygiene",
                "Hibernation-safe flush timer",
            )
        },
        DemoChat {
            branch: Some("scroll-pinning"),
            pr: Some((
                102,
                Open,
                "Clamp transcript offset on every geometry change",
            )),
            last_ago_ms: 80 * MIN,
            seen: false,
            harness: Codex,
            model: "gpt-6-astra",
            reasoning: Ultra,
            ..chat(
                "chat-ios-scroll",
                Some("space-mobile"),
                MAC,
                "Transcript scroll pinning",
                "Clamp the invariant on every geometry change",
            )
        },
        DemoChat {
            last_ago_ms: 5 * HOUR,
            ..chat(
                "chat-ios-keyboard",
                Some("space-mobile"),
                MAC,
                "Keyboard avoidance for composer",
                "Tracking keyboardLayoutGuide fixes it",
            )
        },
        DemoChat {
            last_ago_ms: 2 * HOUR,
            model: "claude-sonnet-5",
            reasoning: Medium,
            ..chat(
                "chat-home",
                None,
                MAC,
                "Clean up ~/Downloads",
                "Found 412 items older than 90 days.",
            )
        },
        DemoChat {
            last_ago_ms: 2 * MIN,
            ..chat(
                "chat-visuals",
                None,
                MAC,
                "Agent visuals",
                "A dashboard, a canvas, a scene and a shader.",
            )
        },
        DemoChat {
            last_ago_ms: 10 * MIN,
            ..chat(
                "chat-cjk",
                Some("space-godspeed-vps"),
                VPS,
                "多言語テキストのレイアウト 🌏",
                "日本語の長い段落です。",
            )
        },
        DemoChat {
            last_ago_ms: 5 * DAY,
            created_ago_ms: 6 * DAY,
            ..chat(
                "chat-blog",
                Some("space-blog"),
                STUDIO,
                "Draft the launch post",
                "## Outline",
            )
        },
        DemoChat {
            parent: Some("chat-veil"),
            last_ago_ms: 30 * MIN,
            ..chat(
                "chat-side",
                Some("space-godspeed"),
                MAC,
                "Side chat: veil timing",
                "α = 0.2 over inter-append gaps",
            )
        },
        DemoChat {
            archived: true,
            last_ago_ms: 3 * DAY,
            created_ago_ms: 4 * DAY,
            ..chat(
                "chat-oklch",
                Some("space-godspeed"),
                MAC,
                "OKLCH conversion drift",
                "Gamma encode matches now.",
            )
        },
        DemoChat {
            archived: true,
            harness: Codex,
            model: "gpt-5.5",
            reasoning: High,
            last_ago_ms: 6 * DAY,
            created_ago_ms: 7 * DAY,
            ..chat(
                "chat-presence",
                Some("space-edge"),
                VPS,
                "Presence beat coalescing",
                "Batched to one beat per 25s.",
            )
        },
    ]
}

pub(crate) fn spaces(now: i64) -> Vec<Space> {
    let space =
        |id: &str, device: &str, path: &str, name: Option<&str>, git: bool, ago: i64| Space {
            id: id.into(),
            device_id: device.into(),
            path: path.into(),
            name: name.map(str::to_owned),
            git_detected: git,
            git_checked_at: git.then(|| ms(now)),
            checkout_id: git.then(|| format!("co-{id}")),
            repository_id: None,
            created_at: ms(now - ago),
        };
    let godspeed = |mut space: Space| {
        space.repository_id = Some("github.com/ShriD5/godspeed-code".into());
        space
    };
    vec![
        space(
            "space-blog",
            STUDIO,
            "/Users/dev/Projects/blog",
            None,
            false,
            20 * DAY,
        ),
        godspeed(space(
            "space-godspeed",
            MAC,
            "/Users/dev/godspeed",
            None,
            true,
            9 * DAY,
        )),
        // A second clone of the same repository on the VPS.
        godspeed(space(
            "space-godspeed-vps",
            VPS,
            "/srv/src/godspeed",
            None,
            true,
            6 * DAY,
        )),
        space("space-edge", VPS, "/srv/deploys/edge", None, true, 4 * DAY),
        space(
            "space-mobile",
            MAC,
            "/Users/dev/godspeed-ios",
            Some("Godspeed iOS"),
            true,
            2 * DAY,
        ),
    ]
}

pub(crate) fn devices(self_id: &str, self_name: &str, now: i64) -> Vec<Device> {
    let device =
        |id: &str, name: &str, platform: &str, version: &str, caps: bool, seen: i64| Device {
            id: id.into(),
            name: name.into(),
            platform: platform.into(),
            last_seen_at: Some(ms(seen)),
            created_at: Some(ms(now - 30 * DAY)),
            version: Some(version.into()),
            cursor_sdk_version: None,
            capabilities: if caps {
                capability::ALL_QUEUE
                    .iter()
                    .map(|c| (*c).to_owned())
                    .collect()
            } else {
                Vec::new()
            },
        };
    vec![
        device(
            MAC,
            "MacBook Pro",
            "macos",
            env!("CARGO_PKG_VERSION"),
            true,
            now,
        ),
        device(
            VPS,
            "hetzner-01",
            "linux",
            env!("CARGO_PKG_VERSION"),
            true,
            now,
        ),
        device(
            STUDIO,
            "Mac Studio",
            "macos",
            "0.2.80",
            false,
            now - 3 * DAY,
        ),
        device(
            self_id,
            self_name,
            "ios",
            env!("CARGO_PKG_VERSION"),
            false,
            now,
        ),
    ]
}

/// Devices whose presence the demo keeps fresh.
pub(crate) const ONLINE: &[&str] = &[MAC, VPS];

pub(crate) struct Seeded {
    pub change_requests: Vec<CheckoutChangeRequestStatus>,
}

/// Write the whole dataset into the replica (as local writes, which the
/// local-only demo replica folds straight into its rows).
pub(crate) fn seed(
    doc: &mut RegistryDoc,
    fixture: DemoFixture,
    self_id: &str,
    self_name: &str,
) -> Result<Seeded, zeron_doc::DocError> {
    let now = crate::now_ms();
    let all_devices = devices(self_id, self_name, now);
    let mut change_requests = Vec::new();
    match fixture {
        DemoFixture::IosOnly => {
            if let Some(phone) = all_devices.iter().find(|d| d.id == self_id) {
                doc.upsert_device(phone)?;
            }
            doc.reconcile_sidebar_pins(true)?;
            return Ok(Seeded { change_requests });
        }
        DemoFixture::NoProjects => {
            for device in &all_devices {
                doc.upsert_device(device)?;
            }
            doc.reconcile_sidebar_pins(true)?;
            return Ok(Seeded { change_requests });
        }
        DemoFixture::Standard => {}
    }
    for device in &all_devices {
        doc.upsert_device(device)?;
    }
    let spaces = spaces(now);
    for space in &spaces {
        doc.upsert_space(space)?;
    }
    for demo in chats() {
        let space = demo.space.and_then(|id| spaces.iter().find(|s| s.id == id));
        let last = now - demo.last_ago_ms;
        let source_context = match (demo.branch, space) {
            (Some(branch), Some(space)) => Some(ConversationSourceContext {
                checkout_id: format!("co-{}", demo.id),
                repo_root: space.path.clone(),
                cwd: space.path.clone(),
                branch: branch.into(),
                head_sha: None,
                observed_at: ms(last),
            }),
            _ => None,
        };
        let chat = Chat {
            id: demo.id.into(),
            device_id: demo.device.into(),
            title: Some(demo.title.into()),
            archived: demo.archived,
            cwd: Some(space.map_or_else(|| "~".to_owned(), |s| s.path.clone())),
            branch: demo
                .branch
                .map(str::to_owned)
                .or_else(|| space.filter(|s| s.git_detected).map(|_| "main".to_owned())),
            checkout_id: source_context.as_ref().map(|s| s.checkout_id.clone()),
            source_context: source_context.clone(),
            config: Some(ChatConfig {
                harness: demo.harness,
                model: Some(demo.model.into()),
                reasoning: Some(demo.reasoning),
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
                access_mode: None,
            }),
            last_message_preview: Some(demo.preview.into()),
            last_message_at: Some(ms(last)),
            created_at: ms(now - demo.created_ago_ms.max(demo.last_ago_ms + MIN)),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: demo.space.map(str::to_owned),
            last_seen_at: Some(ms(if demo.seen { last } else { last - MIN })),
            room_gen: Some(2),
            parent_chat_id: demo.parent.map(str::to_owned),
            limit: None,
            lifecycle: Default::default(),
        };
        doc.upsert_chat(&chat)?;
        if let Some(status) = demo.status {
            doc.upsert_session(&Session {
                last_completed_turn: None,
                running_subagents: 0,
                background_tasks: 0,
                chat_id: demo.id.into(),
                device_id: demo.device.into(),
                status,
                started_at: Some(ms(now - 95_000)),
                updated_at: Utc::now(),
                awaiting_approval: false,
            })?;
        }
        if let (Some((number, state, title)), Some(source)) = (demo.pr, &source_context) {
            change_requests.push(CheckoutChangeRequestStatus {
                checkout_id: source.checkout_id.clone(),
                device_id: demo.device.into(),
                cwd: source.repo_root.clone(),
                branch: source.branch.clone(),
                change_request: Some(ChangeRequestSummary {
                    provider: "github".into(),
                    number,
                    title: title.into(),
                    url: format!("https://github.com/godspeed-sh/godspeed/pull/{number}"),
                    state,
                    base_ref: "main".into(),
                    head_ref: source.branch.clone(),
                }),
                updated_at: Utc::now(),
            });
        }
    }
    seed_states(doc, now)?;
    doc.reconcile_sidebar_pins(true)?;
    let mut after: Option<String> = None;
    for id in ["chat-veil", "chat-picker"] {
        doc.change_sidebar_pin(&SidebarPinChange::Pin {
            session_id: id.into(),
            after: after.clone(),
            before: None,
        })?;
        after = Some(id.into());
    }
    for (section, name, members) in [
        ("section-p0", "P0", &["chat-tabs", "chat-errored"][..]),
        (
            "section-mobile",
            "Mobile",
            &["chat-ios-scroll", "chat-ios-keyboard"][..],
        ),
    ] {
        doc.change_sidebar_pin(&SidebarPinChange::Section {
            change: SidebarSectionChange::Create {
                id: section.into(),
                name: name.into(),
            },
        })?;
        for member in members {
            doc.change_sidebar_pin(&SidebarPinChange::Section {
                change: SidebarSectionChange::Assign {
                    session_id: (*member).into(),
                    section_id: Some(section.into()),
                    after: None,
                    before: None,
                },
            })?;
        }
    }
    Ok(Seeded { change_requests })
}

// ── limits, shelves, inbox, usage ────────────────────────────────────────

/// Extra demo chats for the newer states: one stopped on a usage limit, one
/// snoozed and one settled (the shelves). Old enough to sit below the live
/// rows the flows tap.
pub(crate) fn seed_states(doc: &mut RegistryDoc, now: i64) -> Result<(), zeron_doc::DocError> {
    use zeron_proto::{ChatLifecycle, ChatLimit, LifecycleState, LimitRecovery, LimitResetSource};
    let chat = |id: &str, space: &str, device: &str, title: &str, preview: &str, ago: i64| Chat {
        id: id.into(),
        device_id: device.into(),
        title: Some(title.into()),
        archived: false,
        cwd: None,
        branch: Some("main".into()),
        checkout_id: None,
        source_context: None,
        config: Some(ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("claude-fable-5".into()),
            reasoning: Some(ReasoningLevel::High),
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
            access_mode: None,
        }),
        last_message_preview: Some(preview.into()),
        last_message_at: Some(ms(now - ago)),
        created_at: ms(now - ago - HOUR),
        harness_session_id: None,
        harness_session_cwd: None,
        space_id: Some(space.into()),
        last_seen_at: Some(ms(now - ago)),
        room_gen: Some(2),
        parent_chat_id: None,
        limit: None,
        lifecycle: Default::default(),
    };
    doc.upsert_chat(&chat(
        "chat-limited",
        "space-godspeed-vps",
        VPS,
        "Backfill usage index",
        "You've hit your 5-hour limit.",
        // Older than the rows the flows tap (below them in Recent).
        DAY + 2 * HOUR,
    ))?;
    doc.set_chat_limit(
        "chat-limited",
        Some(&ChatLimit {
            limited_at: ms(now - DAY - 2 * HOUR),
            reset_at: Some(ms(now + 47 * MIN)),
            reset_source: Some(LimitResetSource::Provider),
            window: Some("5-hour".into()),
            recovery: LimitRecovery::Ask,
            login: None,
        }),
    )?;
    doc.upsert_chat(&chat(
        "chat-snoozed",
        "space-godspeed",
        MAC,
        "Bump wgpu after the 29.1 release",
        "Waiting on upstream.",
        2 * DAY,
    ))?;
    doc.set_chat_lifecycle(
        "chat-snoozed",
        &ChatLifecycle::new(
            LifecycleState::Snoozed {
                until: ms(now + DAY),
            },
            ms(now - DAY),
        ),
    )?;
    doc.upsert_chat(&chat(
        "chat-settled",
        "space-edge",
        VPS,
        "Rotate the R2 access keys",
        "Rotated and verified.",
        3 * DAY,
    ))?;
    doc.set_chat_lifecycle(
        "chat-settled",
        &ChatLifecycle::new(LifecycleState::Settled, ms(now - 2 * DAY)),
    )?;
    Ok(())
}

/// The demo hosts' "Needs you" queues (STUDIO is offline: none).
pub(crate) fn attention(
    now: i64,
) -> Vec<(&'static str, zeron_proto::attention::AttentionSnapshot)> {
    use zeron_proto::attention::{
        AttentionAction as A, AttentionItem, AttentionKind, AttentionSnapshot,
    };
    use zeron_proto::{ApprovalKind, ApprovalRequest, ApprovalRisk};
    let item =
        |chat: &str, kind: AttentionKind, title: &str, project: &str, ago: i64, summary: &str| {
            AttentionItem {
                id: format!("chat:{chat}"),
                kind,
                chat_id: Some(chat.into()),
                title: title.into(),
                space_id: None,
                project: Some(project.into()),
                harness: Some(HarnessId::ClaudeCode),
                created_at: ms(now - ago),
                risk: None,
                summary: summary.into(),
                actions: vec![A::Open],
                request_id: None,
                approval: None,
                agent_run: None,
                conductor: None,
            }
        };
    let approval = |chat: &str,
                    request: &str,
                    title: &str,
                    project: &str,
                    ago: i64,
                    risk: ApprovalRisk,
                    approval: ApprovalRequest| AttentionItem {
        id: format!("approval:{chat}:{request}"),
        kind: AttentionKind::Approval,
        risk: Some(risk),
        summary: approval.title.clone(),
        actions: vec![A::Approve, A::Decline, A::Open],
        request_id: Some(request.into()),
        approval: Some(approval),
        ..item(chat, AttentionKind::Approval, title, project, ago, "")
    };
    let command = |title: &str, command: &str, cwd: &str| {
        let mut r = ApprovalRequest::new(ApprovalKind::Command, title);
        r.command = Some(command.into());
        r.cwd = Some(cwd.into());
        r
    };
    let mut edit = ApprovalRequest::new(ApprovalKind::FileChange, "Edit TranscriptListView.swift");
    edit.paths = vec!["apps/ios/Godspeed/Transcript/TranscriptListView.swift".into()];
    edit.diff = Some(
        "@@ -212,7 +212,9 @@\n     func clampOffset() {\n-        let maxY = contentHeight - bounds.height\n+        let inset = adjustedContentInset\n+        let maxY = max(-inset.top, contentHeight - bounds.height + inset.bottom)\n         offset.y = min(offset.y, maxY)\n     }"
            .into(),
    );
    edit.reason =
        Some("Clamp against the adjusted inset so the keyboard never strands the last row.".into());
    let mac = vec![
        approval(
            "chat-veil",
            "r1",
            "Streaming veil on transcript rows",
            "godspeed",
            40_000,
            ApprovalRisk::High,
            command(
                "Force-push the branch",
                "git push --force-with-lease origin veil-fade",
                "/Users/dev/godspeed",
            ),
        ),
        AttentionItem {
            actions: vec![A::Open],
            ..item(
                "chat-picker",
                AttentionKind::Question,
                "Model picker catalog sync",
                "godspeed",
                2 * MIN,
                "Before I wire the RPC, two decisions:",
            )
        },
        approval(
            "chat-ios-scroll",
            "r2",
            "Transcript scroll pinning",
            "Godspeed iOS",
            6 * MIN,
            ApprovalRisk::Medium,
            edit,
        ),
        approval(
            "chat-tabs",
            "r3",
            "Tool group header colors",
            "godspeed",
            9 * MIN,
            ApprovalRisk::Low,
            command(
                "Run the client tests",
                "cargo test -p zeron-client --lib",
                "/Users/dev/godspeed",
            ),
        ),
        approval(
            "chat-home",
            "r4",
            "Clean up ~/Downloads",
            "Home",
            12 * MIN,
            ApprovalRisk::Low,
            command(
                "List old downloads",
                "ls -la ~/Downloads | head -40",
                "/Users/dev",
            ),
        ),
    ];
    let vps = vec![
        AttentionItem {
            actions: vec![A::Open, A::Dismiss],
            ..item(
                "chat-errored",
                AttentionKind::Failed,
                "Durable Object hibernation flush",
                "edge",
                33 * MIN,
                "Harness exited with status 1: rate limited",
            )
        },
        item(
            "chat-limited",
            AttentionKind::Limited,
            "Backfill usage index",
            "godspeed",
            DAY + 2 * HOUR,
            "Stopped on the 5-hour limit until it resets",
        ),
    ];
    vec![
        (
            MAC,
            AttentionSnapshot {
                device_id: MAC.into(),
                items: mac,
            },
        ),
        (
            VPS,
            AttentionSnapshot {
                device_id: VPS.into(),
                items: vps,
            },
        ),
    ]
}

/// The demo Mac's agent logins and their plan windows.
pub(crate) fn agent_accounts(now: i64) -> zeron_proto::AgentAccountsSnapshot {
    use zeron_proto::{AgentAccount, AgentAccountsSnapshot, AgentAuthKind, AgentUsageWindow};
    let window = |label: &str, used: f32, reset_in: i64| AgentUsageWindow {
        label: label.into(),
        used_fraction: used,
        resets_at: Some(ms(now + reset_in)),
    };
    let account = |id: &str, harness: HarnessId, email: &str, plan: &str, active: bool, windows| {
        AgentAccount {
            id: id.into(),
            harness,
            email: Some(email.into()),
            plan_label: Some(plan.into()),
            active,
            usage_windows: windows,
            usage_fetched_at: Some(now - 2 * MIN),
            usage_error: None,
            display_name: None,
            organization: None,
            auth_kind: Some(AgentAuthKind::Oauth),
            switchable: true,
            saved_at: Some(now - DAY),
            provider: None,
        }
    };
    AgentAccountsSnapshot {
        accounts: vec![
            account(
                "claude-1",
                HarnessId::ClaudeCode,
                "dev@example.com",
                "Max 20x",
                true,
                vec![
                    window("Session", 0.64, 2 * HOUR + 14 * MIN),
                    window("Week", 0.38, 3 * DAY + 4 * HOUR),
                ],
            ),
            account(
                "claude-2",
                HarnessId::ClaudeCode,
                "side@example.com",
                "Pro",
                false,
                vec![window("Session", 0.97, 41 * MIN), window("Week", 0.88, DAY)],
            ),
            account(
                "codex-1",
                HarnessId::Codex,
                "dev@example.com",
                "Pro",
                true,
                vec![
                    window("5h", 0.22, 3 * HOUR),
                    window("Week", 0.81, DAY + 6 * HOUR),
                ],
            ),
        ],
        warnings: Vec::new(),
    }
}

/// A plausible usage history for `range` (deterministic).
pub(crate) fn usage_history(range: zeron_proto::UsageRange, now: i64) -> zeron_proto::UsageHistory {
    use zeron_proto::{
        UsageAmount, UsageBucket, UsageBucketSize, UsageHistory, UsagePoint, UsageRange,
        UsageSlice, UsageSources, UsageTokens,
    };
    let (count, step, bucket) = match range {
        UsageRange::Today => (24, HOUR, UsageBucketSize::Hour),
        UsageRange::Week => (7, DAY, UsageBucketSize::Day),
        UsageRange::Month => (30, DAY, UsageBucketSize::Day),
        UsageRange::Quarter => (90, DAY, UsageBucketSize::Day),
        UsageRange::All => (20, 7 * DAY, UsageBucketSize::Week),
    };
    let start = now - (count as i64 - 1) * step;
    let amount = |seed: u64, scale: f64| {
        let wave = ((seed * 2_654_435_761) % 1000) as f64 / 1000.0;
        let output = ((40_000.0 + 160_000.0 * wave) * scale) as u64;
        let input = output / 3;
        let cache_read = output * 9;
        let cost = output as f64 * 25e-6 + input as f64 * 5e-6 + cache_read as f64 * 0.5e-6;
        UsageAmount {
            tokens: UsageTokens {
                input,
                output,
                cache_read,
                cache_write: output / 4,
            },
            cost_usd: (cost * 100.0).round() / 100.0,
            unpriced_tokens: 0,
            cache_savings_usd: cache_read as f64 * 4.5e-6,
            turns: 3 + seed % 9,
            calls: 0,
        }
    };
    let mut total = UsageAmount::default();
    let mut claude = UsageAmount::default();
    let mut codex = UsageAmount::default();
    let buckets: Vec<UsageBucket> = (0..count)
        .map(|i| {
            let at = ms(start + i as i64 * step);
            let a = amount(i as u64 + 1, 1.0);
            let b = amount(i as u64 + 101, 0.4);
            total.add(&a);
            total.add(&b);
            claude.add(&a);
            codex.add(&b);
            UsageBucket {
                start: at,
                day: at.date_naive(),
                hour: matches!(bucket, UsageBucketSize::Hour).then(|| (i % 24) as u8),
                points: vec![
                    UsagePoint {
                        provider: "claude-code".into(),
                        model: "claude-fable-5".into(),
                        amount: a,
                    },
                    UsagePoint {
                        provider: "codex".into(),
                        model: "gpt-6-astra".into(),
                        amount: b,
                    },
                ],
            }
        })
        .collect();
    let slice = |key: &str, label: &str, amount: UsageAmount| UsageSlice {
        key: key.into(),
        label: label.into(),
        amount,
    };
    UsageHistory {
        range,
        bucket,
        time_zone: "UTC".into(),
        start: Some(ms(start)),
        end: Some(ms(now)),
        buckets,
        total,
        sessions: 14,
        by_model: vec![
            slice("claude-fable-5", "Claude Fable 5", claude),
            slice("gpt-6-astra", "GPT-6 Astra", codex),
        ],
        by_provider: vec![
            slice("claude-code", "Claude Code", claude),
            slice("codex", "Codex", codex),
        ],
        by_project: Vec::new(),
        top_chats: Vec::new(),
        sources: UsageSources {
            godspeed_turns: 412,
            claude_files: 88,
            codex_files: 31,
            deduplicated: 0,
            indexing: false,
        },
        prices_checked: "2026-10-18".into(),
    }
}
