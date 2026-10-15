//! Thread lifecycle against a real engine: settle, snooze and wake persist on
//! the chat row, a snooze wakes at its end across a restart, new activity
//! wakes a chat, the opt-in auto-settle spares threads that need the user,
//! and an undo never overwrites a newer state. The timer is driven through
//! `tick_at`, so the clock is the test's.

use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};

use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_proto::{
    ChatLifecycleSettings, HarnessId, LifecycleState, Session, SessionStatus, SidebarPinChange,
};
use zeron_rpc::methods;

struct World {
    _tmp: tempfile::TempDir,
    data_dir: std::path::PathBuf,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        Self {
            data_dir: tmp.path().join("data"),
            _tmp: tmp,
        }
    }

    fn assemble(&self) -> EngineCore {
        EngineCore::assemble(
            &self.data_dir,
            Arc::new(HarnessRegistry::new()),
            HarnessId::Mock,
            None,
        )
        .expect("engine core assembles")
    }
}

async fn mutate(core: &EngineCore, params: Value) -> Result<Value, String> {
    zeron_rpc::memory_client(core.rpc_service())
        .call(methods::MUTATE, params)
        .await
        .map_err(|err| err.to_string())
}

/// An idle chat whose last message is `idle_for` old and already read.
async fn idle_chat(core: &EngineCore, chat_id: &str, idle_for: TimeDelta) {
    mutate(
        core,
        json!({ "op": "createChat", "chatId": chat_id, "deviceId": core.device_id }),
    )
    .await
    .expect("createChat");
    core.workspace.rename_chat(chat_id, "Pre-titled").unwrap();
    let last = (Utc::now() - idle_for).timestamp_millis();
    mutate(
        core,
        json!({ "op": "setChatActivity", "chatId": chat_id, "lastMessageAt": last, "createdAt": last }),
    )
    .await
    .expect("setChatActivity");
    mutate(core, json!({ "op": "markChatSeen", "chatId": chat_id }))
        .await
        .expect("markChatSeen");
}

async fn set_lifecycle(
    core: &EngineCore,
    chat_id: &str,
    lifecycle: LifecycleState,
) -> Result<Value, String> {
    mutate(
        core,
        json!({ "op": "setChatLifecycle", "chatId": chat_id, "lifecycle": lifecycle }),
    )
    .await
}

fn state(core: &EngineCore, chat_id: &str) -> LifecycleState {
    core.workspace
        .chat(chat_id)
        .unwrap()
        .expect("chat row")
        .lifecycle
        .state
}

/// Whole seconds: the registry row stores what serde writes.
fn in_(delta: TimeDelta) -> DateTime<Utc> {
    DateTime::from_timestamp((Utc::now() + delta).timestamp(), 0).unwrap()
}

fn status(core: &EngineCore, chat_id: &str, status: SessionStatus) {
    core.workspace.record_session(&Session {
        last_completed_turn: None,
        chat_id: chat_id.into(),
        device_id: core.device_id.clone(),
        status,
        started_at: Some(Utc::now()),
        updated_at: Utc::now(),
        running_subagents: 0,
        background_tasks: 0,
        awaiting_approval: false,
    });
}

