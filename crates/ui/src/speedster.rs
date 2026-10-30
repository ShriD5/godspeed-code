//! The speedster: a chibi mascot in the Dock that shows what the agents are
//! doing (design/speedster/PLAN.md). Zero model tokens: it only reads the
//! session rows and the "Needs you" queue the app already holds, so chats on
//! other devices move it too.
//!
//! [`Speedster`] is the pure state machine (counts and attention items in, a
//! [`Mood`] and a [`Plan`] out); `dock` is the macOS driver that swaps
//! `NSApp.applicationIconImage` frames only while the plan animates.
//!
//! Idle costs nothing: working moods animate while agents run, but a quiet
//! speedster jogs in place for [`SLEEP_AFTER`] and then settles on one still
//! sleeping frame with no timer, and a waiting alert settles after
//! [`ALERT_HOLD`]. On battery, in Low Power Mode, or with every window
//! hidden or covered, frames step at most [`POWER_SAVING_MAX_FPS`]. Hidden
//! windows are the usual case while agents work (the user is in another
//! app), and then the Dock is the only sign of it, so working moods keep
//! running there; only the quiet idle pose holds still.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use zeron_proto::Session;
use zeron_proto::attention::{AttentionKind, AttentionSnapshot};
use zeron_proto::view::{Indicator, effective_indicator};

/// Frame rate ceiling for any mood.
pub const MAX_FPS: f32 = 12.0;
/// How long a fresh finish or failure holds the celebrate / oops pose.
pub const TRANSIENT: Duration = Duration::from_secs(2);
/// Quiet this long (nothing working, nothing waiting) and the speedster naps:
/// one still sleeping frame, no timer.
pub const SLEEP_AFTER: Duration = Duration::from_secs(45);
/// A waiting alert waves this long, then holds its first frame.
pub const ALERT_HOLD: Duration = Duration::from_secs(60);
/// Frame-rate ceiling on battery, in Low Power Mode, or with every window
/// hidden or covered.
pub const POWER_SAVING_MAX_FPS: f32 = 4.0;
/// After the first attention snapshot from an engine, items keep arriving as
/// its watch catches up. Nothing in that window is news.
pub const SETTLE: Duration = Duration::from_secs(5);
/// From this many working sessions the jog becomes a sprint.
pub const SPRINT_FROM: usize = 4;

/// One embedded artwork frame (`assets/speedster/<name>@512.png`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Frame {
    Idle1,
    Idle2,
    Sleeping1,
    Sleeping2,
    Jog1,
    Jog2,
    Jog3,
    Jog4,
    Sprint1,
    Sprint2,
    Sprint3,
    Sprint4,
    Alert1,
    Alert2,
    Celebrate1,
    Celebrate2,
    Tired,
    Oops,
}

impl Frame {
    pub const COUNT: usize = 18;

    pub const fn index(self) -> usize {
        self as usize
    }
}

/// What the speedster is showing. Declared lowest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Mood {
    Sleeping,
    Idle,
    Jog,
    Sprint,
    Tired,
    Celebrate,
    Oops,
    Alert,
}

impl Mood {
    pub const fn frames(self) -> &'static [Frame] {
        match self {
            Self::Sleeping => &[Frame::Sleeping1, Frame::Sleeping2],
            Self::Idle => &[Frame::Idle1, Frame::Idle2],
            Self::Jog => &[Frame::Jog1, Frame::Jog2, Frame::Jog3, Frame::Jog4],
            Self::Sprint => &[
                Frame::Sprint1,
                Frame::Sprint2,
                Frame::Sprint3,
                Frame::Sprint4,
            ],
            Self::Tired => &[Frame::Tired],
            Self::Celebrate => &[Frame::Celebrate1, Frame::Celebrate2],
            Self::Oops => &[Frame::Oops],
            Self::Alert => &[Frame::Alert1, Frame::Alert2],
        }
    }

    /// Frames per second; 0 holds the first frame. (The Dock holds even
    /// Sleeping still: see [`Speedster::plan`].)
    pub const fn fps(self) -> f32 {
        match self {
            Self::Sleeping | Self::Idle => 0.5,
            Self::Jog => 8.0,
            Self::Sprint => 12.0,
            Self::Alert => 3.0,
            Self::Celebrate => 6.0,
            Self::Tired | Self::Oops => 0.0,
        }
    }

    /// Time between frames, capped at [`MAX_FPS`]; `None` for still moods.
    pub fn frame_interval(self) -> Option<Duration> {
        let fps = self.fps().min(MAX_FPS);
        (fps > 0.0 && self.frames().len() > 1).then(|| Duration::from_secs_f32(1.0 / fps))
    }
}

/// What keeps the Dock driver awake between app-state changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Nothing: the icon holds its frame until the app state changes.
    None,
    /// Step a frame every interval.
    Animate(Duration),
    /// One wake-up for a transient ending or the nap starting.
    Wake(Instant),
}

/// What the driver knows about the world outside the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Conditions {
    /// Reduce Motion (the preference or the OS): every mood holds still.
    pub reduced_motion: bool,
    /// Every window hidden, minimized or covered: the Dock is the only sign
    /// of work, so working moods animate (capped); the quiet idle pose holds
    /// still.
    pub hidden: bool,
    /// On battery or in Low Power Mode.
    pub power_saving: bool,
}

