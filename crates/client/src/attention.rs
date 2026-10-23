//! The "Needs you" inbox across devices.
//!
//! Every execution host runs its own attention queue (`WatchAttention`,
//! `zeron_proto::attention`). The phone keeps one stream per host over the
//! device-room relay while foregrounded and merges the snapshots into one
//! queue in [`attention_order`] (risk band, then oldest first).
//!
//! A host whose engine predates the inbox, or whose stream is down (offline,
//! reconnecting), contributes items derived from its registry rows instead:
//! the same kinds, open-only (answering an approval needs the host's inbox),
//! with Finished/Failed dismissable by marking the chat seen.
//!
//! Answers hide their item at once (optimistically) until the host's next
//! snapshot drops it, so a swipe never bounces back while the relay
//! round-trips; a failed call brings it back.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use zeron_proto::attention::{
    AttentionAction, AttentionItem, AttentionKind, AttentionSnapshot, BatchApproval,
    attention_order,
};
use zeron_proto::{ApprovalDecision, ApprovalRequest, ChatIndicator};
use zeron_rpc::{RpcError, methods};

use crate::client::ClientInner;
use crate::workspace::{SessionRow, WorkspaceSnapshot};
use crate::{lock, now_ms};

/// How long an answered item stays hidden waiting for the host to drop it.
const HIDE_MS: i64 = 20_000;
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// One row of the merged queue.
#[derive(Debug, Clone, PartialEq)]
pub struct AttentionEntry {
    /// The engine hosting the item (where answers go).
    pub device_id: String,
    pub device_name: Option<String>,
    pub item: AttentionItem,
    /// Derived from the registry (host without a live inbox): open-only,
    /// except Finished/Failed, which dismiss by marking the chat seen.
    pub derived: bool,
}

impl AttentionEntry {
    /// Queue band (`AttentionItem::band`): 0 = high-risk approvals.
    pub fn band(&self) -> u8 {
        self.item.band()
    }

    pub fn can_approve(&self) -> bool {
        !self.derived
            && self.item.kind == AttentionKind::Approval
            && self.item.supports(AttentionAction::Approve)
            && self
                .item
                .approval
                .as_ref()
                .is_some_and(|a| decision_for(a, true).is_some())
    }

    pub fn can_decline(&self) -> bool {
        !self.derived
            && self.item.kind == AttentionKind::Approval
            && self.item.supports(AttentionAction::Decline)
            && self
                .item
                .approval
                .as_ref()
                .is_some_and(|a| decision_for(a, false).is_some())
    }

    pub fn can_dismiss(&self) -> bool {
        if self.derived {
            matches!(
                self.item.kind,
                AttentionKind::Finished | AttentionKind::Failed
            ) && self.item.chat_id.is_some()
        } else {
            self.item.supports(AttentionAction::Dismiss)
        }
    }

    /// Only live low-risk approvals batch (`AttentionItem::batch_approvable`).
    pub fn batch_approvable(&self) -> bool {
        !self.derived && self.item.batch_approvable()
    }
}

/// The merged inbox. `revision` changes whenever any host's queue, a hide,
/// or the registry-derived part may have changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AttentionView {
    pub revision: u64,
    /// In [`attention_order`] across every host.
    pub items: Vec<AttentionEntry>,
    /// Hosts answering with a live inbox right now.
    pub live_devices: Vec<String>,
}

impl AttentionView {
    /// Everything waiting (the desktop sidebar's badge rule).
    pub fn badge_count(&self) -> usize {
        self.items.len()
    }

    pub fn low_risk_count(&self) -> usize {
        self.items.iter().filter(|e| e.batch_approvable()).count()
    }
}

/// `ApproveLowRisk` across hosts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchResult {
    pub approved: u32,
    pub skipped: u32,
    /// Hosts the call could not reach.
    pub failed_devices: Vec<String>,
}

