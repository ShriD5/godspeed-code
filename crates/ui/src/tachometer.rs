//! The composer footer's tachometer: the live account's 5-hour usage as a
//! tiny rev counter in the plan-usage ring's place (same 16px glyph, same
//! chip). A 240° dial with ticks and a red zone from 90%, and a needle that
//! springs to its reading and kicks up ("revs") when a turn starts. Zero
//! tokens: it only reads the usage the accounts probe already fetched.
//!
//! The needle is a damped spring driven from render ([`Needle::advance`]),
//! so it eases to new readings and settles by itself; it asks for frames
//! only while moving, and under reduced motion it snaps and never revs.

use std::f32::consts::PI;
use std::time::Instant;

use gpui::{
    Hsla, IntoElement, PathBuilder, Pixels, Point, SharedString, canvas, div, point, prelude::*, px,
};

use crate::theme::Theme;

/// Where the dial starts (0%), clockwise from three o'clock in screen
/// space: lower left. It sweeps over the top to the lower right.
pub const START_ANGLE: f32 = 150.0 * PI / 180.0;
/// The dial's sweep, 0% to 100%.
pub const SWEEP: f32 = 240.0 * PI / 180.0;
/// The red zone starts here.
pub const RED_FROM: f32 = 0.90;
/// Amber from here up to the red zone.
pub const WARN_FROM: f32 = 0.80;
/// Major ticks every quarter.
pub const TICKS: [f32; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];

/// Spring stiffness (rad/s) and damping ratio: a quick move with one small
/// overshoot, like a real needle.
const OMEGA: f32 = 15.0;
const DAMPING: f32 = 0.62;
/// The rev's kick, in dial fractions per second.
const REV_KICK: f32 = 2.6;
/// Settled below these.
const REST_POSITION: f32 = 0.0015;
const REST_VELOCITY: f32 = 0.01;
/// Longest step one frame integrates (a stalled frame must not explode).
const MAX_STEP: f32 = 1.0 / 30.0;

/// How a reading reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Normal,
    Warn,
    Red,
}

/// The zone of a usage fraction (clamped). Pure.
pub fn zone(fraction: f32) -> Zone {
    let fraction = clamp_fraction(fraction);
    if fraction >= RED_FROM {
        Zone::Red
    } else if fraction >= WARN_FROM {
        Zone::Warn
    } else {
        Zone::Normal
    }
}

/// 0..=1, with NaN reading as empty. Pure.
pub fn clamp_fraction(fraction: f32) -> f32 {
    if fraction.is_nan() {
        0.0
    } else {
        fraction.clamp(0.0, 1.0)
    }
}

/// The needle's angle for a dial position, clamped to the dial. Pure.
pub fn needle_angle(fraction: f32) -> f32 {
    START_ANGLE + SWEEP * clamp_fraction(fraction)
}

/// The point at `angle` on a circle of `radius` around `center`. Pure.
pub fn polar(center: (f32, f32), radius: f32, angle: f32) -> (f32, f32) {
    (
        center.0 + radius * angle.cos(),
        center.1 + radius * angle.sin(),
    )
}

/// The animated needle: a damped spring chasing the reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Needle {
    /// Where the needle points, as a dial fraction (may overshoot a little
    /// past the target mid-swing; the paint clamps it).
    pub position: f32,
    pub velocity: f32,
    pub target: f32,
}

impl Needle {
    /// A needle resting on `fraction` (first paint: no sweep up from zero).
    pub fn at(fraction: f32) -> Self {
        let fraction = clamp_fraction(fraction);
        Self {
            position: fraction,
            velocity: 0.0,
            target: fraction,
        }
    }

    pub fn set_target(&mut self, fraction: f32) {
        self.target = clamp_fraction(fraction);
    }

    /// A turn started: kick the needle up; the spring brings it back.
    pub fn rev(&mut self) {
        self.velocity += REV_KICK;
    }

    pub fn settled(&self) -> bool {
        (self.position - self.target).abs() < REST_POSITION && self.velocity.abs() < REST_VELOCITY
    }

    /// Integrate `dt` seconds (semi-implicit Euler in small steps). Reduced
    /// motion snaps to the target. Returns whether it still moves.
    pub fn advance(&mut self, dt: f32, reduce_motion: bool) -> bool {
        if reduce_motion {
            *self = Self::at(self.target);
            return false;
        }
        let mut left = dt.clamp(0.0, MAX_STEP);
        while left > 0.0 {
            let step = left.min(1.0 / 240.0);
            let accel = -2.0 * DAMPING * OMEGA * self.velocity
                - OMEGA * OMEGA * (self.position - self.target);
            self.velocity += accel * step;
            self.position += self.velocity * step;
            left -= step;
        }
        // The pin at the dial's end: a rev near full scale bounces off it.
        if self.position > 1.0 {
            self.position = 1.0;
            self.velocity = -self.velocity.abs() * 0.3;
        } else if self.position < 0.0 {
            self.position = 0.0;
            self.velocity = self.velocity.abs() * 0.3;
        }
        if self.settled() {
            self.position = self.target;
            self.velocity = 0.0;
            return false;
        }
        true
    }
}

