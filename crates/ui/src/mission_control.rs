//! Mission Control: every agent at once, full window. A board of cards — one
//! per live or recently active chat on any device, grouped by device then
//! project — under a strip of live totals and the speedster's mood.
//!
//! Zero model tokens: the board reads the session rows, chats and "Needs you"
//! queue `AppState` already holds ([`board`]), and the only call it makes is
//! the inbox's own low-risk approval. Keyboard-first ([`navigation`]).
//!
//! Motion is painted, not laid out: each card's plate is a canvas that draws
//! its halo and the working card's travelling current, and only asks the
//! shared pulse clock for frames while it is on screen and something on it
//! moves. Reduce Motion paints the resting frame and asks for none.

mod board;
mod navigation;

use std::cell::RefCell;
use std::collections::HashMap;
use std::f32::consts::TAU;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, Bounds, ContentMask, Context, Corners, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, Hsla, KeyDownEvent, Pixels, SharedString, Subscription, Task, Window,
    canvas, div, img, linear_color_stop, linear_gradient, point, prelude::*, px, size,
};
use serde_json::json;
use zeron_proto::attention::BatchApproval;
use zeron_rpc::methods;

use crate::changes::Changes;
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::motion::{self, MotionSpec};
use crate::popover;
use crate::settings::widgets;
use crate::speedster::{Frame, Mood, Observation, Speedster};
use crate::state::AppState;
use crate::theme::Theme;

use board::{Board, Card, CardStatus, DeviceGroup, Totals};
use navigation::{Command, command_for, filter_command_for, reconcile, step};

/// Key context of the filter field.
const FILTER_CONTEXT: &str = "MissionControlFilter";

const CARD_HEIGHT: f32 = 138.0;
const CARD_MIN_WIDTH: f32 = 272.0;
const CARD_GAP: f32 = 12.0;
const CARD_RADIUS: f32 = 14.0;
const MAX_COLUMNS: usize = 4;
const PAGE_MAX_WIDTH: f32 = 1240.0;
const PAGE_PAD_X: f32 = 40.0;
const SPEEDSTER_SIZE: f32 = 76.0;
/// Rebuild at least this often while painting, so elapsed labels tick.
const REBUILD_EVERY: Duration = Duration::from_secs(1);
/// One lap of a working card's current along its rim (the curve is unused:
/// the pulse clock runs linear phase).
const CURRENT_LAP: MotionSpec = MotionSpec::new(2200, motion::EASE);
/// How long a card that just finished takes to settle.
const SETTLE: Duration = Duration::from_millis(1400);

pub enum MissionControlEvent {
    OpenChat(String),
    Close,
}

/// Same order as [`Frame`]; the 256 px exports (the Dock embeds the 512s).
static FRAME_PNGS: [&[u8]; Frame::COUNT] = [
    include_bytes!("../assets/speedster/idle_1@256.png"),
    include_bytes!("../assets/speedster/idle_2@256.png"),
    include_bytes!("../assets/speedster/sleeping_1@256.png"),
    include_bytes!("../assets/speedster/sleeping_2@256.png"),
    include_bytes!("../assets/speedster/jog_1@256.png"),
    include_bytes!("../assets/speedster/jog_2@256.png"),
    include_bytes!("../assets/speedster/jog_3@256.png"),
    include_bytes!("../assets/speedster/jog_4@256.png"),
    include_bytes!("../assets/speedster/sprint_1@256.png"),
    include_bytes!("../assets/speedster/sprint_2@256.png"),
    include_bytes!("../assets/speedster/sprint_3@256.png"),
    include_bytes!("../assets/speedster/sprint_4@256.png"),
    include_bytes!("../assets/speedster/alert_1@256.png"),
    include_bytes!("../assets/speedster/alert_2@256.png"),
    include_bytes!("../assets/speedster/celebrate_1@256.png"),
    include_bytes!("../assets/speedster/celebrate_2@256.png"),
    include_bytes!("../assets/speedster/tired@256.png"),
    include_bytes!("../assets/speedster/oops@256.png"),
];

/// One decoded-on-demand image per frame, shared by every open board (gpui's
/// image cache keys them by content).
fn frame_image(frame: Frame) -> Arc<gpui::Image> {
    static IMAGES: OnceLock<Vec<Arc<gpui::Image>>> = OnceLock::new();
    IMAGES.get_or_init(|| {
        FRAME_PNGS
            .iter()
            .map(|png| {
                Arc::new(gpui::Image::from_bytes(
                    gpui::ImageFormat::Png,
                    png.to_vec(),
                ))
            })
            .collect()
    })[frame.index()]
    .clone()
}

/// What keeps the speedster's frame clock awake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpeedsterPlan {
    None,
    Animate(Duration),
    Wake(Instant),
}