/// `(working, awaiting input)` across every device's session rows,
/// staleness-checked so a crashed engine never keeps the speedster running.
pub fn session_activity(sessions: &[Session], now: DateTime<Utc>) -> (usize, usize) {
    sessions.iter().fold(
        (0, 0),
        |(working, awaiting), session| match effective_indicator(Some(session), now) {
            Indicator::Working => (working + 1, awaiting),
            Indicator::AwaitingInput => (working, awaiting + 1),
            Indicator::None | Indicator::Errored => (working, awaiting),
        },
    )
}

/// What the machine reads from `AppState` on each change.
#[derive(Debug, Clone, Copy)]
pub struct Observation<'a> {
    pub working: usize,
    /// Sessions paused on a question or approval, on any device.
    pub awaiting_input: usize,
    /// An empty `device_id` means no engine has answered yet.
    pub attention: &'a AttentionSnapshot,
}

impl<'a> Observation<'a> {
    pub fn of(state: &'a crate::state::AppState, now: DateTime<Utc>) -> Self {
        let (working, awaiting_input) = session_activity(&state.sessions, now);
        Self {
            working,
            awaiting_input,
            attention: &state.attention,
        }
    }
}

/// The engine whose queue the machine has seen, and when its catch-up ends.
#[derive(Debug)]
struct Baseline {
    device_id: String,
    settles_at: Instant,
}

/// Pure mood state machine. [`Self::observe`] folds in a change, and
/// [`Self::mood`] / [`Self::next_deadline`] answer for any later instant, so a
/// timer can expire the transients without re-reading the app state.
#[derive(Debug)]
pub struct Speedster {
    /// Anything that needed the user before the speedster appeared is old.
    started_at: DateTime<Utc>,
    baseline: Option<Baseline>,
    /// Finished and Failed items already accounted for, by id.
    seen: HashMap<String, AttentionKind>,
    working: usize,
    alert: bool,
    tired: bool,
    celebrate_until: Option<Instant>,
    oops_until: Option<Instant>,
    /// Since when nothing has been working, waiting or limited.
    quiet_since: Option<Instant>,
    /// Since when something has been waiting on the user.
    alert_since: Option<Instant>,
}

impl Speedster {
    pub fn new(started_at: DateTime<Utc>) -> Self {
        Self {
            started_at,
            baseline: None,
            seen: HashMap::new(),
            working: 0,
            alert: false,
            tired: false,
            celebrate_until: None,
            oops_until: None,
            quiet_since: None,
            alert_since: None,
        }
    }

    /// Fold in the latest app state. Allocates only when the set of Finished
    /// and Failed items actually changed.
    pub fn observe(&mut self, observation: Observation<'_>, now: Instant) {
        let attention = observation.attention;
        self.working = observation.working;
        let alert = observation.awaiting_input > 0
            || attention
                .items
                .iter()
                .any(|item| matches!(item.kind, AttentionKind::Approval | AttentionKind::Question));
        if !alert {
            self.alert_since = None;
        } else if !self.alert {
            self.alert_since = Some(now);
        }
        self.alert = alert;
        self.tired = attention
            .items
            .iter()
            .any(|item| item.kind == AttentionKind::Limited);
        self.observe_outcomes(attention, now);

        if self.working > 0 || self.alert || self.tired {
            self.quiet_since = None;
        } else if self.quiet_since.is_none() {
            self.quiet_since = Some(now);
        }
    }

    /// Celebrate / oops fire for Finished / Failed items that are new since the
    /// last snapshot, never for the ones already queued at launch.
    fn observe_outcomes(&mut self, attention: &AttentionSnapshot, now: Instant) {
        if attention.device_id.is_empty() {
            self.baseline = None;
            self.seen.clear();
            return;
        }
        let rebased = self
            .baseline
            .as_ref()
            .is_none_or(|baseline| baseline.device_id != attention.device_id);
        if rebased {
            self.baseline = Some(Baseline {
                device_id: attention.device_id.clone(),
                settles_at: now + SETTLE,
            });
        }
        let settling = self
            .baseline
            .as_ref()
            .is_some_and(|baseline| now < baseline.settles_at);

        let outcomes = || {
            attention
                .items
                .iter()
                .filter(|item| matches!(item.kind, AttentionKind::Finished | AttentionKind::Failed))
        };
        let mut changed = rebased;
        let mut tracked = 0;
        let (mut finished, mut failed) = (false, false);
        for item in outcomes() {
            tracked += 1;
            if self.seen.get(&item.id) == Some(&item.kind) {
                continue;
            }
            changed = true;
            if settling || item.created_at < self.started_at {
                continue;
            }
            match item.kind {
                AttentionKind::Finished => finished = true,
                _ => failed = true,
            }
        }
        if changed || tracked != self.seen.len() {
            self.seen.clear();
            self.seen
                .extend(outcomes().map(|item| (item.id.clone(), item.kind)));
        }
        if finished {
            self.celebrate_until = Some(now + TRANSIENT);
        }
        if failed {
            self.oops_until = Some(now + TRANSIENT);
        }
    }