/// Render-side state: the needle and when it last moved.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tachometer {
    needle: Option<Needle>,
    last_frame: Option<Instant>,
    /// A rev asked for before the first reading arrived.
    pending_rev: bool,
}

impl Tachometer {
    pub fn rev(&mut self, reduce_motion: bool) {
        if reduce_motion {
            return;
        }
        match &mut self.needle {
            Some(needle) => needle.rev(),
            None => self.pending_rev = true,
        }
        self.last_frame = None;
    }

    /// Point the needle at `fraction` and step it to now. Returns the dial
    /// position to paint and whether another frame is needed.
    pub fn frame(&mut self, fraction: f32, reduce_motion: bool) -> (f32, bool) {
        let now = Instant::now();
        let needle = self.needle.get_or_insert_with(|| Needle::at(fraction));
        needle.set_target(fraction);
        if std::mem::take(&mut self.pending_rev) && !reduce_motion {
            needle.rev();
        }
        let dt = self
            .last_frame
            .map_or(1.0 / 120.0, |at| now.duration_since(at).as_secs_f32());
        let moving = if needle.settled() && needle.position == needle.target {
            false
        } else {
            needle.advance(dt, reduce_motion)
        };
        self.last_frame = moving.then_some(now);
        (needle.position, moving)
    }
}

fn stroke_polyline(
    points: impl IntoIterator<Item = Point<Pixels>>,
    width: f32,
) -> Option<gpui::Path<Pixels>> {
    let mut path = PathBuilder::stroke(px(width));
    let mut first = true;
    for p in points {
        if first {
            path.move_to(p);
            first = false;
        } else {
            path.line_to(p);
        }
    }
    path.build().ok()
}

/// The dial's colours, from the theme.
#[derive(Debug, Clone, Copy)]
pub struct DialColors {
    pub track: Hsla,
    pub ticks: Hsla,
    pub red_zone: Hsla,
    pub needle: Hsla,
    pub hub: Hsla,
}

impl DialColors {
    pub fn for_reading(fraction: f32, theme: &Theme) -> Self {
        let needle = match zone(fraction) {
            Zone::Normal => theme.text,
            Zone::Warn => theme.warning,
            Zone::Red => theme.danger,
        };
        Self {
            track: theme.text_faint.opacity(0.32),
            ticks: theme.text_muted.opacity(0.7),
            red_zone: theme.danger.opacity(0.85),
            needle,
            hub: needle,
        }
    }
}

/// The 16px dial: track arc, red zone, ticks, needle and hub. `position`
/// is the needle's dial fraction.
pub fn dial(position: f32, colors: DialColors) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let origin = bounds.origin;
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            // The dial's open side is at the bottom: drop the centre a touch
            // so the arc sits optically centred in the square.
            let center = (w / 2.0, h / 2.0 + h * 0.06);
            let radius = w.min(h) * 0.42;
            let at = |(x, y): (f32, f32)| point(origin.x + px(x), origin.y + px(y));
            let arc = |from: f32, to: f32, r: f32| {
                let steps = ((to - from).abs() * 48.0).ceil().max(2.0) as usize;
                (0..=steps).map(move |i| {
                    let t = from + (to - from) * i as f32 / steps as f32;
                    at(polar(center, r, needle_angle(t)))
                })
            };
            if let Some(path) = stroke_polyline(arc(0.0, RED_FROM, radius), 1.4) {
                window.paint_path(path, colors.track);
            }
            if let Some(path) = stroke_polyline(arc(RED_FROM, 1.0, radius), 1.8) {
                window.paint_path(path, colors.red_zone);
            }
            for tick in TICKS {
                let angle = needle_angle(tick);
                let outer = polar(center, radius - 1.5, angle);
                let inner = polar(center, radius - 3.0, angle);
                if let Some(path) = stroke_polyline([at(inner), at(outer)], 1.0) {
                    window.paint_path(path, colors.ticks);
                }
            }
            let angle = needle_angle(position);
            let tip = polar(center, radius - 0.6, angle);
            let tail = polar(center, -1.4, angle);
            if let Some(path) = stroke_polyline([at(tail), at(tip)], 1.3) {
                window.paint_path(path, colors.needle);
            }
            let mut hub = PathBuilder::fill();
            for i in 0..=12 {
                let p = at(polar(center, 1.6, i as f32 / 12.0 * 2.0 * PI));
                if i == 0 {
                    hub.move_to(p);
                } else {
                    hub.line_to(p);
                }
            }
            if let Ok(path) = hub.build() {
                window.paint_path(path, colors.hub);
            }
        },
    )
    .size(px(16.0))
}