pub struct MissionControl {
    state: Entity<AppState>,
    /// The Changes panes open in the shell: the sidebar's source for +/-
    /// counts, read, never started.
    diffs: Vec<Entity<Changes>>,
    focus: FocusHandle,
    filter: Entity<ComposerInput>,
    board: Board,
    dirty: bool,
    built_at: Option<Instant>,
    focused: Option<String>,
    /// Where focus was, for landing on the next card once it leaves.
    focused_index: usize,
    hovered: Option<String>,
    /// Columns in the grid as last laid out (Up/Down need it).
    columns: usize,
    scroll: widgets::PageScroll,
    /// Each card's bounds as last painted, for scrolling focus into view.
    card_bounds: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    reveal_pending: bool,
    focus_pending: bool,
    /// Status per card at the last rebuild; a change to Finished starts a settle.
    statuses: HashMap<String, CardStatus>,
    settles: HashMap<String, Instant>,
    speedster: Speedster,
    mood: Mood,
    frame: usize,
    plan: SpeedsterPlan,
    speedster_task: Option<Task<()>>,
    task: Option<Task<()>>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<MissionControlEvent> for MissionControl {}

impl Focusable for MissionControl {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl MissionControl {
    pub fn new(
        state: Entity<AppState>,
        diffs: Vec<Entity<Changes>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| ComposerInput::with_context("Filter agents", FILTER_CONTEXT, cx));
        let mut subscriptions = vec![
            cx.observe(&state, |this: &mut Self, _, cx| {
                this.dirty = true;
                this.observe_speedster(cx);
                cx.notify();
            }),
            cx.subscribe(
                &filter,
                |this: &mut Self, _, event: &ComposerInputEvent, cx| {
                    if matches!(event, ComposerInputEvent::Edited) {
                        this.dirty = true;
                        this.reveal_pending = true;
                        cx.notify();
                    }
                },
            ),
            // Reduce Motion (preference or OS) stills the speedster.
            cx.observe_global::<crate::motion::MotionState>(|this: &mut Self, cx| {
                this.sync_speedster(cx);
                cx.notify();
            }),
        ];
        subscriptions.extend(diffs.iter().map(|changes| {
            cx.observe(changes, |this: &mut Self, _, cx| {
                this.dirty = true;
                cx.notify();
            })
        }));
        let mut this = Self {
            state,
            diffs,
            focus: cx.focus_handle(),
            filter,
            board: Board::default(),
            dirty: true,
            built_at: None,
            focused: None,
            focused_index: 0,
            hovered: None,
            columns: 1,
            scroll: widgets::PageScroll::default(),
            card_bounds: Rc::default(),
            reveal_pending: false,
            focus_pending: true,
            statuses: HashMap::new(),
            settles: HashMap::new(),
            speedster: Speedster::new(Utc::now()),
            mood: Mood::Idle,
            frame: 0,
            plan: SpeedsterPlan::None,
            speedster_task: None,
            task: None,
            error: None,
            _subscriptions: subscriptions,
        };
        this.observe_speedster(cx);
        this.rebuild(cx);
        this
    }

    // ----- board ---------------------------------------------------------

    fn query(&self, cx: &App) -> String {
        self.filter.read(cx).text().to_string()
    }

    /// The board under the current filter.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let now = Utc::now();
        let app: &App = cx;
        let state = self.state.read(app);
        let local = state.local_device_id.as_deref();
        let diffs = &self.diffs;
        let read = |chat: &zeron_proto::Chat| {
            diffs
                .iter()
                .find_map(|changes| changes.read(app).diff_stats_for(chat, local))
        };
        let full = board::assemble(state, now, local_midnight(now), &read);
        let query = self.query(app);
        let board = full.filtered(&query);

        // A card turning Finished while the board is up settles into place.
        let at = Instant::now();
        let mut statuses = HashMap::new();
        for card in full.cards() {
            if card.status == CardStatus::Finished
                && self
                    .statuses
                    .get(&card.chat_id)
                    .is_some_and(|before| *before != CardStatus::Finished)
            {
                self.settles.insert(card.chat_id.clone(), at);
            }
            statuses.insert(card.chat_id.clone(), card.status);
        }
        self.settles
            .retain(|id, since| statuses.contains_key(id) && since.elapsed() < SETTLE);
        self.card_bounds
            .borrow_mut()
            .retain(|id, _| statuses.contains_key(id));
        self.statuses = statuses;

        self.board = board;
        self.dirty = false;
        self.built_at = Some(at);
        self.reconcile_focus();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let stale = self.built_at.is_none_or(|at| at.elapsed() >= REBUILD_EVERY);
        if self.dirty || stale {
            self.rebuild(cx);
        }
    }

    fn reconcile_focus(&mut self) {
        let order = self.board.order();
        self.focused = reconcile(&order, self.focused.as_deref(), self.focused_index);
        self.remember_index(&order);
    }

    fn remember_index(&mut self, order: &[String]) {
        if let Some(at) = self
            .focused
            .as_ref()
            .and_then(|id| order.iter().position(|o| o == id))
        {
            self.focused_index = at;
        }
    }

    fn focused_card(&self) -> Option<&Card> {
        self.board.card(self.focused.as_deref()?)
    }

    /// The approval `a` would answer: the focused card's pending low-risk
    /// approval, never anything riskier.
    pub(crate) fn approval_target(&self) -> Option<String> {
        self.focused_card()?.approvable.clone()
    }

    // ----- keyboard ------------------------------------------------------