    /// Priority: alert > oops > celebrate > tired > sprint > jog > idle > sleeping.
    pub fn mood(&self, now: Instant) -> Mood {
        if self.alert {
            Mood::Alert
        } else if self.oops_until.is_some_and(|until| now < until) {
            Mood::Oops
        } else if self.celebrate_until.is_some_and(|until| now < until) {
            Mood::Celebrate
        } else if self.tired {
            Mood::Tired
        } else if self.working >= SPRINT_FROM {
            Mood::Sprint
        } else if self.working > 0 {
            Mood::Jog
        } else if self.sleeps_at().is_some_and(|at| now >= at) {
            Mood::Sleeping
        } else {
            Mood::Idle
        }
    }

    /// The next instant the mood can change without a new observation: a
    /// transient ending or the nap starting.
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        [self.oops_until, self.celebrate_until, self.sleeps_at()]
            .into_iter()
            .flatten()
            .filter(|&at| at > now)
            .min()
    }

    /// How the Dock driver shows the mood at `now`. Animates a mood with
    /// frames to step while motion is allowed; never asleep (one still frame,
    /// the one with the "z"), never a settled alert, and never the idle pose
    /// while every window is hidden. Power saving and hidden windows cap the
    /// rate at [`POWER_SAVING_MAX_FPS`]. Otherwise one wake for the next mood
    /// change, or nothing at all.
    pub fn plan(&self, now: Instant, conditions: Conditions) -> Plan {
        let mood = self.mood(now);
        let settled = mood == Mood::Sleeping
            || (mood == Mood::Alert
                && self
                    .alert_since
                    .is_some_and(|since| now >= since + ALERT_HOLD))
            || (conditions.hidden && mood == Mood::Idle);
        let animate = !conditions.reduced_motion && !settled;
        let capped = conditions.power_saving || conditions.hidden;
        match mood.frame_interval().filter(|_| animate) {
            Some(interval) if capped => {
                Plan::Animate(interval.max(Duration::from_secs_f32(1.0 / POWER_SAVING_MAX_FPS)))
            }
            Some(interval) => Plan::Animate(interval),
            None => self.next_deadline(now).map_or(Plan::None, Plan::Wake),
        }
    }

    fn sleeps_at(&self) -> Option<Instant> {
        let quiet = self.quiet_since?;
        let settled = [self.oops_until, self.celebrate_until]
            .into_iter()
            .flatten()
            .fold(quiet, Instant::max);
        Some(settled + SLEEP_AFTER)
    }
}

/// On battery or in Low Power Mode, read fresh (a cheap syscall); callers
/// cache it. Always false outside macOS and in tests.
pub(crate) fn power_saving_now() -> bool {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        dock::low_power() || dock::on_battery()
    }
    #[cfg(not(all(target_os = "macos", not(test))))]
    {
        false
    }
}

/// Start the Dock driver. Off outside macOS and in tests.
pub fn init(state: gpui::Entity<crate::state::AppState>, cx: &mut gpui::App) {
    #[cfg(all(target_os = "macos", not(test)))]
    dock::init(state, cx);
    #[cfg(not(all(target_os = "macos", not(test))))]
    let _ = (state, cx);
}

#[cfg(all(target_os = "macos", not(test)))]
mod dock {
    use std::time::{Duration, Instant};

    use gpui::{App, BorrowAppContext, Entity, Global, Task, WeakEntity};
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2::rc::Retained;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{NSData, NSProcessInfo};

    use super::{Conditions, Frame, Mood, Observation, Plan, Speedster};
    use crate::state::AppState;

    /// Same order as [`Frame`].
    static PNGS: [&[u8]; Frame::COUNT] = [
        include_bytes!("../assets/speedster/idle_1@512.png"),
        include_bytes!("../assets/speedster/idle_2@512.png"),
        include_bytes!("../assets/speedster/sleeping_1@512.png"),
        include_bytes!("../assets/speedster/sleeping_2@512.png"),
        include_bytes!("../assets/speedster/jog_1@512.png"),
        include_bytes!("../assets/speedster/jog_2@512.png"),
        include_bytes!("../assets/speedster/jog_3@512.png"),
        include_bytes!("../assets/speedster/jog_4@512.png"),
        include_bytes!("../assets/speedster/sprint_1@512.png"),
        include_bytes!("../assets/speedster/sprint_2@512.png"),
        include_bytes!("../assets/speedster/sprint_3@512.png"),
        include_bytes!("../assets/speedster/sprint_4@512.png"),
        include_bytes!("../assets/speedster/alert_1@512.png"),
        include_bytes!("../assets/speedster/alert_2@512.png"),
        include_bytes!("../assets/speedster/celebrate_1@512.png"),
        include_bytes!("../assets/speedster/celebrate_2@512.png"),
        include_bytes!("../assets/speedster/tired@512.png"),
        include_bytes!("../assets/speedster/oops@512.png"),
    ];

    /// How long a battery / Low Power Mode reading is trusted. Only read
    /// while syncing, which a still icon never does on its own.
    const POWER_TTL: Duration = Duration::from_secs(30);

    struct Dock {
        state: WeakEntity<AppState>,
        /// `None` while the setting is off.
        machine: Option<Speedster>,
        /// Decoded once, on first show.
        images: [Option<Retained<NSImage>>; Frame::COUNT],
        /// What AppKit holds now; `None` is the bundle icon.
        shown: Option<Frame>,
        mood: Option<Mood>,
        frame: usize,
        plan: Plan,
        /// Last power reading: (when, saving power).
        power: Option<(Instant, bool)>,
        _task: Option<Task<()>>,
    }

