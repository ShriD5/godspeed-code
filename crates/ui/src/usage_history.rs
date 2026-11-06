//! Settings → Usage, history: tokens and estimated API cost over a range,
//! below the live limit meters ([`crate::usage_cockpit`]).
//!
//! The engine answers `GetUsageHistory` from its usage index (Godspeed's
//! own turns plus Claude Code and Codex logs, never counting a turn twice;
//! see `zeron_engine::usage_history`). This view picks the range, draws the
//! daily (hourly for Today) chart natively — stacked by provider or model,
//! tokens or cost, with a hover card per bar — and lists the totals and the
//! breakdowns by model, provider, project and chat.
//!
//! Costs are what the tokens would cost at list API prices, shown for
//! subscription logins too (T3 Code: "if billed at full API rate"); tokens
//! on a model the price table doesn't know are counted but not priced.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, Bounds, Context, Entity, EventEmitter, Hsla, MouseMoveEvent, Pixels, SharedString,
    Subscription, Task, Window, canvas, div, point, prelude::*, px, size,
};
use zeron_proto::{
    UsageAmount, UsageBucket, UsageBucketSize, UsageHistory, UsageHistoryRequest, UsageRange,
    UsageSlice, provider_label,
};
use zeron_rpc::methods;

use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// While the engine is still indexing, ask again this often...
const POLL_EVERY: Duration = Duration::from_secs(2);
/// ...this many times at most per load.
const MAX_POLLS: u32 = 90;
/// Models drawn as their own series; the rest stack as "Other".
const MODEL_SERIES: usize = 5;
/// Rows a breakdown lists before folding the rest into a count.
const BREAKDOWN_ROWS: usize = 8;
const CHART_HEIGHT: f32 = 168.0;
const Y_AXIS_WIDTH: f32 = 52.0;
const TOOLTIP_WIDTH: f32 = 220.0;
/// Providers in a fixed stacking order, so a provider keeps its color as
/// the range changes.
const PROVIDER_ORDER: [&str; 10] = [
    "claude-code",
    "codex",
    "cursor",
    "muse",
    "opencode",
    "antigravity",
    "grok",
    "devin",
    "pi",
    "hermes",
];

// ---------------------------------------------------------------------------
// Pure: chart model and formatting
// ---------------------------------------------------------------------------

/// What the bars measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartMetric {
    Tokens,
    Cost,
}

/// What the bars stack by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartStack {
    Provider,
    Model,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub key: String,
    pub label: String,
    /// The folded "Other" series (drawn in a neutral tone).
    pub other: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    /// Short axis label ("Oct 17", "3 PM").
    pub label: String,
    /// The hover card's heading ("Fri, Oct 17").
    pub title: String,
    /// One value per series, in series order.
    pub values: Vec<f64>,
    pub total: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChartModel {
    pub series: Vec<Series>,
    pub bars: Vec<Bar>,
    pub max: f64,
}

pub fn metric_of(amount: &UsageAmount, metric: ChartMetric) -> f64 {
    match metric {
        ChartMetric::Tokens => amount.tokens.total() as f64,
        ChartMetric::Cost => amount.cost_usd,
    }
}

/// Fold a history's buckets into stacked bars. Pure.
pub fn chart_model(history: &UsageHistory, metric: ChartMetric, stack: ChartStack) -> ChartModel {
    let key_of = |provider: &str, model: &str| match stack {
        ChartStack::Provider => provider.to_owned(),
        ChartStack::Model => model.to_owned(),
    };
    // Every key's total over the range.
    let mut totals: Vec<(String, f64)> = Vec::new();
    for point in history.buckets.iter().flat_map(|b| &b.points) {
        let key = key_of(&point.provider, &point.model);
        let value = metric_of(&point.amount, metric);
        match totals.iter_mut().find(|(k, _)| *k == key) {
            Some((_, total)) => *total += value,
            None => totals.push((key, value)),
        }
    }
    let mut series: Vec<Series> = match stack {
        ChartStack::Provider => {
            let rank = |key: &str| {
                PROVIDER_ORDER
                    .iter()
                    .position(|p| *p == key)
                    .unwrap_or(PROVIDER_ORDER.len())
            };
            totals.sort_by(|(a, _), (b, _)| rank(a).cmp(&rank(b)).then(a.cmp(b)));
            totals
                .iter()
                .map(|(key, _)| Series {
                    key: key.clone(),
                    label: provider_label(key),
                    other: false,
                })
                .collect()
        }
        ChartStack::Model => {
            totals.sort_by(|(ka, a), (kb, b)| b.total_cmp(a).then(ka.cmp(kb)));
            totals
                .iter()
                .take(MODEL_SERIES)
                .map(|(key, _)| Series {
                    key: key.clone(),
                    label: key.clone(),
                    other: false,
                })
                .collect()
        }
    };
    let folds = stack == ChartStack::Model && totals.len() > MODEL_SERIES;
    if folds {
        series.push(Series {
            key: String::new(),
            label: "Other".into(),
            other: true,
        });
    }
    let bars: Vec<Bar> = history
        .buckets
        .iter()
        .map(|bucket| {
            let mut values = vec![0.0; series.len()];
            for point in &bucket.points {
                let key = key_of(&point.provider, &point.model);
                let ix = series
                    .iter()
                    .position(|s| !s.other && s.key == key)
                    .or(folds.then(|| series.len() - 1));
                if let Some(ix) = ix {
                    values[ix] += metric_of(&point.amount, metric);
                }
            }
            Bar {
                label: bucket_label(bucket, history.bucket),
                title: bucket_title(bucket, history.bucket),
                total: values.iter().sum(),
                values,
            }
        })
        .collect();
    let max = bars.iter().map(|b| b.total).fold(0.0, f64::max);
    ChartModel { series, bars, max }
}