    pub(crate) fn run(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::Move(by) => {
                let rows = self.board.rows(self.columns);
                self.focused = step(&rows, self.focused.as_deref(), by);
                let order = self.board.order();
                self.remember_index(&order);
                self.reveal_pending = true;
                cx.notify();
            }
            Command::Open => {
                if let Some(id) = self.focused.clone() {
                    cx.emit(MissionControlEvent::OpenChat(id));
                }
            }
            Command::Approve => self.approve(cx),
            Command::Filter => {
                window.focus(&self.filter.focus_handle(cx), cx);
                cx.notify();
            }
            Command::Close => {
                let filtering = self.filter.focus_handle(cx).is_focused(window);
                if filtering {
                    // Esc in the field clears it first, then hands the
                    // keyboard back to the board.
                    if self.query(cx).is_empty() {
                        window.focus(&self.focus, cx);
                    } else {
                        self.filter
                            .update(cx, |input, cx| input.set_text(String::new(), cx));
                    }
                    cx.notify();
                } else {
                    cx.emit(MissionControlEvent::Close);
                }
            }
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let other = modifiers.platform || modifiers.control || modifiers.alt || modifiers.function;
        let filtering = self.filter.focus_handle(cx).is_focused(window);
        let command = if filtering {
            filter_command_for(keystroke.key.as_str(), other)
        } else {
            command_for(keystroke.key.as_str(), modifiers.shift, other)
        };
        if let Some(command) = command {
            self.run(command, window, cx);
            cx.stop_propagation();
        }
    }

    /// Answer the focused card's low-risk approval through the inbox's batch
    /// path (`ApproveLowRisk`), which re-checks the risk engine-side.
    fn approve(&mut self, cx: &mut Context<Self>) {
        let Some(item_id) = self.approval_target() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let mut params = json!({ "itemIds": [item_id] });
        let device = self.state.read(cx).attention.device_id.clone();
        if !device.is_empty() {
            params["targetDeviceId"] = json!(device);
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::APPROVE_LOW_RISK, params)
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(reply) => {
                        let batch: BatchApproval =
                            serde_json::from_value(reply).unwrap_or_default();
                        if batch.approved.is_empty() && !batch.skipped.is_empty() {
                            this.error = Some("That request changed; nothing was approved.".into());
                        }
                    }
                    Err(err) => this.error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // ----- speedster -----------------------------------------------------

    fn observe_speedster(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        self.speedster
            .observe(Observation::of(self.state.read(cx), Utc::now()), now);
        self.sync_speedster(cx);
    }

    /// Re-read the mood and keep exactly one timer for its next frame or its
    /// next transition (a transient ending, the nap starting).
    fn sync_speedster(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mood = self.speedster.mood(now);
        if mood != self.mood {
            self.mood = mood;
            self.frame = 0;
        }
        let still = crate::motion::reduced_outside_window(cx);
        if still {
            self.frame = 0;
        }
        let plan = match mood.frame_interval() {
            Some(interval) if !still => SpeedsterPlan::Animate(interval),
            _ => self
                .speedster
                .next_deadline(now)
                .map_or(SpeedsterPlan::None, SpeedsterPlan::Wake),
        };
        if plan == self.plan {
            return;
        }
        self.plan = plan;
        self.speedster_task = match plan {
            SpeedsterPlan::None => None,
            SpeedsterPlan::Animate(interval) => Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(interval).await;
                    let alive = this.update(cx, |this, cx| {
                        this.frame = (this.frame + 1) % this.mood.frames().len();
                        let before = this.plan;
                        this.sync_speedster(cx);
                        cx.notify();
                        this.plan == before
                    });
                    if !matches!(alive, Ok(true)) {
                        break;
                    }
                }
            })),
            SpeedsterPlan::Wake(at) => Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(at.saturating_duration_since(Instant::now()))
                    .await;
                this.update(cx, |this, cx| {
                    // This task is done: the same deadline may be armed again.
                    this.plan = SpeedsterPlan::None;
                    this.sync_speedster(cx);
                    cx.notify();
                })
                .ok();
            })),
        };
    }

    fn speedster_frame(&self) -> Frame {
        let frames = self.mood.frames();
        frames[self.frame % frames.len()]
    }

    // ----- scrolling -----------------------------------------------------

    /// Bring the focused card into view, from the bounds the last paint
    /// recorded (the grid itself does not move when focus does).
    fn reveal_focused(&mut self) {
        let Some(id) = self.focused.as_ref() else {
            return;
        };
        let Some(card) = self.card_bounds.borrow().get(id).copied() else {
            return;
        };
        let viewport = self.scroll.scroll.bounds();
        if viewport.size.height <= px(0.0) {
            return;
        }
        let margin = px(24.0);
        let mut offset = self.scroll.scroll.offset();
        if card.bottom() + margin > viewport.bottom() {
            offset.y -= card.bottom() + margin - viewport.bottom();
        } else if card.top() - margin < viewport.top() {
            offset.y += viewport.top() - (card.top() - margin);
        }
        offset.y = offset.y.min(px(0.0));
        self.scroll.scroll.set_offset(offset);
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }
}

/// Local midnight today, in UTC: where "finished today" starts.
fn local_midnight(now: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::{Local, TimeZone as _};
    let local = now.with_timezone(&Local).date_naive();
    local
        .and_hms_opt(0, 0, 0)
        .and_then(|midnight| Local.from_local_datetime(&midnight).earliest())
        .map_or(now, |midnight| midnight.with_timezone(&Utc))
}

/// How many cards fit across `width`.
fn columns_for(width: f32) -> usize {
    (((width + CARD_GAP) / (CARD_MIN_WIDTH + CARD_GAP)).floor() as usize).clamp(1, MAX_COLUMNS)
}