/// The footer chip around the dial: identical geometry to
/// [`crate::context_usage::ring_chip`] so it sits beside the context ring
/// as an equal. `label` is the percent; `open` holds the hover wash.
pub fn chip(
    id: &'static str,
    position: f32,
    reading: f32,
    label: String,
    open: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let text = match zone(reading) {
        Zone::Normal => theme.text_muted,
        Zone::Warn => theme.warning,
        Zone::Red => theme.danger,
    };
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(24.0))
        .px(px(6.0))
        .rounded(px(6.0))
        .text_size(px(11.0))
        .text_color(text)
        .cursor_pointer()
        .when(open, |s| s.bg(crate::theme::ink(0.05)))
        .hover(|s| s.bg(crate::theme::ink(0.05)))
        .child(dial(position, DialColors::for_reading(reading, theme)))
        .child(SharedString::from(label))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn the_dial_runs_from_lower_left_over_the_top_to_lower_right() {
        assert!(close(needle_angle(0.0), 150f32.to_radians()));
        assert!(
            close(needle_angle(0.5), 270f32.to_radians()),
            "half is straight up"
        );
        assert!(close(needle_angle(1.0), 390f32.to_radians()));
        let (x, y) = polar((0.0, 0.0), 1.0, needle_angle(0.5));
        assert!(close(x, 0.0) && close(y, -1.0), "screen y grows down");
        let (x0, y0) = polar((0.0, 0.0), 1.0, needle_angle(0.0));
        let (x1, y1) = polar((0.0, 0.0), 1.0, needle_angle(1.0));
        assert!(
            x0 < 0.0 && x1 > 0.0 && y0 > 0.0 && close(y0, y1),
            "symmetric ends below centre"
        );
    }

    #[test]
    fn readings_clamp_to_the_dial() {
        assert_eq!(needle_angle(-0.3), needle_angle(0.0));
        assert_eq!(needle_angle(1.7), needle_angle(1.0));
        assert_eq!(needle_angle(f32::NAN), needle_angle(0.0));
        assert_eq!(Needle::at(3.0).target, 1.0);
    }

    #[test]
    fn zones_turn_red_from_ninety_percent() {
        assert_eq!(zone(0.0), Zone::Normal);
        assert_eq!(zone(0.79), Zone::Normal);
        assert_eq!(zone(0.8), Zone::Warn);
        assert_eq!(zone(0.899), Zone::Warn);
        assert_eq!(zone(0.9), Zone::Red);
        assert_eq!(zone(1.4), Zone::Red);
        assert_eq!(zone(-1.0), Zone::Normal);
    }

    fn run(needle: &mut Needle, seconds: f32) -> (f32, f32) {
        let (mut low, mut high) = (needle.position, needle.position);
        let mut t = 0.0;
        while t < seconds {
            needle.advance(1.0 / 120.0, false);
            low = low.min(needle.position);
            high = high.max(needle.position);
            t += 1.0 / 120.0;
        }
        (low, high)
    }

    #[test]
    fn the_needle_eases_to_its_reading_and_settles() {
        let mut needle = Needle::at(0.2);
        needle.set_target(0.6);
        let (_, high) = run(&mut needle, 2.0);
        assert!(needle.settled());
        assert_eq!(needle.position, 0.6);
        assert!(high > 0.6 && high < 0.66, "one small overshoot, got {high}");
        assert!(
            !needle.advance(1.0 / 120.0, false),
            "a settled needle asks for no frames"
        );
    }

    #[test]
    fn a_rev_kicks_up_then_settles_back() {
        let mut needle = Needle::at(0.4);
        needle.rev();
        let (low, high) = run(&mut needle, 2.0);
        assert!(high > 0.45 && high < 0.6, "kicks up a little, got {high}");
        assert!(low > 0.3, "and does not slam down, got {low}");
        assert_eq!(needle.position, 0.4);
        // Near full scale the pin stops it.
        let mut pegged = Needle::at(0.98);
        pegged.rev();
        let (_, high) = run(&mut pegged, 2.0);
        assert!(high <= 1.0);
        assert_eq!(pegged.position, 0.98);
    }

    #[test]
    fn reduced_motion_snaps_and_never_revs() {
        let mut needle = Needle::at(0.1);
        needle.set_target(0.7);
        needle.rev();
        assert!(!needle.advance(1.0 / 120.0, true));
        assert_eq!(needle, Needle::at(0.7));
        let mut tach = Tachometer::default();
        assert_eq!(tach.frame(0.5, true), (0.5, false));
        tach.rev(true);
        assert_eq!(tach.frame(0.5, true), (0.5, false));
    }

    #[test]
    fn the_first_reading_paints_in_place_and_a_rev_animates() {
        let mut tach = Tachometer::default();
        assert_eq!(tach.frame(0.3, false), (0.3, false));
        tach.rev(false);
        let (position, moving) = tach.frame(0.3, false);
        assert!(moving);
        assert!(position >= 0.3);
    }

    #[test]
    fn a_huge_frame_gap_cannot_fling_the_needle() {
        let mut needle = Needle::at(0.0);
        needle.set_target(1.0);
        needle.advance(10.0, false);
        assert!(needle.position <= 1.0 && needle.position >= 0.0);
    }
}