fn hour_label(hour: u8) -> String {
    match hour {
        0 => "12 AM".into(),
        12 => "12 PM".into(),
        h if h < 12 => format!("{h} AM"),
        h => format!("{} PM", h - 12),
    }
}

/// The short axis label of a bucket. Pure.
pub fn bucket_label(bucket: &UsageBucket, size: UsageBucketSize) -> String {
    match (size, bucket.hour) {
        (UsageBucketSize::Hour, Some(hour)) => hour_label(hour),
        _ => bucket.day.format("%b %-d").to_string(),
    }
}

/// The hover card's heading for a bucket. Pure.
pub fn bucket_title(bucket: &UsageBucket, size: UsageBucketSize) -> String {
    match (size, bucket.hour) {
        (UsageBucketSize::Hour, Some(hour)) => {
            format!("{} – {}", hour_label(hour), hour_label((hour + 1) % 24))
        }
        (UsageBucketSize::Week, _) => format!("Week of {}", bucket.day.format("%b %-d, %Y")),
        _ => bucket.day.format("%a, %b %-d").to_string(),
    }
}

/// `987`, `12.3K`, `4.5M`, `1.2B`. Pure.
pub fn format_tokens(n: u64) -> String {
    let scaled = |value: f64, unit: &str| {
        let text = if value < 10.0 {
            format!("{value:.1}")
        } else {
            format!("{value:.0}")
        };
        format!("{}{unit}", text.trim_end_matches(".0"))
    };
    match n {
        n if n < 1_000 => n.to_string(),
        n if n < 999_500 => scaled(n as f64 / 1e3, "K"),
        n if n < 999_500_000 => scaled(n as f64 / 1e6, "M"),
        n => scaled(n as f64 / 1e9, "B"),
    }
}

/// `$0`, `<$0.01`, `$12.34`, `$1,234`. Pure.
pub fn format_usd(usd: f64) -> String {
    if usd <= 0.0 {
        return "$0".into();
    }
    if usd < 0.005 {
        return "<$0.01".into();
    }
    if usd < 1_000.0 {
        return format!("${usd:.2}");
    }
    format!("${}", crate::receipt::format_count(usd.round() as u64))
}

/// A slice's cost: "no price" when none of its tokens could be priced.
/// Pure.
pub fn amount_cost(amount: &UsageAmount) -> String {
    if amount.wholly_unpriced() {
        "no price".into()
    } else {
        format_usd(amount.cost_usd)
    }
}

pub fn format_metric(value: f64, metric: ChartMetric) -> String {
    match metric {
        ChartMetric::Tokens => format_tokens(value.max(0.0).round() as u64),
        ChartMetric::Cost => format_usd(value),
    }
}