fn status_tone(status: CardStatus, theme: &Theme) -> Hsla {
    match status {
        CardStatus::Working => theme.busy,
        CardStatus::NeedsYou => theme.warning,
        CardStatus::Limited => theme.limited,
        CardStatus::Failed => theme.danger,
        CardStatus::Finished => theme.success,
    }
}

fn mood_tone(mood: Mood, theme: &Theme) -> Hsla {
    match mood {
        Mood::Sleeping | Mood::Idle => theme.text_muted,
        Mood::Jog | Mood::Sprint => theme.busy,
        Mood::Alert => theme.warning,
        Mood::Celebrate => theme.success,
        Mood::Oops => theme.danger,
        Mood::Tired => theme.limited,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The line under the title: what the speedster is up to.
fn mood_caption(mood: Mood, totals: &Totals) -> String {
    match mood {
        Mood::Sleeping => "All quiet. The speedster is napping.".into(),
        Mood::Idle => "Standing by.".into(),
        Mood::Jog => format!(
            "On the move with {}.",
            plural(totals.working, "agent", "agents")
        ),
        Mood::Sprint => format!(
            "Full sprint: {} at once.",
            plural(totals.working, "agent", "agents")
        ),
        Mood::Alert if totals.needs_you > 0 => {
            format!(
                "{} waiting on you.",
                plural(totals.needs_you, "agent is", "agents are")
            )
        }
        Mood::Alert => "Something needs you.".into(),
        Mood::Celebrate => "One just crossed the line.".into(),
        Mood::Oops => "Something tripped.".into(),
        Mood::Tired => "Out of juice: a usage limit hit.".into(),
    }
}

// ---------------------------------------------------------------------------
// Card plate (painted)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Plate {
    status: CardStatus,
    tone: Hsla,
    fill: Hsla,
    rim: Hsla,
    fresh: bool,
    focused: bool,
    focus_ring: Hsla,
    settle_from: Option<Instant>,
}

/// The card's glass, halo, focus ring and (working) current, painted in one
/// pass. Off screen it paints nothing and asks for no frames.
fn paint_plate(
    plate: &Plate,
    bounds: Bounds<Pixels>,
    view: EntityId,
    window: &mut Window,
    cx: &mut App,
) {
    if !window
        .content_mask()
        .bounds
        .intersects(&bounds.dilate(px(10.0)))
    {
        return;
    }
    let still = cx.reduce_motion();
    let radius = px(CARD_RADIUS);
    // Halo strength and rim emphasis, 0..1.
    let (halo, rim) = match plate.status {
        CardStatus::Working => (0.5, 0.5),
        CardStatus::NeedsYou if still => (0.6, 0.65),
        CardStatus::NeedsYou => {
            let phase = motion::pulse_delta_slow(&motion::GODSPEED_PULSE, view, cx);
            let wave = 0.5 - 0.5 * (phase * TAU).cos();
            (0.3 + 0.6 * wave, 0.35 + 0.5 * wave)
        }
        CardStatus::Failed => (if plate.fresh { 0.3 } else { 0.0 }, 0.45),
        CardStatus::Limited => (0.0, 0.4),
        CardStatus::Finished => {
            let t = plate
                .settle_from
                .map_or(1.0, |at| at.elapsed().as_secs_f32() / SETTLE.as_secs_f32());
            if t < 1.0 && !still {
                // Only the frames matter here; the settle runs on wall time.
                motion::pulse_delta_slow(&motion::GODSPEED_PULSE, view, cx);
                let left = 1.0 - motion::EASE_OUT.eval(t);
                (0.9 * left, 0.25 + 0.6 * left)
            } else {
                (0.0, if plate.fresh { 0.3 } else { 0.0 })
            }
        }
    };

    // Halo: concentric hairline rings fading outward, so the glow sits
    // around the glass without tinting what is on it.
    if halo > 0.0 {
        const RINGS: usize = 5;
        const STEP: f32 = 1.6;
        for ring in 1..=RINGS {
            let spread = px(ring as f32 * STEP);
            let falloff = (1.0 - ring as f32 / (RINGS as f32 + 1.0)).powi(2);
            window.paint_quad(gpui::quad(
                bounds.dilate(spread),
                Corners::all(radius + spread),
                gpui::transparent_black(),
                px(STEP),
                plate.tone.opacity(halo * 0.24 * falloff),
                gpui::BorderStyle::Solid,
            ));
        }
    }

    // Focus ring, a hair outside the rim.
    if plate.focused {
        window.paint_quad(gpui::quad(
            bounds.dilate(px(3.0)),
            Corners::all(radius + px(3.0)),
            gpui::transparent_black(),
            px(1.5),
            plate.focus_ring,
            gpui::BorderStyle::Solid,
        ));
    }

    // The glass: a wash, tinted toward the status while something is live.
    let rim_color = if rim > 0.0 {
        plate.tone.opacity(0.16 + 0.5 * rim)
    } else {
        plate.rim
    };
    window.paint_quad(gpui::quad(
        bounds,
        Corners::all(radius),
        plate.fill,
        px(1.0),
        rim_color,
        gpui::BorderStyle::Solid,
    ));
    if plate.status.live() {
        window.paint_quad(gpui::fill(bounds, plate.tone.opacity(0.04)).corner_radii(radius));
    }

    if plate.status == CardStatus::Working {
        if still {
            // At rest the current sits as a spark at the top centre.
            paint_current(bounds, 0.5, plate.tone, true, 0.7, window);
        } else {
            let phase = motion::pulse_delta(&CURRENT_LAP, view, cx);
            paint_current(bounds, phase, plate.tone, true, 1.0, window);
            paint_current(
                bounds,
                (phase + 0.5).fract(),
                plate.tone,
                false,
                0.5,
                window,
            );
        }
    }
}

/// A bright segment riding the rim: left to right along the top edge,
/// right to left along the bottom. It enters and leaves through the straight
/// part of the edge, clear of the rounded corners.
fn paint_current(
    bounds: Bounds<Pixels>,
    phase: f32,
    tone: Hsla,
    top: bool,
    strength: f32,
    window: &mut Window,
) {
    let inset = px(CARD_RADIUS * 0.75);
    let track_left = bounds.left() + inset;
    let track_right = bounds.right() - inset;
    let track = track_right - track_left;
    if track <= px(0.0) {
        return;
    }
    let segment = (track * 0.42).max(px(56.0));
    let progress = if top { phase } else { 1.0 - phase };
    let head = track_left - segment + (track + segment) * progress;
    let edge = if top {
        bounds.top() + px(0.5)
    } else {
        bounds.bottom() - px(0.5)
    };
    let clip = Bounds::from_corners(
        point(track_left, edge - px(4.0)),
        point(track_right, edge + px(4.0)),
    );
    let core = motion::mix(tone, gpui::white(), 0.35);
    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
        // A soft bloom, then the hot core.
        for (height, color, alpha) in [(px(6.0), tone, 0.22), (px(1.5), core, 0.95)] {
            let color = color.opacity(alpha * strength);
            let y = edge - height / 2.0;
            let half = segment / 2.0;
            let rising = Bounds::new(point(head, y), size(half, height));
            let falling = Bounds::new(point(head + half, y), size(half, height));
            window.paint_quad(
                gpui::fill(
                    rising,
                    linear_gradient(
                        90.0,
                        linear_color_stop(color.opacity(0.0), 0.0),
                        linear_color_stop(color, 1.0),
                    ),
                )
                .corner_radii(height / 2.0),
            );
            window.paint_quad(
                gpui::fill(
                    falling,
                    linear_gradient(
                        90.0,
                        linear_color_stop(color, 0.0),
                        linear_color_stop(color.opacity(0.0), 1.0),
                    ),
                )
                .corner_radii(height / 2.0),
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl MissionControl {
    fn render_speedster(&self, frame: Frame, size_px: f32, tone: Hsla) -> AnyElement {
        // A soft disc of the mood's colour behind the mascot.
        let glow = size_px * 0.86;
        div()
            .relative()
            .flex_none()
            .size(px(size_px))
            .child(
                div()
                    .absolute()
                    .top(px((size_px - glow) / 2.0))
                    .left(px((size_px - glow) / 2.0))
                    .size(px(glow))
                    .rounded_full()
                    .bg(tone.opacity(0.10))
                    .shadow(vec![gpui::BoxShadow {
                        color: tone.opacity(0.28),
                        offset: point(px(0.0), px(0.0)),
                        blur_radius: px(size_px * 0.35),
                        spread_radius: px(0.0),
                        inset: false,
                    }]),
            )
            .child(
                img(frame_image(frame))
                    .absolute()
                    .top_0()
                    .left_0()
                    .size(px(size_px)),
            )
            .into_any_element()
    }

    fn render_header(
        &self,
        totals: &Totals,
        empty: bool,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tone = mood_tone(self.mood, theme);
        let filtering = self.filter.focus_handle(cx).is_focused(window);
        let query_empty = self.query(cx).is_empty();
        let filter = div()
            .id("mission-control-filter")
            .w(px(240.0))
            .h(px(32.0))
            .px(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .rounded(px(9.0))
            .bg(theme.wash(if filtering { 0.08 } else { 0.05 }))
            .border_1()
            .border_color(if filtering {
                theme.accent.opacity(0.55)
            } else {
                theme.hairline(0.08)
            })
            .cursor_text()
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.filter.focus_handle(cx), cx);
                cx.notify();
            }))
            .child(
                icon(icons::MAGNIFER)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(crate::typography::ui_rems(13.0))
                    .child(self.filter.clone()),
            )
            .when(!filtering && query_empty, |el| {
                el.child(popover::kbd_hint(theme, "/"))
            });
        let close = div()
            .id("mission-control-close")
            .role(gpui::Role::Button)
            .aria_label("Close Mission Control")
            .h(px(32.0))
            .pl(px(10.0))
            .pr(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .rounded(px(9.0))
            .cursor_pointer()
            .hover(|el| el.bg(theme.wash(0.08)))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(MissionControlEvent::Close)))
            .child(popover::kbd_hint(theme, "esc"))
            .child(
                icon(icons::CLOSE)
                    .size(px(15.0))
                    .text_color(theme.text_muted),
            );
        let title = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(22.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from("Mission Control")),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted)
                    .truncate()
                    .child(SharedString::from(mood_caption(self.mood, totals))),
            );
        let tiles = [
            ("Working", totals.working, theme.busy),
            ("Waiting on you", totals.needs_you, theme.warning),
            ("Limited", totals.limited, theme.limited),
            ("Finished today", totals.finished_today, theme.success),
        ]
        .into_iter()
        .map(|(label, count, tone)| stat_tile(label, count, tone, theme));
        div()
            .flex()
            .flex_col()
            .gap(px(18.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(18.0))
                    .when(!empty, |el| {
                        el.child(self.render_speedster(
                            self.speedster_frame(),
                            SPEEDSTER_SIZE,
                            tone,
                        ))
                    })
                    .child(title)
                    .child(filter)
                    .child(close),
            )
            .child(div().flex().flex_row().gap(px(CARD_GAP)).children(tiles))
            .into_any_element()
    }

    fn render_device(
        &self,
        device: &DeviceGroup,
        card_width: f32,
        ix: &mut usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = device.card_count();
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                icon(if device.local {
                    icons::LAPTOP
                } else {
                    icons::MONITOR
                })
                .size(px(14.0))
                .text_color(theme.text_muted),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(11.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(SharedString::from(popover::tracked_upper(&device.name))),
            )
            .when(device.local, |el| {
                el.child(
                    div()
                        .px(px(6.0))
                        .py(px(1.0))
                        .rounded(px(5.0))
                        .bg(theme.wash(0.06))
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("This device")),
                )
            })
            .when(!device.online, |el| {
                el.child(
                    icon(icons::WIFI_OFF)
                        .size(px(13.0))
                        .text_color(theme.warning.opacity(0.8)),
                )
            })
            .child(div().flex_1().h(px(1.0)).bg(theme.hairline(0.07)))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child(SharedString::from(plural(count, "agent", "agents"))),
            );
        let mut projects = Vec::new();
        for project in &device.projects {
            let mut cards = Vec::new();
            for card in &project.cards {
                cards.push(self.render_card(card, *ix, card_width, theme, cx));
                *ix += 1;
            }
            projects.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(13.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from(project.label.clone())),
                            )
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .text_color(theme.text_muted.opacity(0.7))
                                    .child(SharedString::from(project.cards.len().to_string())),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(CARD_GAP))
                            .children(cards),
                    ),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(header)
            .children(projects)
            .into_any_element()
    }

    fn render_card(
        &self,
        card: &Card,
        ix: usize,
        width: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = self.focused.as_deref() == Some(card.chat_id.as_str());
        let hovered = self.hovered.as_deref() == Some(card.chat_id.as_str());
        let tone = status_tone(card.status, theme);
        let plate = Plate {
            status: card.status,
            tone,
            fill: theme.wash(if focused {
                0.075
            } else if hovered {
                0.06
            } else {
                0.035
            }),
            rim: theme.hairline(if hovered || focused { 0.14 } else { 0.08 }),
            fresh: card.fresh,
            focused,
            focus_ring: theme.accent.opacity(0.7),
            settle_from: self.settles.get(&card.chat_id).copied(),
        };
        let view = cx.entity_id();
        let sink = self.card_bounds.clone();
        let chat_id = card.chat_id.clone();
        let plate_layer = canvas(
            move |bounds, _, _| {
                sink.borrow_mut().insert(chat_id, bounds);
            },
            move |bounds, (), window, cx| paint_plate(&plate, bounds, view, window, cx),
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        let pill = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .px(px(8.0))
            .py(px(2.0))
            .rounded_full()
            .bg(tone.opacity(0.13))
            .text_size(crate::typography::ui_rems(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(tone)
            .child(if card.status == CardStatus::Finished {
                icon(icons::CHECK)
                    .size(px(10.0))
                    .text_color(tone)
                    .into_any_element()
            } else {
                div()
                    .size(px(6.0))
                    .rounded_full()
                    .bg(tone)
                    .into_any_element()
            })
            .child(SharedString::from(card.status_label.clone()));

        let heading = div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .when_some(
                card.harness.map(crate::pickers::harness_brand_icon),
                |el, (path, tint)| {
                    el.child(
                        icon(path)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(tint.unwrap_or(theme.text_muted)),
                    )
                },
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(13.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(card.title.clone())),
            );

        let activity = div()
            .w_full()
            .min_w_0()
            .line_clamp(2)
            .text_ellipsis()
            .text_size(crate::typography::ui_rems(12.5))
            .text_color(theme.text_muted)
            .child(SharedString::from(
                card.activity
                    .clone()
                    .unwrap_or_else(|| "No messages yet".into()),
            ));

        let mut meta: Vec<AnyElement> = Vec::new();
        if let Some(branch) = &card.branch {
            meta.push(
                div()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        icon(if card.worktree {
                            icons::WORKTREE
                        } else {
                            icons::GIT_BRANCH
                        })
                        .size(px(12.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(0.8)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(theme.font_mono.clone())
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(branch.clone())),
                    )
                    .into_any_element(),
            );
        }
        if let Some((additions, deletions)) = card.diff {
            meta.push(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .gap(px(5.0))
                    .font_family(theme.font_mono.clone())
                    .text_size(crate::typography::ui_rems(11.0))
                    .child(
                        div()
                            .text_color(theme.diff_add)
                            .child(SharedString::from(format!("+{additions}"))),
                    )
                    .child(
                        div()
                            .text_color(theme.diff_del)
                            .child(SharedString::from(format!("−{deletions}"))),
                    )
                    .into_any_element(),
            );
        }
        if card.subagents > 0 {
            meta.push(
                div()
                    .flex_none()
                    .child(crate::running_pill::running_pill(
                        format!("mission-control-subagents-{}", card.chat_id),
                        card.subagents,
                        theme,
                    ))
                    .into_any_element(),
            );
        }
        let hint = focused.then(|| {
            div()
                .flex_none()
                .flex()
                .flex_row()
                .gap(px(10.0))
                .when(card.approvable.is_some(), |el| {
                    el.child(popover::kbd_hint(theme, "A approve"))
                })
                .child(popover::kbd_hint(theme, "↵ open"))
        });
        let footer = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .children(meta)
            .child(div().flex_1())
            .children(hint);

        let id = card.chat_id.clone();
        let hover_id = card.chat_id.clone();
        div()
            .id(("mission-control-card", ix))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(format!(
                "{}, {}",
                card.title, card.status_label
            )))
            .relative()
            .flex_none()
            .w(px(width))
            .h(px(CARD_HEIGHT))
            .cursor_pointer()
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                let next = hovered.then(|| hover_id.clone());
                if *hovered || this.hovered.as_ref() == Some(&hover_id) {
                    this.hovered = next;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.focused = Some(id.clone());
                cx.emit(MissionControlEvent::OpenChat(id.clone()));
            }))
            .child(plate_layer)
            .child(
                div()
                    .relative()
                    .size_full()
                    .px(px(14.0))
                    .pt(px(12.0))
                    .pb(px(11.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(heading.child(pill))
                    .child(activity)
                    .child(div().flex_1())
                    .child(footer),
            )
            .into_any_element()
    }

    fn render_empty(&self, theme: &Theme, cx: &App) -> AnyElement {
        let query = self.query(cx);
        let (frame, title, body) = if query.trim().is_empty() {
            // The nap frames, breathing only while the speedster really naps.
            let frames = Mood::Sleeping.frames();
            let frame = if self.mood == Mood::Sleeping {
                frames[self.frame % frames.len()]
            } else {
                frames[0]
            };
            (
                frame,
                "All quiet".to_string(),
                "Nothing is running or finished in the last day. Start a chat and it lands here."
                    .to_string(),
            )
        } else {
            (
                Frame::Idle1,
                format!("Nothing matches \u{201c}{}\u{201d}", query.trim()),
                "Try a title, project, branch, device or status.".to_string(),
            )
        };
        div()
            .id("mission-control-empty")
            .py(px(72.0))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .child(self.render_speedster(frame, 132.0, theme.text_muted))
            .child(
                div()
                    .mt(px(6.0))
                    .text_size(crate::typography::ui_rems(16.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .max_w(px(360.0))
                    .text_center()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(body)),
            )
            .into_any_element()
    }
}

/// One live total in the top strip.
fn stat_tile(label: &'static str, count: usize, tone: Hsla, theme: &Theme) -> AnyElement {
    let lit = count > 0;
    div()
        .flex_1()
        .min_w_0()
        .h(px(70.0))
        .px(px(16.0))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(2.0))
        .rounded(px(12.0))
        .bg(if lit {
            tone.opacity(0.07)
        } else {
            theme.wash(0.035)
        })
        .border_1()
        .border_color(if lit {
            tone.opacity(0.22)
        } else {
            theme.hairline(0.07)
        })
        .child(
            div()
                .font_family(theme.font_mono.clone())
                .text_size(crate::typography::ui_rems(24.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(if lit {
                    tone
                } else {
                    theme.text_muted.opacity(0.6)
                })
                .child(SharedString::from(count.to_string())),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(div().size(px(6.0)).rounded_full().bg(if lit {
                    tone
                } else {
                    theme.text_muted.opacity(0.35)
                }))
                .child(
                    div()
                        .truncate()
                        .text_size(crate::typography::ui_rems(11.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(label)),
                ),
        )
        .into_any_element()
}

impl popover::ScrollRailHost for MissionControl {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

impl Render for MissionControl {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        self.refresh(cx);
        if std::mem::take(&mut self.focus_pending) {
            window.focus(&self.focus, cx);
        }
        if std::mem::take(&mut self.reveal_pending) {
            self.reveal_focused();
        }
        let theme = Theme::of(cx).for_popup();
        let viewport = f32::from(window.viewport_size().width);
        let content_width = (viewport - 2.0 * PAGE_PAD_X).clamp(CARD_MIN_WIDTH, PAGE_MAX_WIDTH);
        self.columns = columns_for(content_width);
        let card_width =
            (content_width - CARD_GAP * (self.columns as f32 - 1.0)) / self.columns as f32;

        let board = self.board.clone();
        let header = self.render_header(&board.totals, board.is_empty(), &theme, window, cx);
        let mut sections: Vec<AnyElement> = Vec::new();
        let mut ix = 0;
        for device in &board.devices {
            sections.push(self.render_device(device, card_width, &mut ix, &theme, cx));
        }
        if sections.is_empty() {
            sections.push(self.render_empty(&theme, cx));
        }
        let legend = div()
            .flex_none()
            .w(px(content_width))
            .py(px(12.0))
            .flex()
            .flex_row()
            .flex_wrap()
            .justify_center()
            .gap(px(16.0))
            .child(popover::key_hint_text(&theme, "← ↑ ↓ →", "Move"))
            .child(popover::key_hint_text(&theme, "↵", "Open"))
            .child(popover::key_hint_text(&theme, "A", "Approve low risk"))
            .child(popover::key_hint_text(&theme, "/", "Filter"))
            .child(popover::key_hint_text(&theme, "esc", "Close"));
        let scrollbar = popover::rail(self, "mission-control-scrollbar", &theme, cx);
        let board_scroller = crate::edge_fade::edge_faded(
            24.0,
            true,
            true,
            div()
                .id("mission-control-board")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll.scroll)
                .child(
                    div().w_full().flex().flex_col().items_center().child(
                        div()
                            .w(px(content_width))
                            .pt(px(8.0))
                            .pb(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(28.0))
                            .children(sections),
                    ),
                ),
        )
        .fade_overflow_y(&self.scroll.scroll);

        div()
            .id("mission-control")
            .key_context("MissionControl")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .text_color(theme.text)
            .child(
                div()
                    .flex_none()
                    .w(px(content_width))
                    .pt(px(Theme::TITLEBAR_HEIGHT + 14.0))
                    .pb(px(20.0))
                    .child(header),
            )
            .child(
                div()
                    .id("mission-control-board-host")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .on_hover(cx.listener(Self::on_scroll_hovered))
                    .child(board_scroller)
                    .children(scrollbar),
            )
            .child(legend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use gpui::TestAppContext;
    use zeron_proto::attention::{
        AttentionAction, AttentionItem, AttentionKind, AttentionSnapshot,
    };
    use zeron_proto::{ApprovalRisk, Session, SessionStatus};

    fn chat(id: &str) -> zeron_proto::Chat {
        let mut chat: zeron_proto::Chat = serde_json::from_value(json!({
            "id": id,
            "deviceId": "mac",
            "title": format!("Chat {id}"),
            "archived": false,
            "cwd": null,
            "branch": null,
            "checkoutId": null,
            "config": null,
            "lastMessagePreview": null,
            "lastMessageAt": null,
            "createdAt": Utc::now() - TimeDelta::hours(1),
        }))
        .unwrap();
        chat.last_message_at = Some(Utc::now() - TimeDelta::minutes(1));
        chat
    }

    fn asking(chat_id: &str) -> Session {
        Session {
            last_completed_turn: None,
            chat_id: chat_id.into(),
            device_id: "mac".into(),
            status: SessionStatus::AwaitingInput,
            started_at: None,
            updated_at: Utc::now(),
            running_subagents: 0,
            background_tasks: 0,
            awaiting_approval: true,
        }
    }

    fn approval(id: &str, chat_id: &str, risk: ApprovalRisk) -> AttentionItem {
        AttentionItem {
            id: id.into(),
            kind: AttentionKind::Approval,
            chat_id: Some(chat_id.into()),
            title: id.into(),
            space_id: None,
            project: None,
            harness: None,
            created_at: Utc::now(),
            risk: Some(risk),
            summary: String::new(),
            actions: vec![AttentionAction::Approve],
            request_id: None,
            approval: None,
            agent_run: None,
        }
    }

    #[test]
    fn columns_fit_the_width_up_to_four() {
        assert_eq!(columns_for(200.0), 1);
        assert_eq!(columns_for(CARD_MIN_WIDTH * 2.0 + CARD_GAP), 2);
        assert_eq!(columns_for(CARD_MIN_WIDTH * 2.0 + CARD_GAP - 1.0), 1);
        assert_eq!(columns_for(10_000.0), MAX_COLUMNS);
    }

    #[gpui::test]
    fn the_approve_key_targets_only_the_focused_low_risk_approval(cx: &mut TestAppContext) {
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, _| {
            state.local_device_id = Some("mac".into());
            state.chats = vec![chat("low"), chat("high")];
            state.sessions = vec![asking("low"), asking("high")];
            state.apply_attention(AttentionSnapshot {
                device_id: "mac".into(),
                items: vec![
                    approval("item-high", "high", ApprovalRisk::High),
                    approval("item-low", "low", ApprovalRisk::Low),
                ],
            });
        });
        let control = cx.new(|cx| MissionControl::new(state.clone(), Vec::new(), cx));
        control.update(cx, |control, _| {
            let order = control.board.order();
            assert_eq!(order.len(), 2);
            control.focused = Some("high".into());
            assert_eq!(
                control.approval_target(),
                None,
                "high risk is never batch-approved"
            );
            control.focused = Some("low".into());
            assert_eq!(control.approval_target().as_deref(), Some("item-low"));
            control.focused = None;
            assert_eq!(control.approval_target(), None);
        });
    }

    #[gpui::test]
    fn focus_survives_a_card_leaving_the_board(cx: &mut TestAppContext) {
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, _| {
            state.chats = vec![chat("a"), chat("b"), chat("c")];
            state.sessions = vec![asking("a"), asking("b"), asking("c")];
        });
        let control = cx.new(|cx| MissionControl::new(state.clone(), Vec::new(), cx));
        let order = control.read_with(cx, |control, _| control.board.order());
        let second = order[1].clone();
        let third = order[2].clone();
        control.update(cx, |control, _| {
            control.focused = Some(second.clone());
            control.focused_index = 1;
        });
        state.update(cx, |state, cx| {
            state.chats.retain(|chat| chat.id != second);
            cx.notify();
        });
        cx.run_until_parked();
        control.update(cx, |control, cx| {
            control.rebuild(cx);
            assert_eq!(control.focused.as_deref(), Some(third.as_str()));
        });
    }
}