    impl Global for Dock {}

    pub(super) fn init(state: Entity<AppState>, cx: &mut App) {
        cx.set_global(Dock {
            state: state.downgrade(),
            machine: None,
            images: Default::default(),
            shown: None,
            mood: None,
            frame: 0,
            plan: Plan::None,
            power: None,
            _task: None,
        });
        cx.observe(&state, |_, cx| sync(cx, true)).detach();
        // The setting toggle, and Reduce Motion (preference or OS).
        cx.observe_global::<crate::settings::SettingsStore>(|cx| sync(cx, false))
            .detach();
        cx.observe_global::<crate::motion::MotionState>(|cx| sync(cx, false))
            .detach();
        // Windows shown, hidden or covered.
        cx.observe_global::<crate::app_visibility::AppVisibility>(|cx| sync(cx, false))
            .detach();
        sync(cx, true);
    }

    fn sync(cx: &mut App, reread: bool) {
        if cx.has_global::<Dock>() {
            cx.update_global::<Dock, _>(|dock, cx| dock.sync(reread, cx));
        }
    }

    impl Dock {
        fn sync(&mut self, reread: bool, cx: &mut App) {
            if !crate::settings::animated_dock_icon(cx) {
                self.machine = None;
                self.mood = None;
                self.show(None);
                self.set_plan(Plan::None, cx);
                return;
            }
            let now = Instant::now();
            let conditions = Conditions {
                reduced_motion: crate::motion::reduced_outside_window(cx),
                hidden: !crate::app_visibility::is_visible(cx),
                power_saving: self.power_saving(now),
            };
            let reread = reread || self.machine.is_none();
            let machine = self
                .machine
                .get_or_insert_with(|| Speedster::new(chrono::Utc::now()));
            if reread && let Some(state) = self.state.upgrade() {
                machine.observe(Observation::of(state.read(cx), chrono::Utc::now()), now);
            }
            let mood = machine.mood(now);
            if self.mood != Some(mood) {
                self.mood = Some(mood);
                self.frame = 0;
            }
            let plan = machine.plan(now, conditions);
            if !matches!(plan, Plan::Animate(_)) {
                // Still: the mood's first frame (asleep: the one with the "z").
                self.frame = 0;
            }
            let frames = mood.frames();
            self.show(Some(frames[self.frame % frames.len()]));
            self.set_plan(plan, cx);
        }

        fn power_saving(&mut self, now: Instant) -> bool {
            match self.power {
                Some((at, saving)) if now.saturating_duration_since(at) < POWER_TTL => saving,
                _ => {
                    let saving = super::power_saving_now();
                    self.power = Some((now, saving));
                    saving
                }
            }
        }

        fn step(&mut self, cx: &mut App) {
            if let Some(mood) = self.mood {
                self.frame = (self.frame + 1) % mood.frames().len();
            }
            self.sync(false, cx);
        }