/// The axis top for `max`: the next 1, 2, 2.5 or 5 times a power of ten.
/// Pure.
pub fn nice_ceiling(max: f64) -> f64 {
    if max <= 0.0 || !max.is_finite() {
        return 1.0;
    }
    let magnitude = 10f64.powf(max.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .map(|step| step * magnitude)
        .find(|top| *top >= max * 0.999_999)
        .unwrap_or(10.0 * magnitude)
}

/// "N turns" / "N calls" / both, for a slice. Pure.
pub fn activity_label(amount: &UsageAmount) -> String {
    let plural = |n: u64, one: &str| {
        format!(
            "{} {one}{}",
            crate::receipt::format_count(n),
            if n == 1 { "" } else { "s" }
        )
    };
    match (amount.turns, amount.calls) {
        (0, 0) => String::new(),
        (t, 0) => plural(t, "turn"),
        (0, c) => plural(c, "call"),
        (t, c) => format!("{} · {}", plural(t, "turn"), plural(c, "call")),
    }
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

pub enum UsageHistoryEvent {
    OpenChat(String),
}

pub struct UsageHistoryView {
    state: Entity<AppState>,
    range: UsageRange,
    metric: ChartMetric,
    stack: ChartStack,
    data: Option<UsageHistory>,
    loading: bool,
    error: Option<SharedString>,
    load_task: Option<Task<()>>,
    /// The bar under the pointer.
    hover: Option<usize>,
    /// Where the plot painted last, for hit-testing the pointer.
    plot: Rc<Cell<Option<Bounds<Pixels>>>>,
    _state: Subscription,
}

impl EventEmitter<UsageHistoryEvent> for UsageHistoryView {}

impl UsageHistoryView {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // Settings can open before the engine connects: load once it does.
        let state_sub = cx.observe(&state, |view: &mut Self, _, cx| {
            if view.data.is_none() && view.load_task.is_none() {
                view.load(cx);
            }
        });
        let mut view = Self {
            state,
            range: UsageRange::Month,
            metric: ChartMetric::Cost,
            stack: ChartStack::Provider,
            data: None,
            loading: false,
            error: None,
            load_task: None,
            hover: None,
            plot: Rc::new(Cell::new(None)),
            _state: state_sub,
        };
        view.load(cx);
        view
    }

    /// Fetch the selected range, then keep asking while the engine is still
    /// indexing (its first backfill can take a while).
    pub fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let request = UsageHistoryRequest {
            range: self.range,
            time_zone: zeron_proto::local_time_zone(),
        };
        let Ok(params) = serde_json::to_value(&request) else {
            return;
        };
        self.loading = true;
        self.error = None;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let mut polls = 0;
            loop {
                let result = engine
                    .client()
                    .call_as::<UsageHistory>(methods::GET_USAGE_HISTORY, params.clone())
                    .await;
                let indexing = matches!(&result, Ok(h) if h.sources.indexing);
                let applied = this.update(cx, |view, cx| {
                    match result {
                        Ok(history) if history.range == view.range => {
                            view.data = Some(history);
                            view.error = None;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            view.error =
                                Some(format!("Couldn't read usage history: {error}").into());
                        }
                    }
                    view.loading = indexing;
                    cx.notify();
                });
                polls += 1;
                if applied.is_err() || !indexing || polls >= MAX_POLLS {
                    break;
                }
                cx.background_executor().timer(POLL_EVERY).await;
            }
            this.update(cx, |view, cx| {
                view.loading = false;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn set_range(&mut self, range: UsageRange, cx: &mut Context<Self>) {
        if self.range != range {
            self.range = range;
            self.hover = None;
            self.load(cx);
        }
    }

    fn on_plot_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let bars = self.data.as_ref().map_or(0, |d| d.buckets.len());
        let hover = self.plot.get().and_then(|plot| {
            let x = f32::from(event.position.x - plot.origin.x);
            let width = f32::from(plot.size.width);
            (bars > 0 && width > 0.0 && (0.0..width).contains(&x))
                .then(|| ((x / width * bars as f32) as usize).min(bars - 1))
        });
        if hover != self.hover {
            self.hover = hover;
            cx.notify();
        }
    }

    fn on_plot_hover(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if !*hovered && self.hover.take().is_some() {
            cx.notify();
        }
    }

    /// A row of pills, one selected.
    fn segmented<T: Copy + PartialEq + 'static>(
        &self,
        theme: &Theme,
        key: &str,
        options: &[(T, &'static str)],
        selected: T,
        on_pick: fn(&mut Self, T, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .flex_none()
            .p(px(2.0))
            .gap(px(2.0))
            .rounded(px(8.0))
            .bg(theme.wash(0.05))
            .children(options.iter().map(|(value, label)| {
                let value = *value;
                let on = value == selected;
                div()
                    .id(SharedString::from(format!("usage-history-{key}-{label}")))
                    .px(px(9.0))
                    .py(px(3.0))
                    .rounded(px(6.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .cursor_pointer()
                    .tab_index(0)
                    .role(gpui::Role::Button)
                    .focus_visible(|s| s.border_1().border_color(theme.accent))
                    .when(on, |el| {
                        el.bg(theme.wash(0.12))
                            .text_color(theme.text)
                            .font_weight(gpui::FontWeight::MEDIUM)
                    })
                    .when(!on, |el| {
                        el.text_color(theme.text_muted)
                            .hover(|s| s.text_color(theme.text))
                    })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        on_pick(view, value, cx);
                        cx.notify();
                    }))
                    .child(SharedString::from(*label))
            }))
            .into_any_element()
    }

    fn controls(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let ranges: Vec<(UsageRange, &'static str)> =
            UsageRange::ALL.iter().map(|r| (*r, r.label())).collect();
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .child(self.segmented(theme, "range", &ranges, self.range, Self::set_range, cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .child(self.segmented(
                        theme,
                        "metric",
                        &[(ChartMetric::Cost, "Cost"), (ChartMetric::Tokens, "Tokens")],
                        self.metric,
                        |view, metric, _| view.metric = metric,
                        cx,
                    ))
                    .child(self.segmented(
                        theme,
                        "stack",
                        &[
                            (ChartStack::Provider, "By provider"),
                            (ChartStack::Model, "By model"),
                        ],
                        self.stack,
                        |view, stack, _| view.stack = stack,
                        cx,
                    )),
            )
            .into_any_element()
    }

    /// One figure in the totals strip.
    fn stat(theme: &Theme, label: &'static str, value: String, note: Option<String>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .min_w(px(92.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted)
                    .child(label),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(15.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(value)),
            )
            .when_some(note, |el, note| {
                el.child(
                    div()
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(note)),
                )
            })
            .into_any_element()
    }

    fn totals(history: &UsageHistory, theme: &Theme) -> AnyElement {
        let total = &history.total;
        let t = &total.tokens;
        let cost_note = if total.unpriced_tokens > 0 {
            Some(format!(
                "+{} tokens unpriced",
                format_tokens(total.unpriced_tokens)
            ))
        } else if total.cache_savings_usd >= 0.01 {
            Some(format!(
                "cache saved {}",
                format_usd(total.cache_savings_usd)
            ))
        } else {
            None
        };
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_x(px(24.0))
            .gap_y(px(12.0))
            .child(Self::stat(
                theme,
                "Cost at API prices*",
                format_usd(total.cost_usd),
                cost_note,
            ))
            .child(Self::stat(
                theme,
                "Tokens",
                format_tokens(t.total()),
                Some(format!(
                    "{} chat{}",
                    history.sessions,
                    if history.sessions == 1 { "" } else { "s" }
                )),
            ))
            .child(Self::stat(theme, "Input", format_tokens(t.input), None))
            .child(Self::stat(theme, "Output", format_tokens(t.output), None))
            .child(Self::stat(
                theme,
                "Cache read",
                format_tokens(t.cache_read),
                None,
            ))
            .child(Self::stat(
                theme,
                "Cache write",
                format_tokens(t.cache_write),
                None,
            ))
            .child(Self::stat(
                theme,
                "Turns",
                crate::receipt::format_count(total.turns),
                (total.calls > 0)
                    .then(|| format!("+{} CLI calls", crate::receipt::format_count(total.calls))),
            ))
            .into_any_element()
    }

    fn series_colors(model: &ChartModel, theme: &Theme) -> Vec<Hsla> {
        let palette = crate::visuals::chart_series(theme);
        let mut next = 0;
        model
            .series
            .iter()
            .map(|series| {
                if series.other {
                    theme.text_faint
                } else {
                    let color = palette[next % palette.len()];
                    next += 1;
                    color
                }
            })
            .collect()
    }

    fn chart(
        &self,
        model: &ChartModel,
        colors: &[Hsla],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let top = nice_ceiling(model.max);
        let metric = self.metric;
        let hover = self.hover.filter(|ix| *ix < model.bars.len());
        let y_labels = div()
            .w(px(Y_AXIS_WIDTH))
            .flex_none()
            .h(px(CHART_HEIGHT))
            .flex()
            .flex_col()
            .justify_between()
            .items_end()
            .pr(px(8.0))
            .text_size(crate::typography::ui_rems(10.5))
            .text_color(theme.text_faint)
            .child(SharedString::from(format_metric(top, metric)))
            .child(SharedString::from(format_metric(top / 2.0, metric)))
            .child(SharedString::from(format_metric(0.0, metric)));

        let plot_cell = self.plot.clone();
        let bars: Vec<(Vec<f64>, f64)> = model
            .bars
            .iter()
            .map(|b| (b.values.clone(), b.total))
            .collect();
        let paint_colors = colors.to_vec();
        let grid = theme.border.opacity(0.5);
        let hover_wash = theme.wash(0.06);
        let plot = div()
            .id("usage-history-plot")
            .relative()
            .flex_1()
            .min_w_0()
            .h(px(CHART_HEIGHT))
            .on_mouse_move(cx.listener(Self::on_plot_move))
            .on_hover(cx.listener(Self::on_plot_hover))
            .child(
                canvas(
                    move |bounds, _, _| {
                        plot_cell.set(Some(bounds));
                        bounds
                    },
                    move |_, bounds, window, _| {
                        let (ox, oy) = (bounds.origin.x, bounds.origin.y);
                        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                        for frac in [0.0f32, 0.5, 1.0] {
                            let y = oy + px((h - 1.0) * frac);
                            window.paint_quad(gpui::fill(
                                Bounds::new(point(ox, y), size(bounds.size.width, px(1.0))),
                                grid,
                            ));
                        }
                        let n = bars.len().max(1);
                        let slot = w / n as f32;
                        let bar_w = (slot * 0.72).clamp(1.0, 28.0);
                        for (i, (values, total)) in bars.iter().enumerate() {
                            let x = ox + px(slot * i as f32 + (slot - bar_w) / 2.0);
                            if hover == Some(i) {
                                window.paint_quad(
                                    gpui::fill(
                                        Bounds::new(
                                            point(ox + px(slot * i as f32), oy),
                                            size(px(slot), bounds.size.height),
                                        ),
                                        hover_wash,
                                    )
                                    .corner_radii(px(4.0)),
                                );
                            }
                            if *total <= 0.0 {
                                continue;
                            }
                            let last = values.iter().rposition(|v| *v > 0.0);
                            let mut base = h;
                            for (s, value) in values.iter().enumerate() {
                                if *value <= 0.0 {
                                    continue;
                                }
                                let seg = (*value / top) as f32 * h;
                                let y = (base - seg).max(0.0);
                                let rect = Bounds::new(
                                    point(x, oy + px(y)),
                                    size(px(bar_w), px((base - y).max(0.5))),
                                );
                                let radius = if Some(s) == last {
                                    2.0f32.min(bar_w / 2.0)
                                } else {
                                    0.0
                                };
                                window.paint_quad(gpui::fill(rect, paint_colors[s]).corner_radii(
                                    gpui::Corners {
                                        top_left: px(radius),
                                        top_right: px(radius),
                                        bottom_left: px(0.0),
                                        bottom_right: px(0.0),
                                    },
                                ));
                                base = y;
                            }
                        }
                    },
                )
                .size_full(),
            )
            .when_some(hover, |el, ix| {
                el.child(self.tooltip(model, colors, ix, theme))
            });

        let n = model.bars.len();
        let x_labels = div()
            .ml(px(Y_AXIS_WIDTH))
            .mt(px(6.0))
            .flex()
            .flex_row()
            .justify_between()
            .text_size(crate::typography::ui_rems(10.5))
            .text_color(theme.text_faint)
            .children(
                [0, n / 2, n.saturating_sub(1)]
                    .into_iter()
                    .filter(|ix| *ix < n)
                    .fold(Vec::<usize>::new(), |mut acc, ix| {
                        if !acc.contains(&ix) {
                            acc.push(ix);
                        }
                        acc
                    })
                    .into_iter()
                    .map(|ix| SharedString::from(model.bars[ix].label.clone())),
            );
        let legend = div()
            .mt(px(10.0))
            .ml(px(Y_AXIS_WIDTH))
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_x(px(14.0))
            .gap_y(px(4.0))
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.text_muted)
            .children(model.series.iter().zip(colors).map(|(series, color)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().flex_none().size(px(8.0)).rounded(px(2.0)).bg(*color))
                    .child(SharedString::from(series.label.clone()))
            }));
        div()
            .flex()
            .flex_col()
            .child(div().flex().flex_row().child(y_labels).child(plot))
            .child(x_labels)
            .when(!model.series.is_empty(), |el| el.child(legend))
            .into_any_element()
    }

    /// The hover card for bar `ix`: its bucket, each series' value, total.
    fn tooltip(&self, model: &ChartModel, colors: &[Hsla], ix: usize, theme: &Theme) -> AnyElement {
        let bar = &model.bars[ix];
        let metric = self.metric;
        let width = self
            .plot
            .get()
            .map_or(TOOLTIP_WIDTH, |b| f32::from(b.size.width));
        let center = width * (ix as f32 + 0.5) / model.bars.len().max(1) as f32;
        // Beside the bar, flipping to its left past the middle.
        let left = if center < width / 2.0 {
            center + 12.0
        } else {
            center - 12.0 - TOOLTIP_WIDTH
        }
        .clamp(0.0, (width - TOOLTIP_WIDTH).max(0.0));
        let row = |swatch: Option<Hsla>, label: String, value: String, strong: bool| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(match swatch {
                    Some(color) => div().flex_none().size(px(7.0)).rounded(px(2.0)).bg(color),
                    None => div().flex_none().size(px(7.0)),
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(label)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(if strong { theme.text } else { theme.text_muted })
                        .when(strong, |el| el.font_weight(gpui::FontWeight::MEDIUM))
                        .child(SharedString::from(value)),
                )
        };
        let mut rows: Vec<AnyElement> = model
            .series
            .iter()
            .zip(colors)
            .zip(&bar.values)
            .filter(|(_, value)| **value > 0.0)
            .map(|((series, color), value)| {
                row(
                    Some(*color),
                    series.label.clone(),
                    format_metric(*value, metric),
                    false,
                )
                .into_any_element()
            })
            .collect();
        if rows.is_empty() {
            rows.push(
                div()
                    .text_color(theme.text_faint)
                    .child("No usage")
                    .into_any_element(),
            );
        } else if rows.len() > 1 {
            rows.push(
                row(None, "Total".into(), format_metric(bar.total, metric), true)
                    .into_any_element(),
            );
        }
        let card = div()
            .w(px(TOOLTIP_WIDTH))
            .px(px(10.0))
            .py(px(8.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(crate::popover::surface_bg(theme))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.text_muted)
            .child(
                div()
                    .mb(px(2.0))
                    .text_color(theme.text)
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(SharedString::from(bar.title.clone())),
            )
            .children(rows);
        div()
            .absolute()
            .top(px(4.0))
            .left(px(left))
            .child(crate::frost::frosted(8.0, crate::frost::MENU_BLUR, card))
            .into_any_element()
    }

    /// One breakdown as compact rows: label, share of the range, tokens,
    /// cost.
    fn slice_card(&self, slices: &[UsageSlice], total: &UsageAmount, theme: &Theme) -> AnyElement {
        let metric = self.metric;
        let whole = metric_of(total, metric).max(f64::MIN_POSITIVE);
        let mut card = widgets::section_card(theme).mt_0().py(px(4.0));
        for slice in slices.iter().take(BREAKDOWN_ROWS) {
            let share = (metric_of(&slice.amount, metric) / whole).clamp(0.0, 1.0) as f32;
            card = card.child(Self::row(
                theme,
                SharedString::from(slice.label.clone()),
                activity_label(&slice.amount),
                share,
                &slice.amount,
            ));
        }
        if slices.len() > BREAKDOWN_ROWS {
            card = card.child(
                div()
                    .px(px(16.0))
                    .py(px(6.0))
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!(
                        "and {} more",
                        slices.len() - BREAKDOWN_ROWS
                    ))),
            );
        }
        card.into_any_element()
    }

    fn row(
        theme: &Theme,
        label: SharedString,
        meta: String,
        share: f32,
        amount: &UsageAmount,
    ) -> gpui::Div {
        div()
            .mx(px(16.0))
            .min_h(px(36.0))
            .py(px(4.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .text_size(crate::typography::ui_rems(12.5))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().truncate().text_color(theme.text).child(label))
                    .when(!meta.is_empty(), |el| {
                        el.child(
                            div()
                                .truncate()
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(meta)),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(88.0))
                    .flex_none()
                    .h(px(4.0))
                    .rounded_full()
                    .overflow_hidden()
                    .bg(theme.wash(0.08))
                    .when(share > 0.0, |el| {
                        el.child(
                            div()
                                .h_full()
                                .w(gpui::relative(share.max(0.02)))
                                .rounded_full()
                                .bg(theme.accent.opacity(0.75)),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(64.0))
                    .flex_none()
                    .text_right()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format_tokens(amount.tokens.total()))),
            )
            .child(
                div()
                    .w(px(72.0))
                    .flex_none()
                    .text_right()
                    .text_color(if amount.wholly_unpriced() {
                        theme.text_faint
                    } else {
                        theme.text
                    })
                    .child(SharedString::from(amount_cost(amount))),
            )
    }

    fn chats_card(
        &self,
        history: &UsageHistory,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let metric = self.metric;
        let whole = metric_of(&history.total, metric).max(f64::MIN_POSITIVE);
        let mut card = widgets::section_card(theme).mt_0().py(px(4.0));
        for (ix, chat) in history.top_chats.iter().enumerate() {
            let mut meta = vec![provider_label(&chat.provider)];
            if let Some(project) = &chat.project {
                meta.push(project.clone());
            }
            meta.push(
                chat.last_at
                    .with_timezone(&chrono::Local)
                    .format("%b %-d")
                    .to_string(),
            );
            let activity = activity_label(&chat.amount);
            if !activity.is_empty() {
                meta.push(activity);
            }
            let share = (metric_of(&chat.amount, metric) / whole).clamp(0.0, 1.0) as f32;
            let row = Self::row(
                theme,
                SharedString::from(chat.title.clone()),
                meta.join(" · "),
                share,
                &chat.amount,
            );
            let row = div()
                .id(SharedString::from(format!("usage-history-chat-{ix}")))
                .rounded(px(8.0))
                .child(row);
            let row = match chat.chat_id.clone() {
                Some(id) => row
                    .cursor_pointer()
                    .tab_index(0)
                    .role(gpui::Role::Link)
                    .hover(|s| s.bg(theme.wash(0.04)))
                    .focus_visible(|s| s.border_1().border_color(theme.accent))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(UsageHistoryEvent::OpenChat(id.clone()));
                    })),
                None => row,
            };
            card = card.child(row);
        }
        card.into_any_element()
    }

    fn note(theme: &Theme, copy: String) -> AnyElement {
        div()
            .px(px(8.0))
            .text_size(crate::typography::ui_rems(11.5))
            .line_height(crate::typography::ui_rems(16.0))
            .text_color(theme.text_faint)
            .child(SharedString::from(copy))
            .into_any_element()
    }
}

