//! The local engine's zoomies, followed live (`WatchAgents`) for the
//! sidebar, plus the jog clock its running rows animate on.

use std::time::Duration;

use gpui::{Context, Entity, Subscription, Task};
use serde_json::json;
use zeron_proto::agents::{AgentRunStatus, AgentsSnapshot};
use zeron_rpc::methods;

use crate::speedster::Mood;
use crate::state::{AppState, EngineHandle};

/// A dropped watch retries after this long.
const RETRY: Duration = Duration::from_secs(5);

pub struct ZoomiesFeed {
    state: Entity<AppState>,
    snapshot: Option<AgentsSnapshot>,
    /// The connection `watch` follows; a runtime swap restarts it.
    engine: Option<EngineHandle>,
    watch: Option<Task<()>>,
    /// Steps `jog_frame` while a run works and motion is allowed.
    ticker: Option<Task<()>>,
    jog_frame: usize,
    /// Set by the first [`Self::ensure_watching`]: nothing subscribes until
    /// the sidebar actually shows the section.
    wanted: bool,
    _observe: Subscription,
    _motion: Subscription,
}

impl ZoomiesFeed {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        let motion = cx.observe_global::<crate::motion::MotionState>(|this: &mut Self, cx| {
            this.sync_ticker(cx);
        });
        Self {
            state,
            snapshot: None,
            engine: None,
            watch: None,
            ticker: None,
            jog_frame: 0,
            wanted: false,
            _observe: observe,
            _motion: motion,
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

    /// Index into the jog frames for running rows.
    pub fn jog_frame(&self) -> usize {
        self.jog_frame % Mood::Jog.frames().len()
    }

    fn any_running(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.agents.iter().any(|agent| {
                agent.active_run_id.is_some()
                    && agent
                        .last_run
                        .as_ref()
                        .is_none_or(|run| run.status != AgentRunStatus::NeedsApproval)
            })
        })
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
            self.sync_ticker(cx);
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
                        feed.snapshot = Some(snapshot);
                        feed.sync_ticker(cx);
                        cx.notify();
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

    /// Keep one frame timer while something runs and motion is allowed.
    fn sync_ticker(&mut self, cx: &mut Context<Self>) {
        let animate = self.any_running() && !cx.reduce_motion();
        if !animate {
            self.ticker = None;
            self.jog_frame = 0;
            return;
        }
        if self.ticker.is_some() {
            return;
        }
        let interval = Mood::Jog
            .frame_interval()
            .unwrap_or(Duration::from_millis(125));
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                let alive = this.update(cx, |feed, cx| {
                    feed.jog_frame = feed.jog_frame.wrapping_add(1);
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
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
        self.snapshot = Some(snapshot);
        self.sync_ticker(cx);
        cx.notify();
    }
}
