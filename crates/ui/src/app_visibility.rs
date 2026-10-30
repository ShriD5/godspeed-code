//! Whether any Godspeed window is on screen, for work that only matters when
//! someone can see it (the Dock icon's frame animation, the usage gauge's
//! refresh). On macOS this is the app's occlusion state: hidden (Cmd-H),
//! minimized, fully covered or on another Space all read as not visible.
//! Observers of [`AppVisibility`] hear every change; elsewhere, and before
//! [`init`], the app always counts as visible.

use gpui::{App, Global};

/// The current answer; replaced (so observers fire) on every change.
pub struct AppVisibility {
    visible: bool,
}

impl Global for AppVisibility {}

/// Whether at least part of one window is on screen.
pub fn is_visible(cx: &App) -> bool {
    cx.try_global::<AppVisibility>()
        .is_none_or(|state| state.visible)
}

/// Start following the platform's visibility. Off in tests.
pub fn init(cx: &mut App) {
    #[cfg(all(target_os = "macos", not(test)))]
    mac::init(cx);
    #[cfg(not(all(target_os = "macos", not(test))))]
    let _ = cx;
}

#[cfg(any(test, all(target_os = "macos", not(test))))]
fn set_visible(visible: bool, cx: &mut App) {
    if cx.try_global::<AppVisibility>().map(|state| state.visible) != Some(visible) {
        cx.set_global(AppVisibility { visible });
    }
}

#[cfg(all(target_os = "macos", not(test)))]
mod mac {
    use std::ptr::NonNull;

    use futures::StreamExt as _;
    use gpui::App;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSApplicationDidChangeOcclusionStateNotification,
        NSApplicationOcclusionState,
    };
    use objc2_foundation::{NSNotification, NSNotificationCenter, NSOperationQueue};

    pub(super) fn init(cx: &mut App) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        super::set_visible(current(mtm), cx);
        // AppKit posts on the main queue; hop into the app through a channel
        // rather than touching app state from inside the notification.
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        let block = block2::RcBlock::new(move |_: NonNull<NSNotification>| {
            let _ = tx.unbounded_send(());
        });
        // SAFETY: a valid notification name, the main queue, and a block that
        // only sends on a channel. The observer lives as long as the app.
        let observer = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(NSApplicationDidChangeOcclusionStateNotification),
                None,
                Some(&NSOperationQueue::mainQueue()),
                &block,
            )
        };
        std::mem::forget(observer);
        cx.spawn(async move |cx| {
            while rx.next().await.is_some() {
                let _ = cx.update(|cx| {
                    if let Some(mtm) = MainThreadMarker::new() {
                        super::set_visible(current(mtm), cx);
                    }
                });
            }
        })
        .detach();
    }

    fn current(mtm: MainThreadMarker) -> bool {
        NSApplication::sharedApplication(mtm)
            .occlusionState()
            .contains(NSApplicationOcclusionState::Visible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn visible_until_told_otherwise_and_observers_hear_changes(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            assert!(is_visible(cx), "no platform state: visible");
            set_visible(false, cx);
            assert!(!is_visible(cx));
            set_visible(true, cx);
            assert!(is_visible(cx));
        });
    }
}