impl Render for UsageHistoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let controls = self.controls(&theme, cx);
        let mut card = widgets::section_card(&theme)
            .mt_0()
            .p(px(16.0))
            .gap(px(18.0))
            .child(controls);
        let mut after: Vec<AnyElement> = Vec::new();
        match &self.data {
            None => {
                let copy = match (&self.error, self.loading) {
                    (Some(error), _) => error.to_string(),
                    (None, true) => "Reading usage…".into(),
                    (None, false) => "Usage history appears once the engine connects.".into(),
                };
                card = card.child(
                    div()
                        .text_size(crate::typography::ui_rems(12.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(copy)),
                );
            }
            Some(history) => {
                let history = history.clone();
                let model = chart_model(&history, self.metric, self.stack);
                let colors = Self::series_colors(&model, &theme);
                card = card.child(Self::totals(&history, &theme));
                if history.total.tokens.total() == 0 {
                    card = card.child(
                        div()
                            .text_size(crate::typography::ui_rems(12.5))
                            .text_color(theme.text_muted)
                            .child(if history.sources.indexing {
                                "Reading Claude Code and Codex logs for older usage…"
                            } else {
                                "No usage in this range."
                            }),
                    );
                } else {
                    card = card.child(self.chart(&model, &colors, &theme, cx));
                }
                let mut notes = vec![format!(
                    "* An estimate of what these tokens would cost billed per token at list API \
                     prices (checked {}), shown for subscription accounts too. Fast mode, \
                     long-context and batch pricing aren't modelled.",
                    history.prices_checked
                )];
                if history.total.unpriced_tokens > 0 {
                    notes.push(format!(
                        "{} tokens ran on models without a known price and are left out of the cost.",
                        format_tokens(history.total.unpriced_tokens)
                    ));
                }
                let s = &history.sources;
                notes.push(format!(
                    "From {} Godspeed turns, {} Claude Code and {} Codex log files on this device{}.",
                    crate::receipt::format_count(s.godspeed_turns),
                    crate::receipt::format_count(s.claude_files),
                    crate::receipt::format_count(s.codex_files),
                    if s.deduplicated > 0 {
                        format!(
                            "; {} log entries already counted as Godspeed turns were skipped",
                            crate::receipt::format_count(s.deduplicated)
                        )
                    } else {
                        String::new()
                    }
                ));
                if s.indexing {
                    notes.push("Still reading older logs; the numbers fill in as it goes.".into());
                }
                if history.total.tokens.total() > 0 {
                    after.push(
                        widgets::section(
                            &theme,
                            "By model",
                            self.slice_card(&history.by_model, &history.total, &theme),
                        )
                        .into_any_element(),
                    );
                    after.push(
                        widgets::section(
                            &theme,
                            "By provider",
                            self.slice_card(&history.by_provider, &history.total, &theme),
                        )
                        .into_any_element(),
                    );
                    after.push(
                        widgets::section(
                            &theme,
                            "By project",
                            self.slice_card(&history.by_project, &history.total, &theme),
                        )
                        .into_any_element(),
                    );
                    if !history.top_chats.is_empty() {
                        after.push(
                            widgets::section(
                                &theme,
                                "Top chats by cost",
                                self.chats_card(&history, &theme, cx),
                            )
                            .into_any_element(),
                        );
                    }
                }
                after.push(
                    div()
                        .mt(px(12.0))
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .children(notes.into_iter().map(|n| Self::note(&theme, n)))
                        .into_any_element(),
                );
            }
        }
        div()
            .flex()
            .flex_col()
            .child(widgets::section(&theme, "History", card))
            .children(after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use zeron_proto::{UsagePoint, UsageTokens};

    fn amount(tokens: u64, cost: f64) -> UsageAmount {
        UsageAmount {
            tokens: UsageTokens {
                input: tokens,
                ..Default::default()
            },
            cost_usd: cost,
            turns: 1,
            ..Default::default()
        }
    }

    fn point(provider: &str, model: &str, tokens: u64, cost: f64) -> UsagePoint {
        UsagePoint {
            provider: provider.into(),
            model: model.into(),
            amount: amount(tokens, cost),
        }
    }

    fn bucket(day: u32, points: Vec<UsagePoint>) -> UsageBucket {
        UsageBucket {
            start: Utc.with_ymd_and_hms(2026, 10, day, 0, 0, 0).unwrap(),
            day: NaiveDate::from_ymd_opt(2026, 10, day).unwrap(),
            hour: None,
            points,
        }
    }

    fn history() -> UsageHistory {
        UsageHistory {
            range: UsageRange::Week,
            bucket: UsageBucketSize::Day,
            buckets: vec![
                bucket(
                    16,
                    vec![
                        point("codex", "gpt-5-codex", 1_000, 1.0),
                        point("claude-code", "claude-opus-5-5", 3_000, 4.0),
                    ],
                ),
                bucket(17, vec![]),
                bucket(
                    18,
                    (0..7)
                        .map(|i| point("claude-code", &format!("m{i}"), 100 * (i + 1), 0.0))
                        .collect(),
                ),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn providers_stack_in_a_fixed_order_and_values_follow_the_metric() {
        let model = chart_model(&history(), ChartMetric::Cost, ChartStack::Provider);
        let keys: Vec<&str> = model.series.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["claude-code", "codex"], "Claude Code first, always");
        assert_eq!(model.series[0].label, "Claude Code");
        assert_eq!(model.bars.len(), 3);
        assert_eq!(model.bars[0].values, [4.0, 1.0]);
        assert_eq!(model.bars[0].total, 5.0);
        assert_eq!(model.bars[1].total, 0.0, "empty days keep their bar");
        assert_eq!(model.max, 5.0);
        assert_eq!(model.bars[0].label, "Oct 16");
        assert_eq!(model.bars[0].title, "Fri, Oct 16");
        let tokens = chart_model(&history(), ChartMetric::Tokens, ChartStack::Provider);
        assert_eq!(tokens.bars[2].total, 2_800.0);
    }

    #[test]
    fn models_past_the_top_five_fold_into_other() {
        let model = chart_model(&history(), ChartMetric::Tokens, ChartStack::Model);
        assert_eq!(model.series.len(), MODEL_SERIES + 1);
        assert_eq!(model.series[0].key, "claude-opus-5-5");
        assert!(model.series.last().unwrap().other);
        // Opus, gpt-5-codex, m6, m5 and m4 make the top five; m3..m0 fold.
        let other = *model.bars[2].values.last().unwrap();
        assert_eq!(other, 400.0 + 300.0 + 200.0 + 100.0);
        assert_eq!(model.bars[2].total, 2_800.0, "nothing lost in the fold");
    }

    #[test]
    fn labels_and_numbers_read_well() {
        let mut hourly = bucket(17, vec![]);
        hourly.hour = Some(15);
        assert_eq!(bucket_label(&hourly, UsageBucketSize::Hour), "3 PM");
        assert_eq!(bucket_title(&hourly, UsageBucketSize::Hour), "3 PM – 4 PM");
        hourly.hour = Some(23);
        assert_eq!(
            bucket_title(&hourly, UsageBucketSize::Hour),
            "11 PM – 12 AM"
        );
        assert_eq!(
            bucket_title(&bucket(13, vec![]), UsageBucketSize::Week),
            "Week of Oct 13, 2026"
        );
        assert_eq!(format_tokens(987), "987");
        assert_eq!(format_tokens(1_000), "1K");
        assert_eq!(format_tokens(12_345), "12K");
        assert_eq!(format_tokens(4_500_000), "4.5M");
        assert_eq!(format_tokens(999_999), "1M");
        assert_eq!(format_tokens(1_200_000_000), "1.2B");
        assert_eq!(format_usd(0.0), "$0");
        assert_eq!(format_usd(0.004), "<$0.01");
        assert_eq!(format_usd(12.345), "$12.35");
        assert_eq!(format_usd(12_345.6), "$12,346");
        let unpriced = UsageAmount {
            unpriced_tokens: 1_000,
            ..amount(1_000, 0.0)
        };
        assert_eq!(amount_cost(&unpriced), "no price");
        assert_eq!(amount_cost(&amount(10, 2.5)), "$2.50");
        assert_eq!(nice_ceiling(0.0), 1.0);
        assert_eq!(nice_ceiling(3.2), 5.0);
        assert_eq!(nice_ceiling(2_100.0), 2_500.0);
        assert_eq!(nice_ceiling(10.0), 10.0);
        let both = UsageAmount {
            turns: 2,
            calls: 1,
            ..Default::default()
        };
        assert_eq!(activity_label(&both), "2 turns · 1 call");
        assert_eq!(activity_label(&UsageAmount::default()), "");
    }
}

/// The demo recording shows a usage history without an engine.
#[cfg(feature = "demo-fixture")]
impl UsageHistoryView {
    pub fn fixture_set_data(&mut self, data: UsageHistory, cx: &mut Context<Self>) {
        self.range = data.range;
        self.data = Some(data);
        self.loading = false;
        self.error = None;
        cx.notify();
    }
}
