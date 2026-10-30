//! The speedster, small, on zoomie rows: idle by default, jogging while a
//! run works, asleep in the empty state.
//!
//! A running row's jog is its own tiny cached view on the shared pulse clock
//! (like the loaders' `MiniSpinner`): each step repaints one 18 pt image,
//! never the shell, and the clock parks as soon as no jog is painted
//! (section collapsed, window hidden, run finished).

use std::sync::{Arc, OnceLock};

use gpui::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, RenderOnce,
    SharedString, Styled as _, Window, div, img, px,
};

use crate::motion::{CubicBezier, MotionSpec};
use crate::speedster::{Frame, Mood};

/// The 44 px exports (2x for the ~22 pt sidebar glyph), same order as
/// [`Frame`].
static PNGS: [&[u8]; Frame::COUNT] = [
    include_bytes!("../../assets/speedster/idle_1@44.png"),
    include_bytes!("../../assets/speedster/idle_2@44.png"),
    include_bytes!("../../assets/speedster/sleeping_1@44.png"),
    include_bytes!("../../assets/speedster/sleeping_2@44.png"),
    include_bytes!("../../assets/speedster/jog_1@44.png"),
    include_bytes!("../../assets/speedster/jog_2@44.png"),
    include_bytes!("../../assets/speedster/jog_3@44.png"),
    include_bytes!("../../assets/speedster/jog_4@44.png"),
    include_bytes!("../../assets/speedster/sprint_1@44.png"),
    include_bytes!("../../assets/speedster/sprint_2@44.png"),
    include_bytes!("../../assets/speedster/sprint_3@44.png"),
    include_bytes!("../../assets/speedster/sprint_4@44.png"),
    include_bytes!("../../assets/speedster/alert_1@44.png"),
    include_bytes!("../../assets/speedster/alert_2@44.png"),
    include_bytes!("../../assets/speedster/celebrate_1@44.png"),
    include_bytes!("../../assets/speedster/celebrate_2@44.png"),
    include_bytes!("../../assets/speedster/tired@44.png"),
    include_bytes!("../../assets/speedster/oops@44.png"),
];

/// The larger export for the empty state's sleeping speedster.
static SLEEPING_256: &[u8] = include_bytes!("../../assets/speedster/sleeping_1@256.png");

fn image(frame: Frame) -> Arc<gpui::Image> {
    static IMAGES: OnceLock<Vec<Arc<gpui::Image>>> = OnceLock::new();
    IMAGES.get_or_init(|| {
        PNGS.iter()
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

/// Which pose a row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pose {
    Idle,
    /// Jogging, on frame `n` of the jog cycle.
    Jog(usize),
    Oops,
    Alert,
}

impl Pose {
    pub fn frame(self) -> Frame {
        match self {
            Self::Idle => Frame::Idle1,
            Self::Jog(n) => {
                let frames = crate::speedster::Mood::Jog.frames();
                frames[n % frames.len()]
            }
            Self::Oops => Frame::Oops,
            Self::Alert => Frame::Alert1,
        }
    }

    /// The pose for a row: reduce motion holds the first jog frame.
    pub fn for_row(status: super::rows::RowStatus, jog_frame: usize, still: bool) -> Self {
        use super::rows::RowStatus;
        match status {
            RowStatus::Running => Self::Jog(if still { 0 } else { jog_frame }),
            RowStatus::Failed | RowStatus::Paused => Self::Oops,
            RowStatus::NeedsYou => Self::Alert,
            _ => Self::Idle,
        }
    }
}

/// A `size`-pt speedster in `pose`.
pub fn mascot(pose: Pose, size: f32) -> impl IntoElement {
    img(image(pose.frame())).size(px(size)).flex_none()
}

/// One jog cycle: [`Mood::Jog`]'s frames at its frame rate (4 × 125 ms).
const JOG_CYCLE: MotionSpec = MotionSpec::new(500, CubicBezier::new(0.0, 0.0, 1.0, 1.0));
/// Pulse-clock ticks (~33 ms) per repaint: ~7.5 Hz, the jog's own 8 fps.
const JOG_STRIDE: u64 = 4;

/// The jog frame for a phase `[0, 1)` of [`JOG_CYCLE`].
pub(crate) fn jog_frame_at(phase: f32) -> usize {
    let frames = Mood::Jog.frames().len();
    ((phase.clamp(0.0, 0.999_999) * frames as f32) as usize).min(frames - 1)
}

/// A `size`-pt speedster jogging, keyed by `key` (one per row). Reduce
/// motion, including the background pause, holds the first jog frame and
/// schedules nothing.
pub fn jogging(key: impl Into<SharedString>, size: f32) -> impl IntoElement {
    Jogging {
        key: key.into(),
        size,
    }
}

#[derive(IntoElement)]
struct Jogging {
    key: SharedString,
    size: f32,
}

impl RenderOnce for Jogging {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let size = self.size;
        let view = window.with_global_id(self.key.into(), |id, window| {
            window.with_element_state(id, |previous: Option<Entity<JoggingView>>, _| {
                let view = previous.unwrap_or_else(|| cx.new(|_| JoggingView { size }));
                view.update(cx, |view, cx| {
                    if view.size != size {
                        view.size = size;
                        cx.notify();
                    }
                });
                (view.clone(), view)
            })
        });
        // Cached: a pulse repaint touches only this view.
        view.cached(gpui::StyleRefinement::default().size(px(size)))
    }
}

struct JoggingView {
    size: f32,
}

impl Render for JoggingView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let phase = crate::motion::pulse_delta_stride(&JOG_CYCLE, cx.entity_id(), JOG_STRIDE, cx);
        let frame = Pose::Jog(jog_frame_at(phase)).frame();
        div()
            .size(px(self.size))
            .child(img(image(frame)).size(px(self.size)).flex_none())
    }
}