/// The decision a one-tap approve / decline sends: the provider's plain
/// option first (desktop `approval_default_index`).
pub fn decision_for(approval: &ApprovalRequest, approve: bool) -> Option<ApprovalDecision> {
    use ApprovalDecision as D;
    let want: &[D] = if approve {
        &[D::Approve, D::ApproveForSession, D::ApproveAlways]
    } else {
        &[D::Decline, D::Cancel, D::DeclineAlways]
    };
    want.iter()
        .find(|w| approval.decisions.iter().any(|o| o.decision == **w))
        .copied()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Support {
    #[default]
    Unknown,
    Supported,
    /// The engine has no `WatchAttention` (before 0.4): derive.
    Unsupported,
}

#[derive(Debug, Clone, Default)]
struct Feed {
    support: Support,
    /// The stream is delivering (its snapshot is current).
    connected: bool,
    snapshot: Option<AttentionSnapshot>,
}

impl Feed {
    fn live(&self) -> bool {
        self.support == Support::Supported && self.connected && self.snapshot.is_some()
    }
}

#[derive(Default)]
pub(crate) struct AttentionStore {
    feeds: Mutex<HashMap<String, Feed>>,
    watches: Mutex<HashMap<String, CancellationToken>>,
    /// `(device, item)` → hidden until (epoch ms).
    hidden: Mutex<HashMap<(String, String), i64>>,
    revision: AtomicU64,
}

impl AttentionStore {
    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    fn bump(&self) -> u64 {
        self.revision.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Install a host's snapshot (stream item, or the demo host).
    pub(crate) fn put(&self, device_id: &str, snapshot: AttentionSnapshot) -> u64 {
        {
            let mut feeds = lock(&self.feeds);
            let feed = feeds.entry(device_id.to_owned()).or_default();
            feed.support = Support::Supported;
            feed.connected = true;
            // Answered items the host has dropped need no hiding any more.
            let present: std::collections::HashSet<&str> =
                snapshot.items.iter().map(|i| i.id.as_str()).collect();
            lock(&self.hidden)
                .retain(|(device, item), _| device != device_id || present.contains(item.as_str()));
            feed.snapshot = Some(snapshot);
        }
        self.bump()
    }

    fn disconnected(&self, device_id: &str) -> Option<u64> {
        let mut feeds = lock(&self.feeds);
        let feed = feeds.get_mut(device_id)?;
        if !feed.connected {
            return None;
        }
        feed.connected = false;
        drop(feeds);
        Some(self.bump())
    }

    fn unsupported(&self, device_id: &str) -> u64 {
        lock(&self.feeds)
            .entry(device_id.to_owned())
            .or_default()
            .support = Support::Unsupported;
        self.bump()
    }

    /// Forget "unsupported" verdicts (the Mac may have updated).
    pub(crate) fn clear_unsupported(&self) {
        for feed in lock(&self.feeds).values_mut() {
            if feed.support == Support::Unsupported {
                feed.support = Support::Unknown;
            }
        }
    }

    fn hide(&self, device_id: &str, item_id: &str) -> u64 {
        lock(&self.hidden).insert(
            (device_id.to_owned(), item_id.to_owned()),
            now_ms() + HIDE_MS,
        );
        self.bump()
    }

    fn unhide(&self, device_id: &str, item_id: &str) -> u64 {
        lock(&self.hidden).remove(&(device_id.to_owned(), item_id.to_owned()));
        self.bump()
    }

    /// The live item, when its host's inbox is live.
    fn item(&self, device_id: &str, item_id: &str) -> Option<AttentionItem> {
        let feeds = lock(&self.feeds);
        let feed = feeds.get(device_id).filter(|f| f.live())?;
        feed.snapshot
            .as_ref()?
            .items
            .iter()
            .find(|i| i.id == item_id)
            .cloned()
    }

    /// Edit a host's stored snapshot in place (demo host answers).
    pub(crate) fn edit(
        &self,
        device_id: &str,
        f: impl FnOnce(&mut AttentionSnapshot),
    ) -> Option<u64> {
        {
            let mut feeds = lock(&self.feeds);
            let snapshot = feeds.get_mut(device_id)?.snapshot.as_mut()?;
            f(snapshot);
        }
        Some(self.bump())
    }

    /// The merged queue for `ws`.
    pub(crate) fn view(&self, ws: &WorkspaceSnapshot, now_ms: i64) -> AttentionView {
        let feeds = lock(&self.feeds).clone();
        let hidden = {
            let mut hidden = lock(&self.hidden);
            hidden.retain(|_, until| *until > now_ms);
            hidden.clone()
        };
        merge(&feeds, &hidden, ws, now_ms, self.revision())
    }

    fn reconcile(self: &Arc<Self>, inner: &Arc<ClientInner>, targets: Vec<String>) {
        let mut watches = lock(&self.watches);
        let skip: Vec<String> = lock(&self.feeds)
            .iter()
            .filter(|(_, f)| f.support == Support::Unsupported)
            .map(|(id, _)| id.clone())
            .collect();
        watches.retain(|device, token| {
            let keep = targets.contains(device) && !skip.contains(device);
            if !keep {
                token.cancel();
            }
            keep
        });
        for device in targets {
            if watches.contains_key(&device) || skip.contains(&device) {
                continue;
            }
            let token = CancellationToken::new();
            watches.insert(device.clone(), token.clone());
            spawn_watch(Arc::downgrade(inner), device, token);
        }
    }

    pub(crate) fn stop(&self) {
        for (_, token) in lock(&self.watches).drain() {
            token.cancel();
        }
        let mut feeds = lock(&self.feeds);
        for feed in feeds.values_mut() {
            feed.connected = false;
        }
        drop(feeds);
        self.bump();
    }
}

/// Pure merge: live hosts' items plus registry-derived items for every other
/// host, minus hidden ones, in queue order.
fn merge(
    feeds: &HashMap<String, Feed>,
    hidden: &HashMap<(String, String), i64>,
    ws: &WorkspaceSnapshot,
    now_ms: i64,
    revision: u64,
) -> AttentionView {
    let device_name = |id: &str| ws.device(id).map(|d| d.name.clone());
    let mut items: Vec<AttentionEntry> = Vec::new();
    let mut live_devices: Vec<String> = Vec::new();
    for (device_id, feed) in feeds {
        if !feed.live() {
            continue;
        }
        live_devices.push(device_id.clone());
        let Some(snapshot) = &feed.snapshot else {
            continue;
        };
        for item in &snapshot.items {
            if hidden.contains_key(&(device_id.clone(), item.id.clone())) {
                continue;
            }
            items.push(AttentionEntry {
                device_id: device_id.clone(),
                device_name: device_name(device_id),
                item: item.clone(),
                derived: false,
            });
        }
    }
    live_devices.sort();
    let mut rows: Vec<&Arc<SessionRow>> = ws
        .sessions
        .values()
        .filter(|r| !r.archived && r.parent_chat_id.is_none())
        .filter(|r| !live_devices.contains(&r.device_id))
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    for row in rows {
        let Some(item) = derived_item(row, now_ms) else {
            continue;
        };
        if hidden.contains_key(&(row.device_id.clone(), item.id.clone())) {
            continue;
        }
        items.push(AttentionEntry {
            device_id: row.device_id.clone(),
            device_name: row
                .device_name
                .clone()
                .or_else(|| device_name(&row.device_id)),
            item,
            derived: true,
        });
    }
    items.sort_by(|a, b| {
        attention_order(&a.item, &b.item).then_with(|| a.device_id.cmp(&b.device_id))
    });
    AttentionView {
        revision,
        items,
        live_devices,
    }
}

fn utc(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
}

/// What a registry row says needs the user, as an inbox item.
fn derived_item(row: &SessionRow, now_ms: i64) -> Option<AttentionItem> {
    use AttentionAction::{Dismiss, Open};
    let (kind, summary, actions) = match row.indicator {
        ChatIndicator::AwaitingApproval => (
            AttentionKind::Approval,
            "Waiting on your approval".to_owned(),
            vec![Open],
        ),
        ChatIndicator::AwaitingInput => (
            AttentionKind::Question,
            "Waiting on your answer".to_owned(),
            vec![Open],
        ),
        ChatIndicator::Limited => (
            AttentionKind::Limited,
            match row.limit.as_ref().and_then(|l| l.reset_at_ms) {
                Some(at) if at > now_ms => {
                    "Stopped on a usage limit until its window resets".into()
                }
                _ => "Stopped on a usage limit".into(),
            },
            vec![Open],
        ),
        ChatIndicator::Errored => (
            AttentionKind::Failed,
            row.preview
                .clone()
                .unwrap_or_else(|| "The last turn failed".into()),
            vec![Open, Dismiss],
        ),
        ChatIndicator::Completed => (
            AttentionKind::Finished,
            row.preview.clone().unwrap_or_else(|| "Finished".into()),
            vec![Open, Dismiss],
        ),
        ChatIndicator::Working | ChatIndicator::Idle => return None,
    };
    let since = match kind {
        AttentionKind::Approval | AttentionKind::Question => {
            row.working_since_ms.unwrap_or(row.last_activity_ms)
        }
        AttentionKind::Limited => row
            .limit
            .as_ref()
            .map_or(row.last_activity_ms, |l| l.limited_at_ms),
        _ => row.last_activity_ms,
    };
    Some(AttentionItem {
        id: format!("chat:{}", row.id),
        kind,
        chat_id: Some(row.id.clone()),
        title: row.title.clone(),
        space_id: row.project.as_ref().map(|p| p.id.clone()),
        project: row.project.as_ref().map(|p| p.name.clone()),
        harness: row
            .harness
            .as_deref()
            .and_then(|h| serde_json::from_value(json!(h)).ok()),
        created_at: utc(since),
        risk: None,
        summary,
        actions,
        request_id: None,
        approval: None,
        agent_run: None,
        conductor: None,
    })
}

/// One `WatchAttention` stream, retried 1s→30s; an engine without the
/// method marks its host unsupported (derived items from then on).
fn spawn_watch(weak: Weak<ClientInner>, device_id: String, cancel: CancellationToken) {
    crate::runtime::shared().spawn(async move {
        let mut backoff = BACKOFF_MIN;
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let Some(inner) = weak.upgrade() else { return };
            let Some(live) = inner.live() else { return };
            let links = live.relay.links();
            drop(inner);
            let subscribe = async {
                let client = links.client(&device_id).await?;
                client
                    .subscribe_checked(methods::WATCH_ATTENTION, json!({}))
                    .await
            };
            let subscription = tokio::select! {
                _ = cancel.cancelled() => return,
                result = subscribe => result,
            };
            match subscription {
                Ok(mut stream) => loop {
                    let item = tokio::select! {
                        _ = cancel.cancelled() => return,
                        item = stream.recv() => item,
                    };
                    let Some(item) = item else { break };
                    let Ok(snapshot) = serde_json::from_value::<AttentionSnapshot>(item) else {
                        continue;
                    };
                    backoff = BACKOFF_MIN;
                    let Some(inner) = weak.upgrade() else { return };
                    let revision = inner.attention.put(&device_id, snapshot);
                    inner.events.attention(revision);
                },
                Err(RpcError::UnknownMethod(_)) => {
                    if let Some(inner) = weak.upgrade() {
                        let revision = inner.attention.unsupported(&device_id);
                        inner.events.attention(revision);
                    }
                    return;
                }
                Err(RpcError::Closed | RpcError::Transport(_)) => links.invalidate(&device_id),
                Err(_) => {}
            }
            if let Some(inner) = weak.upgrade()
                && let Some(revision) = inner.attention.disconnected(&device_id)
            {
                inner.events.attention(revision);
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    });
}

impl ClientInner {
    /// Converge the attention streams onto every other execution host, only
    /// while foregrounded (live mode).
    pub(crate) fn reconcile_attention_watches(self: &Arc<Self>) {
        if self.live().is_none() {
            return;
        }
        if !self.is_foreground() {
            self.attention.stop();
            return;
        }
        let ws = self.workspace.snapshot();
        let targets: Vec<String> = ws
            .devices
            .iter()
            .filter(|d| d.is_execution_host && !d.is_self)
            .map(|d| d.id.clone())
            .collect();
        self.attention.reconcile(self, targets);
    }
}

// ── actions ───────────────────────────────────────────────────────────────

pub(crate) async fn respond(
    inner: &Arc<ClientInner>,
    device_id: &str,
    item_id: &str,
    approve: bool,
) -> crate::error::Result<()> {
    use crate::error::ClientError;
    let item = inner
        .attention
        .item(device_id, item_id)
        .ok_or_else(|| ClientError::NotFound(item_id.to_owned()))?;
    let decision = item
        .approval
        .as_ref()
        .and_then(|a| decision_for(a, approve))
        .ok_or_else(|| ClientError::HostError("this request has no such answer".into()))?;
    let revision = inner.attention.hide(device_id, item_id);
    inner.events.attention(revision);
    let result = match inner.backend() {
        crate::client::Backend::Demo(demo) => {
            demo.answer_attention(inner, device_id, &item, approve);
            Ok(())
        }
        crate::client::Backend::Live(live) => live
            .relay
            .call(
                device_id,
                methods::RESPOND_ATTENTION,
                json!({ "itemId": item_id, "decision": decision }),
            )
            .await
            .map(|_| ()),
    };
    if result.is_err() {
        let revision = inner.attention.unhide(device_id, item_id);
        inner.events.attention(revision);
    }
    result
}

pub(crate) async fn dismiss(
    inner: &Arc<ClientInner>,
    device_id: &str,
    item_id: &str,
) -> crate::error::Result<()> {
    let live_item = inner.attention.item(device_id, item_id);
    let revision = inner.attention.hide(device_id, item_id);
    inner.events.attention(revision);
    let Some(item) = live_item else {
        // Derived: dismissing a finished / failed chat marks it seen.
        if let Some(chat_id) = item_id.strip_prefix("chat:") {
            inner.mark_seen(chat_id);
        }
        return Ok(());
    };
    let result = match inner.backend() {
        crate::client::Backend::Demo(_) => {
            if let Some(revision) = inner
                .attention
                .edit(device_id, |s| s.items.retain(|i| i.id != item.id))
            {
                inner.events.attention(revision);
            }
            Ok(())
        }
        crate::client::Backend::Live(live) => live
            .relay
            .call(
                device_id,
                methods::DISMISS_ATTENTION,
                json!({ "itemId": item_id }),
            )
            .await
            .map(|_| ()),
    };
    if result.is_err() {
        let revision = inner.attention.unhide(device_id, item_id);
        inner.events.attention(revision);
    }
    result
}

pub(crate) async fn approve_low_risk(inner: &Arc<ClientInner>) -> BatchResult {
    let view = inner.attention.view(&inner.workspace.snapshot(), now_ms());
    let mut by_device: HashMap<String, Vec<String>> = HashMap::new();
    for entry in view.items.iter().filter(|e| e.batch_approvable()) {
        by_device
            .entry(entry.device_id.clone())
            .or_default()
            .push(entry.item.id.clone());
    }
    let mut result = BatchResult::default();
    let mut devices: Vec<(String, Vec<String>)> = by_device.into_iter().collect();
    devices.sort();
    for (device_id, ids) in devices {
        for id in &ids {
            inner.attention.hide(&device_id, id);
        }
        inner.events.attention(inner.attention.revision());
        let reply = match inner.backend() {
            crate::client::Backend::Demo(demo) => {
                Ok(demo.approve_low_risk(inner, &device_id, &ids))
            }
            crate::client::Backend::Live(live) => live
                .relay
                .call(
                    &device_id,
                    methods::APPROVE_LOW_RISK,
                    json!({ "itemIds": ids }),
                )
                .await
                .and_then(|v| {
                    serde_json::from_value::<BatchApproval>(v)
                        .map_err(|e| crate::error::ClientError::HostError(e.to_string()))
                }),
        };
        match reply {
            Ok(batch) => {
                result.approved += batch.approved.len() as u32;
                result.skipped += batch.skipped.len() as u32;
                for id in &batch.skipped {
                    inner.attention.unhide(&device_id, id);
                }
            }
            Err(err) => {
                tracing::debug!(device = %device_id, error = %err, "ApproveLowRisk failed");
                for id in &ids {
                    inner.attention.unhide(&device_id, id);
                }
                result.failed_devices.push(device_id.clone());
            }
        }
        inner.events.attention(inner.attention.revision());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{Lifecycle, ProjectRef};
    use zeron_proto::{ApprovalKind, ApprovalRisk};

    fn at(minute: i64) -> DateTime<Utc> {
        utc(1_790_000_000_000 + minute * 60_000)
    }

    fn approval(id: &str, risk: ApprovalRisk, minute: i64) -> AttentionItem {
        let mut request = ApprovalRequest::new(ApprovalKind::Command, "Run cargo test");
        request.command = Some("cargo test".into());
        AttentionItem {
            id: id.into(),
            kind: AttentionKind::Approval,
            chat_id: Some(format!("chat-{id}")),
            title: id.into(),
            space_id: None,
            project: None,
            harness: None,
            created_at: at(minute),
            risk: Some(risk),
            summary: "cargo test".into(),
            actions: vec![
                AttentionAction::Approve,
                AttentionAction::Decline,
                AttentionAction::Open,
            ],
            request_id: Some("r".into()),
            approval: Some(request),
            agent_run: None,
            conductor: None,
        }
    }

    fn row(id: &str, device: &str, indicator: ChatIndicator, minute: i64) -> Arc<SessionRow> {
        Arc::new(SessionRow {
            id: id.into(),
            revision: 0,
            title: id.into(),
            has_title: true,
            preview: Some("preview".into()),
            project: Some(ProjectRef {
                id: "space".into(),
                name: "godspeed".into(),
                color_index: 0,
            }),
            device_id: device.into(),
            device_name: Some(device.into()),
            device_online: true,
            harness: Some("claude-code".into()),
            harness_label: None,
            model: None,
            model_label: None,
            reasoning: None,
            branch: None,
            cwd: None,
            indicator,
            host_indicator: indicator,
            working_since_ms: None,
            last_activity_ms: at(minute).timestamp_millis(),
            time_label: "now".into(),
            created_at_ms: 0,
            unseen: true,
            archived: false,
            pinned: false,
            section_id: None,
            pull_request: None,
            send_state: None,
            parent_chat_id: None,
            room_gen: 2,
            running_subagents: 0,
            background_work: 0,
            limit: None,
            lifecycle: Lifecycle::Active,
        })
    }

    fn feed(items: Vec<AttentionItem>) -> Feed {
        Feed {
            support: Support::Supported,
            connected: true,
            snapshot: Some(AttentionSnapshot {
                device_id: String::new(),
                items,
            }),
        }
    }

    fn workspace(rows: Vec<Arc<SessionRow>>) -> WorkspaceSnapshot {
        let mut ws = WorkspaceSnapshot::default();
        ws.sessions = rows.into_iter().map(|r| (r.id.clone(), r)).collect();
        ws
    }

    #[test]
    fn merges_hosts_in_risk_order_and_derives_for_hosts_without_an_inbox() {
        let mut feeds = HashMap::new();
        feeds.insert(
            "mac".to_owned(),
            feed(vec![
                approval("low", ApprovalRisk::Low, 1),
                approval("high", ApprovalRisk::High, 5),
            ]),
        );
        feeds.insert(
            "vps".to_owned(),
            feed(vec![approval("medium", ApprovalRisk::Medium, 0)]),
        );
        let ws = workspace(vec![
            // Hosted on a live inbox: the engine's own item wins.
            row("on-mac", "mac", ChatIndicator::Completed, 0),
            // A 0.3 host: derived.
            row("old-failed", "old", ChatIndicator::Errored, 2),
            row("old-question", "old", ChatIndicator::AwaitingInput, 3),
            row("old-idle", "old", ChatIndicator::Idle, 3),
        ]);
        let view = merge(&feeds, &HashMap::new(), &ws, at(10).timestamp_millis(), 7);
        let order: Vec<&str> = view.items.iter().map(|e| e.item.id.as_str()).collect();
        assert_eq!(
            order,
            [
                "high",
                "chat:old-question",
                "medium",
                "low",
                "chat:old-failed"
            ]
        );
        assert_eq!(view.live_devices, ["mac", "vps"]);
        assert_eq!(view.low_risk_count(), 1);
        assert_eq!(view.badge_count(), 5);
        let derived = &view.items[1];
        assert!(derived.derived && !derived.can_approve() && !derived.can_dismiss());
        let failed = &view.items[4];
        assert!(failed.can_dismiss() && !failed.batch_approvable());
        assert!(view.items[0].can_approve() && view.items[0].can_decline());
    }

    #[test]
    fn hidden_items_drop_out_and_a_down_stream_falls_back_to_derived() {
        let mut feeds = HashMap::new();
        feeds.insert(
            "mac".to_owned(),
            feed(vec![approval("a", ApprovalRisk::Low, 0)]),
        );
        let mut hidden = HashMap::new();
        hidden.insert(("mac".to_owned(), "a".to_owned()), i64::MAX);
        let ws = workspace(vec![row("c", "mac", ChatIndicator::AwaitingApproval, 0)]);
        let view = merge(&feeds, &hidden, &ws, 0, 1);
        assert!(view.items.is_empty());
        feeds.get_mut("mac").unwrap().connected = false;
        let view = merge(&feeds, &hidden, &ws, 0, 2);
        assert_eq!(view.items.len(), 1);
        assert_eq!(view.items[0].item.kind, AttentionKind::Approval);
        assert!(view.items[0].derived);
    }

    #[test]
    fn one_tap_decisions_prefer_the_plain_options() {
        let request = ApprovalRequest::new(ApprovalKind::Command, "x").with_decisions(&[
            ApprovalDecision::ApproveForSession,
            ApprovalDecision::Cancel,
        ]);
        assert_eq!(
            decision_for(&request, true),
            Some(ApprovalDecision::ApproveForSession)
        );
        assert_eq!(
            decision_for(&request, false),
            Some(ApprovalDecision::Cancel)
        );
        let full = ApprovalRequest::new(ApprovalKind::Command, "x");
        assert_eq!(decision_for(&full, true), Some(ApprovalDecision::Approve));
        assert_eq!(decision_for(&full, false), Some(ApprovalDecision::Decline));
    }

    #[test]
    fn store_put_clears_hides_the_host_dropped() {
        let store = AttentionStore::default();
        store.put(
            "mac",
            AttentionSnapshot {
                device_id: "mac".into(),
                items: vec![approval("a", ApprovalRisk::Low, 0)],
            },
        );
        store.hide("mac", "a");
        assert!(lock(&store.hidden).contains_key(&("mac".into(), "a".into())));
        store.put(
            "mac",
            AttentionSnapshot {
                device_id: "mac".into(),
                items: vec![],
            },
        );
        assert!(lock(&store.hidden).is_empty());
    }
}