        fn set_plan(&mut self, plan: Plan, cx: &mut App) {
            if self.plan == plan {
                return;
            }
            self.plan = plan;
            self._task = match plan {
                Plan::None => None,
                Plan::Animate(interval) => Some(cx.spawn(async move |cx| {
                    loop {
                        cx.background_executor().timer(interval).await;
                        let stepped = cx.update(|cx| {
                            cx.has_global::<Dock>()
                                && cx.update_global::<Dock, _>(|dock, cx| {
                                    dock.step(cx);
                                    true
                                })
                        });
                        if !stepped {
                            break;
                        }
                    }
                })),
                Plan::Wake(at) => Some(cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(at.saturating_duration_since(Instant::now()))
                        .await;
                    cx.update(|cx| {
                        if cx.has_global::<Dock>() {
                            cx.update_global::<Dock, _>(|dock, cx| {
                                // This task is done: an early wake must be able
                                // to schedule the same deadline again.
                                dock.plan = Plan::None;
                                dock.sync(false, cx);
                            });
                        }
                    });
                })),
            };
        }

        /// Touch AppKit only when the frame actually changes.
        fn show(&mut self, frame: Option<Frame>) {
            if self.shown == frame {
                return;
            }
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let image = match frame {
                Some(frame) => {
                    let slot = &mut self.images[frame.index()];
                    if slot.is_none() {
                        let data = NSData::with_bytes(PNGS[frame.index()]);
                        *slot = NSImage::initWithData(NSImage::alloc(), &data);
                    }
                    let Some(image) = slot.as_deref() else {
                        tracing::warn!(?frame, "Could not decode speedster frame");
                        return;
                    };
                    Some(image)
                }
                None => None,
            };
            // SAFETY: nil restores the bundle icon (documented AppKit behavior).
            unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(image) };
            self.shown = frame;
        }
    }

    pub(super) fn low_power() -> bool {
        NSProcessInfo::processInfo().isLowPowerModeEnabled()
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        /// Seconds of battery left; `kIOPSTimeRemainingUnlimited` (-2) on
        /// external power, `kIOPSTimeRemainingUnknown` (-1) while on battery
        /// and still estimating.
        fn IOPSGetTimeRemainingEstimate() -> f64;
    }

    pub(super) fn on_battery() -> bool {
        // SAFETY: no arguments; a plain read of the power source state.
        let remaining = unsafe { IOPSGetTimeRemainingEstimate() };
        (remaining + 2.0).abs() > 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use zeron_proto::SessionStatus;
    use zeron_proto::attention::AttentionItem;

    fn item(id: &str, kind: AttentionKind, created_at: DateTime<Utc>) -> AttentionItem {
        AttentionItem {
            id: id.into(),
            kind,
            chat_id: None,
            title: String::new(),
            space_id: None,
            project: None,
            harness: None,
            created_at,
            risk: None,
            summary: String::new(),
            actions: Vec::new(),
            request_id: None,
            approval: None,
            agent_run: None,
            conductor: None,
        }
    }

    fn snapshot(items: Vec<AttentionItem>) -> AttentionSnapshot {
        AttentionSnapshot {
            device_id: "dev".into(),
            items,
        }
    }

    fn session(device: &str, status: SessionStatus, now: DateTime<Utc>) -> Session {
        Session {
            last_completed_turn: None,
            chat_id: format!("{device}-{status:?}"),
            device_id: device.into(),
            status,
            started_at: None,
            updated_at: now,
            running_subagents: 0,
            background_tasks: 0,
            awaiting_approval: false,
        }
    }

    /// A machine whose engine baseline has already settled at `t0 + SETTLE`.
    struct Rig {
        machine: Speedster,
        t0: Instant,
        wall: DateTime<Utc>,
    }

    impl Rig {
        fn new() -> Self {
            Self::with_launch_items(Vec::new())
        }

        fn with_launch_items(items: Vec<AttentionItem>) -> Self {
            let wall = Utc::now();
            let t0 = Instant::now();
            let mut machine = Speedster::new(wall);
            machine.observe(obs(0, 0, &snapshot(items)), t0);
            Self { machine, t0, wall }
        }

        /// Seconds after the baseline settled.
        fn at(&self, secs: f32) -> Instant {
            self.t0 + SETTLE + Duration::from_secs_f32(secs)
        }

        fn fresh(&self) -> DateTime<Utc> {
            self.wall + TimeDelta::seconds(1)
        }

        fn observe(&mut self, working: usize, items: Vec<AttentionItem>, secs: f32) -> Mood {
            let now = self.at(secs);
            self.machine.observe(obs(working, 0, &snapshot(items)), now);
            self.machine.mood(now)
        }
    }

    fn obs(
        working: usize,
        awaiting_input: usize,
        attention: &AttentionSnapshot,
    ) -> Observation<'_> {
        Observation {
            working,
            awaiting_input,
            attention,
        }
    }

    #[test]
    fn working_count_picks_idle_jog_or_sprint() {
        let mut rig = Rig::new();
        assert_eq!(rig.observe(0, vec![], 0.0), Mood::Idle);
        assert_eq!(rig.observe(1, vec![], 1.0), Mood::Jog);
        assert_eq!(rig.observe(3, vec![], 2.0), Mood::Jog);
        assert_eq!(rig.observe(4, vec![], 3.0), Mood::Sprint);
        assert_eq!(rig.observe(9, vec![], 4.0), Mood::Sprint);
        assert_eq!(rig.observe(0, vec![], 5.0), Mood::Idle);
    }

    #[test]
    fn session_activity_counts_every_device_and_skips_stale_rows() {
        let now = Utc::now();
        let mut stale = session("c", SessionStatus::Working, now);
        stale.updated_at = now - TimeDelta::milliseconds(zeron_proto::view::SESSION_STALE_MS + 1);
        stale.chat_id = "stale".into();
        let sessions = vec![
            session("a", SessionStatus::Working, now),
            session("b", SessionStatus::Working, now),
            session("c", SessionStatus::AwaitingInput, now),
            session("d", SessionStatus::Idle, now),
            session("d", SessionStatus::Errored, now),
            stale,
        ];
        assert_eq!(session_activity(&sessions, now), (2, 1));
    }

    #[test]
    fn priority_runs_alert_oops_celebrate_tired_sprint() {
        let mut rig = Rig::new();
        let fresh = rig.fresh();
        let everything = vec![
            item("q", AttentionKind::Question, fresh),
            item("chat:f", AttentionKind::Failed, fresh),
            item("chat:d", AttentionKind::Finished, fresh),
            item("chat:l", AttentionKind::Limited, fresh),
        ];
        assert_eq!(rig.observe(5, everything.clone(), 0.0), Mood::Alert);
        let without_question = everything[1..].to_vec();
        assert_eq!(rig.observe(5, without_question.clone(), 0.1), Mood::Oops);
        // Both transients started together; oops outranks celebrate until
        // both end, then tired outranks the sprint.
        let after = rig.at(0.0) + TRANSIENT;
        assert_eq!(rig.machine.mood(after), Mood::Tired);
        let no_limit = vec![without_question[0].clone(), without_question[1].clone()];
        rig.machine
            .observe(obs(5, 0, &snapshot(no_limit.clone())), after);
        assert_eq!(rig.machine.mood(after), Mood::Sprint);
        rig.machine.observe(obs(2, 0, &snapshot(no_limit)), after);
        assert_eq!(rig.machine.mood(after), Mood::Jog);
    }

    #[test]
    fn celebrate_outranks_tired_but_not_oops() {
        let mut rig = Rig::new();
        let fresh = rig.fresh();
        let limited = item("chat:l", AttentionKind::Limited, fresh);
        assert_eq!(rig.observe(0, vec![limited.clone()], 0.0), Mood::Tired);
        let done = item("chat:d", AttentionKind::Finished, fresh);
        assert_eq!(
            rig.observe(0, vec![limited.clone(), done.clone()], 1.0),
            Mood::Celebrate
        );
        let failed = item("chat:f", AttentionKind::Failed, fresh);
        assert_eq!(rig.observe(0, vec![limited, done, failed], 1.5), Mood::Oops);
    }

    #[test]
    fn awaiting_sessions_on_any_device_alert() {
        let mut rig = Rig::new();
        let now = rig.at(0.0);
        rig.machine.observe(obs(2, 1, &snapshot(vec![])), now);
        assert_eq!(rig.machine.mood(now), Mood::Alert);
        rig.machine.observe(obs(2, 0, &snapshot(vec![])), now);
        assert_eq!(rig.machine.mood(now), Mood::Jog);
    }

    #[test]
    fn approvals_alert_and_timed_out_questions_do_not() {
        let mut rig = Rig::new();
        let fresh = rig.fresh();
        let approval = item("approval:c:r", AttentionKind::Approval, fresh);
        assert_eq!(rig.observe(0, vec![approval], 0.0), Mood::Alert);
        let timed_out = item("chat:t", AttentionKind::TimedOut, fresh);
        assert_eq!(rig.observe(0, vec![timed_out], 1.0), Mood::Idle);
    }

    #[test]
    fn celebrate_lasts_two_seconds_then_falls_back() {
        let mut rig = Rig::new();
        let done = item("chat:d", AttentionKind::Finished, rig.fresh());
        assert_eq!(rig.observe(1, vec![done.clone()], 0.0), Mood::Celebrate);
        assert_eq!(rig.machine.mood(rig.at(1.9)), Mood::Celebrate);
        assert_eq!(
            rig.machine.next_deadline(rig.at(1.0)),
            Some(rig.at(0.0) + TRANSIENT)
        );
        assert_eq!(rig.machine.mood(rig.at(2.0)), Mood::Jog);
        // The same item staying queued is not news again.
        assert_eq!(rig.observe(1, vec![done], 3.0), Mood::Jog);
    }

    #[test]
    fn oops_lasts_two_seconds_and_is_still() {
        let mut rig = Rig::new();
        let failed = item("run:r", AttentionKind::Failed, rig.fresh());
        assert_eq!(rig.observe(0, vec![failed], 0.0), Mood::Oops);
        assert_eq!(Mood::Oops.frame_interval(), None);
        assert_eq!(rig.machine.mood(rig.at(2.5)), Mood::Idle);
    }

    #[test]
    fn no_celebrate_for_items_queued_at_launch() {
        let wall = Utc::now();
        let old = wall - TimeDelta::minutes(5);
        let mut rig = Rig::with_launch_items(vec![
            item("chat:d", AttentionKind::Finished, old),
            item("chat:f", AttentionKind::Failed, old),
        ]);
        assert_eq!(rig.machine.mood(rig.t0), Mood::Idle);
        let still_queued = vec![
            item("chat:d", AttentionKind::Finished, old),
            item("chat:f", AttentionKind::Failed, old),
        ];
        assert_eq!(rig.observe(0, still_queued, 0.0), Mood::Idle);
    }

    #[test]
    fn items_arriving_while_the_engine_catches_up_are_not_news() {
        let wall = Utc::now();
        let t0 = Instant::now();
        let mut machine = Speedster::new(wall);
        // No engine yet, then its empty initial frame, then the real queue.
        machine.observe(obs(0, 0, &AttentionSnapshot::default()), t0);
        machine.observe(obs(0, 0, &snapshot(vec![])), t0);
        let fresh = wall + TimeDelta::seconds(1);
        let late = t0 + Duration::from_secs(1);
        machine.observe(
            obs(
                0,
                0,
                &snapshot(vec![item("chat:d", AttentionKind::Finished, fresh)]),
            ),
            late,
        );
        assert_eq!(machine.mood(late), Mood::Idle);
        // Once settled, a new finish celebrates.
        let settled = t0 + SETTLE + Duration::from_secs(1);
        machine.observe(
            obs(
                0,
                0,
                &snapshot(vec![
                    item("chat:d", AttentionKind::Finished, fresh),
                    item("chat:e", AttentionKind::Finished, fresh),
                ]),
            ),
            settled,
        );
        assert_eq!(machine.mood(settled), Mood::Celebrate);
    }

    #[test]
    fn items_older_than_the_speedster_are_never_news() {
        let mut rig = Rig::new();
        let before_launch = rig.wall - TimeDelta::seconds(30);
        let done = item("chat:d", AttentionKind::Finished, before_launch);
        assert_eq!(rig.observe(0, vec![done], 0.0), Mood::Idle);
    }

    #[test]
    fn switching_engines_rebaselines() {
        let mut rig = Rig::new();
        let fresh = rig.fresh();
        let now = rig.at(0.0);
        let other = AttentionSnapshot {
            device_id: "other".into(),
            items: vec![item("chat:d", AttentionKind::Finished, fresh)],
        };
        rig.machine.observe(obs(0, 0, &other), now);
        assert_eq!(rig.machine.mood(now), Mood::Idle);
    }

    #[test]
    fn a_chat_finishing_again_after_being_seen_celebrates_again() {
        let mut rig = Rig::new();
        let done = item("chat:d", AttentionKind::Finished, rig.fresh());
        assert_eq!(rig.observe(0, vec![done.clone()], 0.0), Mood::Celebrate);
        // Opened (dropped from the queue), then finished again.
        assert_eq!(rig.observe(0, vec![], 3.0), Mood::Idle);
        assert_eq!(rig.observe(0, vec![done], 4.0), Mood::Celebrate);
    }

    #[test]
    fn a_finished_chat_failing_next_turn_is_news() {
        let mut rig = Rig::new();
        let fresh = rig.fresh();
        assert_eq!(
            rig.observe(0, vec![item("chat:c", AttentionKind::Finished, fresh)], 0.0),
            Mood::Celebrate
        );
        assert_eq!(
            rig.observe(0, vec![item("chat:c", AttentionKind::Failed, fresh)], 3.0),
            Mood::Oops
        );
    }

    #[test]
    fn naps_after_a_quiet_spell() {
        let mut rig = Rig::new();
        assert_eq!(rig.observe(0, vec![], 0.0), Mood::Idle);
        let nap = rig.t0 + SLEEP_AFTER;
        assert_eq!(rig.machine.next_deadline(rig.at(0.0)), Some(nap));
        assert_eq!(rig.machine.mood(nap - Duration::from_millis(1)), Mood::Idle);
        assert_eq!(rig.machine.mood(nap), Mood::Sleeping);
        assert_eq!(rig.machine.next_deadline(nap), None);
        // Work wakes it, and the quiet clock restarts when work ends.
        let woke = nap + Duration::from_secs(1);
        rig.machine.observe(obs(1, 0, &snapshot(vec![])), woke);
        assert_eq!(rig.machine.mood(woke), Mood::Jog);
        rig.machine.observe(obs(0, 0, &snapshot(vec![])), woke);
        assert_eq!(rig.machine.mood(woke), Mood::Idle);
        assert_eq!(rig.machine.mood(woke + SLEEP_AFTER), Mood::Sleeping);
    }

    #[test]
    fn waiting_or_limited_never_naps() {
        let mut rig = Rig::new();
        let limited = item("chat:l", AttentionKind::Limited, rig.fresh());
        assert_eq!(rig.observe(0, vec![limited], 0.0), Mood::Tired);
        assert_eq!(rig.machine.mood(rig.at(3600.0)), Mood::Tired);
        assert_eq!(rig.machine.next_deadline(rig.at(0.0)), None);
    }

    #[test]
    fn a_celebration_pushes_the_nap_back() {
        let mut rig = Rig::new();
        let done = item("chat:d", AttentionKind::Finished, rig.fresh());
        let at = rig.at(60.0);
        rig.machine.observe(obs(0, 0, &snapshot(vec![done])), at);
        assert_eq!(rig.machine.mood(at), Mood::Celebrate);
        let nap = at + TRANSIENT + SLEEP_AFTER;
        assert_eq!(rig.machine.mood(nap - Duration::from_millis(1)), Mood::Idle);
        assert_eq!(rig.machine.mood(nap), Mood::Sleeping);
    }

    #[test]
    fn frame_rates_stay_under_the_cap() {
        for mood in [
            Mood::Sleeping,
            Mood::Idle,
            Mood::Jog,
            Mood::Sprint,
            Mood::Tired,
            Mood::Celebrate,
            Mood::Oops,
            Mood::Alert,
        ] {
            assert!(!mood.frames().is_empty());
            if let Some(interval) = mood.frame_interval() {
                assert!(interval >= Duration::from_secs_f32(1.0 / MAX_FPS));
            }
        }
        assert_eq!(Mood::Jog.frame_interval(), Some(Duration::from_millis(125)));
        assert_eq!(Mood::Idle.frame_interval(), Some(Duration::from_secs(2)));
        assert_eq!(Mood::Tired.frame_interval(), None);
    }

    const VISIBLE: Conditions = Conditions {
        reduced_motion: false,
        hidden: false,
        power_saving: false,
    };

    #[test]
    fn a_quiet_speedster_jogs_in_place_briefly_then_holds_still_with_no_timer() {
        let mut rig = Rig::new();
        rig.observe(1, vec![], 0.0);
        assert_eq!(
            rig.machine.plan(rig.at(0.0), VISIBLE),
            Plan::Animate(Duration::from_millis(125)),
            "working moods animate while agents run"
        );
        // Work ends: idle animates for the hold.
        rig.observe(0, vec![], 10.0);
        assert_eq!(
            rig.machine.plan(rig.at(10.0), VISIBLE),
            Plan::Animate(Duration::from_secs(2))
        );
        assert!(SLEEP_AFTER >= Duration::from_secs(30) && SLEEP_AFTER <= Duration::from_secs(60));
        // Then one still sleeping frame and nothing scheduled, for good.
        let asleep = rig.at(10.0) + SLEEP_AFTER;
        assert_eq!(rig.machine.mood(asleep), Mood::Sleeping);
        assert_eq!(Mood::Sleeping.frames()[0], Frame::Sleeping1);
        assert_eq!(rig.machine.plan(asleep, VISIBLE), Plan::None);
        assert_eq!(
            rig.machine
                .plan(asleep + Duration::from_secs(86_400), VISIBLE),
            Plan::None
        );
    }

    #[test]
    fn a_waiting_alert_waves_then_settles() {
        let mut rig = Rig::new();
        let ask = item("chat:q", AttentionKind::Question, rig.fresh());
        assert_eq!(rig.observe(0, vec![ask.clone()], 0.0), Mood::Alert);
        assert!(matches!(
            rig.machine.plan(rig.at(0.0), VISIBLE),
            Plan::Animate(_)
        ));
        // Still waiting later (new snapshots do not restart the wave).
        rig.observe(0, vec![ask.clone()], 30.0);
        let settled = rig.at(0.0) + ALERT_HOLD;
        assert_eq!(rig.machine.mood(settled), Mood::Alert);
        assert_eq!(rig.machine.plan(settled, VISIBLE), Plan::None);
        // Answered, then asked again: waves again.
        rig.observe(0, vec![], 70.0);
        rig.observe(0, vec![ask], 80.0);
        assert!(matches!(
            rig.machine.plan(rig.at(80.0), VISIBLE),
            Plan::Animate(_)
        ));
    }

    #[test]
    fn battery_and_low_power_cap_working_moods_at_four_fps() {
        let mut rig = Rig::new();
        let saving = Conditions {
            power_saving: true,
            ..VISIBLE
        };
        let cap = Duration::from_secs_f32(1.0 / POWER_SAVING_MAX_FPS);
        rig.observe(1, vec![], 0.0);
        assert_eq!(rig.machine.plan(rig.at(0.0), saving), Plan::Animate(cap));
        rig.observe(SPRINT_FROM, vec![], 1.0);
        assert_eq!(rig.machine.mood(rig.at(1.0)), Mood::Sprint);
        assert_eq!(rig.machine.plan(rig.at(1.0), saving), Plan::Animate(cap));
        // Slower moods keep their own pace.
        rig.observe(0, vec![], 2.0);
        assert_eq!(
            rig.machine.plan(rig.at(2.0), saving),
            Plan::Animate(Duration::from_secs(2))
        );
    }

    /// Every window hidden while agents run is the common case (the user is
    /// in another app), and the Dock is then the only sign of work: working
    /// moods keep running, at most four frames a second.
    #[test]
    fn hidden_window_plus_running_agents_animates_at_most_four_fps() {
        let mut rig = Rig::new();
        let hidden = Conditions {
            hidden: true,
            ..VISIBLE
        };
        let cap = Duration::from_secs_f32(1.0 / POWER_SAVING_MAX_FPS);
        let at_most_four_fps = |plan: Plan| matches!(plan, Plan::Animate(i) if i >= cap);
        rig.observe(1, vec![], 0.0);
        assert_eq!(rig.machine.mood(rig.at(0.0)), Mood::Jog);
        let plan = rig.machine.plan(rig.at(0.0), hidden);
        assert!(at_most_four_fps(plan), "{plan:?}");
        assert_eq!(plan, Plan::Animate(cap));
        rig.observe(SPRINT_FROM, vec![], 1.0);
        assert_eq!(rig.machine.mood(rig.at(1.0)), Mood::Sprint);
        assert!(at_most_four_fps(rig.machine.plan(rig.at(1.0), hidden)));
        // Hidden and on battery: still capped, never faster.
        let both = Conditions {
            power_saving: true,
            ..hidden
        };
        assert_eq!(rig.machine.plan(rig.at(1.0), both), Plan::Animate(cap));
    }

    #[test]
    fn hidden_windows_hold_the_quiet_pose_still_until_the_nap() {
        let mut rig = Rig::new();
        let hidden = Conditions {
            hidden: true,
            ..VISIBLE
        };
        rig.observe(0, vec![], 1.0);
        assert_eq!(rig.machine.mood(rig.at(1.0)), Mood::Idle);
        // No frame timer: one wake for the nap (quiet since the rig's first
        // observation), then nothing.
        let nap = rig.t0 + SLEEP_AFTER;
        assert_eq!(rig.machine.plan(rig.at(1.0), hidden), Plan::Wake(nap));
        assert_eq!(rig.machine.plan(nap, hidden), Plan::None);
        // A waiting alert still waves while hidden, then settles.
        let ask = item("chat:q", AttentionKind::Question, rig.fresh());
        rig.observe(0, vec![ask], 100.0);
        assert!(matches!(
            rig.machine.plan(rig.at(100.0), hidden),
            Plan::Animate(_)
        ));
        assert_eq!(
            rig.machine.plan(rig.at(100.0) + ALERT_HOLD, hidden),
            Plan::None
        );
    }

    #[test]
    fn reduced_motion_follows_mood_changes_only() {
        let mut rig = Rig::new();
        let still = Conditions {
            reduced_motion: true,
            ..VISIBLE
        };
        rig.observe(SPRINT_FROM, vec![], 0.0);
        assert_eq!(
            rig.machine.plan(rig.at(0.0), still),
            Plan::None,
            "no frame timer"
        );
        // A pending mood change still gets its one wake-up.
        rig.observe(0, vec![], 1.0);
        assert_eq!(
            rig.machine.plan(rig.at(1.0), still),
            Plan::Wake(rig.at(1.0) + SLEEP_AFTER)
        );
    }

    #[test]
    fn frame_indices_are_dense() {
        assert_eq!(Frame::Oops.index(), Frame::COUNT - 1);
        assert_eq!(Frame::Idle1.index(), 0);
    }
}