/// The sleeping speedster for the empty state.
pub fn sleeping(size: f32) -> impl IntoElement {
    static IMAGE: OnceLock<Arc<gpui::Image>> = OnceLock::new();
    let image = IMAGE
        .get_or_init(|| {
            Arc::new(gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                SLEEPING_256.to_vec(),
            ))
        })
        .clone();
    img(image).size(px(size)).flex_none()
}

#[cfg(test)]
mod tests {
    use super::super::rows::RowStatus;
    use super::*;

    #[test]
    fn the_jog_cycle_steps_through_every_frame_once() {
        let frames: Vec<Frame> = [0.0, 0.25, 0.5, 0.75, 0.999]
            .into_iter()
            .map(|phase| Pose::Jog(jog_frame_at(phase)).frame())
            .collect();
        assert_eq!(
            frames,
            vec![
                Frame::Jog1,
                Frame::Jog2,
                Frame::Jog3,
                Frame::Jog4,
                Frame::Jog4
            ]
        );
        // Reduced motion pins the phase at 0: the first jog frame.
        assert_eq!(Pose::Jog(jog_frame_at(0.0)).frame(), Frame::Jog1);
        // The cycle runs at the jog's own frame rate.
        let per_frame = JOG_CYCLE.total() / Mood::Jog.frames().len() as u32;
        assert_eq!(Some(per_frame), Mood::Jog.frame_interval());
    }

    #[test]
    fn running_rows_jog_unless_motion_is_reduced() {
        assert_eq!(
            Pose::for_row(RowStatus::Running, 2, false).frame(),
            Frame::Jog3
        );
        assert_eq!(
            Pose::for_row(RowStatus::Running, 5, false).frame(),
            Frame::Jog2
        );
        assert_eq!(
            Pose::for_row(RowStatus::Running, 2, true).frame(),
            Frame::Jog1
        );
        assert_eq!(
            Pose::for_row(RowStatus::Succeeded, 2, false).frame(),
            Frame::Idle1
        );
        assert_eq!(
            Pose::for_row(RowStatus::Fresh, 0, false).frame(),
            Frame::Idle1
        );
        assert_eq!(
            Pose::for_row(RowStatus::Failed, 0, false).frame(),
            Frame::Oops
        );
        assert_eq!(
            Pose::for_row(RowStatus::NeedsYou, 0, false).frame(),
            Frame::Alert1
        );
    }
}
