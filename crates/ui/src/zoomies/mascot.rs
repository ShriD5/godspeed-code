//! The speedster, small, on zoomie rows: idle by default, jogging while a
//! run works, asleep in the empty state.

use std::sync::{Arc, OnceLock};

use gpui::{IntoElement, Styled as _, img, px};

use crate::speedster::Frame;

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
