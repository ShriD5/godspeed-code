//! The local engine's zoomies, followed live (`WatchAgents`) for the
//! sidebar. Running rows animate in their own small views
//! ([`super::mascot::jogging`]); this feed notifies its observers (the shell)
//! only when the snapshot actually changes.

use std::time::Duration;

use gpui::{Context, Entity, Subscription, Task};
use serde_json::json;
use zeron_proto::agents::AgentsSnapshot;
use zeron_rpc::methods;

use crate::state::{AppState, EngineHandle};

/// A dropped watch retries after this long.
const RETRY: Duration = Duration::from_secs(5);

pub struct ZoomiesFeed {
    state: Entity<AppState>,
    snapshot: Option<AgentsSnapshot>,
    /// The connection `watch` follows; a runtime swap restarts it.
    engine: Option<EngineHandle>,
    watch: Option<Task<()>>,
    /// Set by the first [`Self::ensure_watching`]: nothing subscribes until
    /// the sidebar actually shows the section.
    wanted: bool,
    _observe: Subscription,
}

impl ZoomiesFeed {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        Self {
            state,
            snapshot: None,
            engine: None,
            watch: None,
            wanted: false,
            _observe: observe,
        }
    }

    /// Start following the engine's zoomies (idempotent; call on render).
    pub fn ensure_watching(&mut self, cx: &mut Context<Self>) {
        if !self.wanted {
            self.wanted = true;
            self.sync(cx);
        }
    }

    /// `None` until the first snapshot lands.
    pub fn snapshot(&self) -> Option<&AgentsSnapshot> {
        self.snapshot.as_ref()
    }

    /// Take a snapshot; `false` (and no notify) when nothing changed, so an
    /// engine re-sending the same list never re-renders the shell.
    fn apply_snapshot(&mut self, snapshot: AgentsSnapshot) -> bool {
        if self.snapshot.as_ref() == Some(&snapshot) {
            return false;
        }
        self.snapshot = Some(snapshot);
        true
    }

    /// Follow the current engine; restart when it changes or the watch died.
    fn sync(&mut self, cx: &mut Context<Self>) {
        if !self.wanted {
            return;
        }
        let engine = self.state.read(cx).engine().cloned();
        let same = match (&engine, &self.engine) {
            (Some(now), Some(before)) => now.same_connection(before),
            (None, None) => true,
            _ => false,
        };
        if same && (self.watch.is_some() || engine.is_none()) {
            return;
        }
        if !same {
            self.snapshot = None;
        }
        self.engine = engine.clone();
        let Some(engine) = engine else {
            self.watch = None;
            cx.notify();
            return;
        };
        // Like the app state's own watches: the client's streams run on its
        // transport, so this needs no runtime of its own.
        self.watch = Some(cx.spawn(async move |this, cx| {
            if let Ok(mut items) = engine
                .client()
                .subscribe(methods::WATCH_AGENTS, json!({}))
                .await
            {
                while let Some(item) = items.recv().await {
                    let Ok(snapshot) = serde_json::from_value::<AgentsSnapshot>(item) else {
                        continue;
                    };
                    let alive = this.update(cx, |feed, cx| {
                        if feed.apply_snapshot(snapshot) {
                            cx.notify();
                        }
                    });
                    if alive.is_err() {
                        return;
                    }
                }
            }
            // Older engines without zoomies end here too: back off, retry.
            cx.background_executor().timer(RETRY).await;
            this.update(cx, |feed, cx| {
                feed.watch = None;
                feed.sync(cx);
            })
            .ok();
        }));
    }

    /// Fire an agent RPC on the local engine; failures toast.
    pub fn call(&self, method: &'static str, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            crate::toast::error(cx, "Engine not connected");
            return;
        };
        cx.spawn(async move |_, cx| {
            if let Err(err) = engine.client().call(method, params).await {
                let message = err.to_string();
                cx.update(|cx| crate::toast::error(cx, message));
            }
        })
        .detach();
    }

    #[cfg(any(test, feature = "demo-fixture"))]
    pub(crate) fn set_snapshot_for_test(
        &mut self,
        snapshot: AgentsSnapshot,
        cx: &mut Context<Self>,
    ) {
        if self.apply_snapshot(snapshot) {
            cx.notify();
        }
    }
}