async fn set_settings(core: &EngineCore, days: Option<u32>) -> ChatLifecycleSettings {
    let reply = zeron_rpc::memory_client(core.rpc_service())
        .call(
            methods::SET_CHAT_LIFECYCLE_SETTINGS,
            json!({ "autoSettleAfterDays": days }),
        )
        .await
        .expect("SetChatLifecycleSettings");
    serde_json::from_value(reply).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settle_snooze_and_wake_persist_and_bad_snoozes_are_refused() {
    let world = World::new();
    let core = world.assemble();
    idle_chat(&core, "chat", TimeDelta::hours(1)).await;
    assert_eq!(state(&core, "chat"), LifecycleState::Active);

    set_lifecycle(&core, "chat", LifecycleState::Settled)
        .await
        .unwrap();
    assert_eq!(state(&core, "chat"), LifecycleState::Settled);

    let until = in_(TimeDelta::hours(3));
    set_lifecycle(&core, "chat", LifecycleState::Snoozed { until })
        .await
        .unwrap();
    for bad in [in_(-TimeDelta::minutes(1)), in_(TimeDelta::days(400))] {
        let refused = set_lifecycle(&core, "chat", LifecycleState::Snoozed { until: bad }).await;
        assert!(refused.is_err(), "a snooze ending at {bad} is refused");
    }
    assert_eq!(state(&core, "chat"), LifecycleState::Snoozed { until });

    core.shutdown().await;
    drop(core);
    let core = world.assemble();
    assert_eq!(state(&core, "chat"), LifecycleState::Snoozed { until });
    set_lifecycle(&core, "chat", LifecycleState::Active)
        .await
        .unwrap();
    assert_eq!(state(&core, "chat"), LifecycleState::Active);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snooze_wakes_at_its_end_after_a_restart() {
    let world = World::new();
    let core = world.assemble();
    idle_chat(&core, "chat", TimeDelta::hours(1)).await;
    let until = in_(TimeDelta::hours(2));
    set_lifecycle(&core, "chat", LifecycleState::Snoozed { until })
        .await
        .unwrap();
    core.shutdown().await;
    drop(core);

    let core = world.assemble();
    assert_eq!(
        core.chat_lifecycle.tick_at(until - TimeDelta::minutes(1)),
        Some(until),
        "the timer wakes for the snooze's end"
    );
    assert_eq!(state(&core, "chat"), LifecycleState::Snoozed { until });
    // The machine slept through the end: the first tick after it wakes.
    core.chat_lifecycle.tick_at(until + TimeDelta::hours(5));
    assert_eq!(state(&core, "chat"), LifecycleState::Active);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_wakes_a_chat_and_live_threads_cannot_be_hidden() {
    let world = World::new();
    let core = world.assemble();
    idle_chat(&core, "chat", TimeDelta::hours(1)).await;

    set_lifecycle(&core, "chat", LifecycleState::Settled)
        .await
        .unwrap();
    core.workspace.note_message("chat", "a new message");
    assert_eq!(state(&core, "chat"), LifecycleState::Active);

    let until = in_(TimeDelta::hours(1));
    set_lifecycle(&core, "chat", LifecycleState::Snoozed { until })
        .await
        .unwrap();
    status(&core, "chat", SessionStatus::AwaitingInput);
    assert_eq!(state(&core, "chat"), LifecycleState::Active);

    let refused = set_lifecycle(&core, "chat", LifecycleState::Settled).await;
    assert!(
        refused.is_err(),
        "a thread waiting on the user stays listed"
    );
    assert_eq!(state(&core, "chat"), LifecycleState::Active);

    // An idle status never wakes, and a repeated one does not either.
    status(&core, "chat", SessionStatus::Idle);
    set_lifecycle(&core, "chat", LifecycleState::Settled)
        .await
        .unwrap();
    status(&core, "chat", SessionStatus::Idle);
    assert_eq!(state(&core, "chat"), LifecycleState::Settled);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_settle_is_opt_in_and_spares_threads_that_need_the_user() {
    let world = World::new();
    let core = world.assemble();
    let now = Utc::now();
    idle_chat(&core, "stale", TimeDelta::days(5)).await;
    idle_chat(&core, "pinned", TimeDelta::days(5)).await;
    idle_chat(&core, "fresh", TimeDelta::hours(1)).await;
    idle_chat(&core, "unread", TimeDelta::days(5)).await;
    mutate(&core, json!({ "op": "markChatUnread", "chatId": "unread" }))
        .await
        .unwrap();
    core.workspace
        .change_sidebar_pin(&SidebarPinChange::Pin {
            session_id: "pinned".into(),
            after: None,
            before: None,
        })
        .unwrap();

    assert_eq!(core.chat_lifecycle.tick_at(now), None, "off by default");
    assert_eq!(state(&core, "stale"), LifecycleState::Active);

    assert_eq!(
        set_settings(&core, Some(3)).await.auto_settle_after_days,
        Some(3)
    );
    let next = core.chat_lifecycle.tick_at(now);
    assert_eq!(state(&core, "stale"), LifecycleState::Settled);
    for spared in ["pinned", "fresh", "unread"] {
        assert_eq!(state(&core, spared), LifecycleState::Active, "{spared}");
    }
    let fresh_due = core
        .workspace
        .chat("fresh")
        .unwrap()
        .unwrap()
        .last_message_at
        .unwrap()
        + TimeDelta::days(3);
    assert_eq!(
        next,
        Some(fresh_due),
        "the timer wakes for the next idle thread"
    );

    // Waking a settled thread restarts its idle clock instead of settling
    // it again on the next tick.
    set_lifecycle(&core, "stale", LifecycleState::Active)
        .await
        .unwrap();
    core.chat_lifecycle.tick_at(Utc::now());
    assert_eq!(state(&core, "stale"), LifecycleState::Active);

    core.shutdown().await;
    drop(core);
    let core = world.assemble();
    assert_eq!(
        core.chat_lifecycle.settings().auto_settle_after_days,
        Some(3)
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undo_applies_only_while_the_state_it_undoes_still_holds() {
    let world = World::new();
    let core = world.assemble();
    idle_chat(&core, "chat", TimeDelta::hours(1)).await;
    let now = Utc::now();

    set_lifecycle(&core, "chat", LifecycleState::Settled)
        .await
        .unwrap();
    let undone = mutate(
        &core,
        json!({ "op": "setChatLifecycle", "chatId": "chat",
                "lifecycle": LifecycleState::Active, "expected": LifecycleState::Settled }),
    )
    .await;
    assert!(undone.is_ok());
    assert_eq!(state(&core, "chat"), LifecycleState::Active);

    // New activity got there first: the undo leaves the newer state alone.
    let until = in_(TimeDelta::hours(1));
    set_lifecycle(&core, "chat", LifecycleState::Snoozed { until })
        .await
        .unwrap();
    core.workspace.note_message("chat", "activity");
    let stale = core.workspace.change_chat_lifecycle(
        "chat",
        LifecycleState::Settled,
        Some(LifecycleState::Snoozed { until }),
        now,
    );
    assert_eq!(stale.ok(), Some(false));
    assert_eq!(state(&core, "chat"), LifecycleState::Active);

    // Mark unread round-trips through the synced seen marker.
    mutate(&core, json!({ "op": "markChatUnread", "chatId": "chat" }))
        .await
        .unwrap();
    assert!(core.workspace.chat("chat").unwrap().unwrap().unseen());
    mutate(&core, json!({ "op": "markChatSeen", "chatId": "chat" }))
        .await
        .unwrap();
    assert!(!core.workspace.chat("chat").unwrap().unwrap().unseen());
    core.shutdown().await;
}
