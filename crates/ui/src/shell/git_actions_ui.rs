//! Shell glue for git write actions: the Create pull request dialog overlay
//! and the command palette's Commit / Push / Pull / Create PR entries. The
//! actions themselves live in [`crate::git_actions`].

use super::*;
use crate::git_actions::{
    CreatePullRequestDialog, CreatePullRequestEvent, GitTarget, run_sync_action,
};

impl Shell {
    /// Open the Create pull request dialog for `target` (one at a time).
    pub(super) fn open_create_pull_request(
        &mut self,
        target: GitTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.create_pull_request.is_some() {
            return;
        }
        let dialog = cx.new(|cx| CreatePullRequestDialog::new(self.state.clone(), target, cx));
        let events = cx.subscribe(&dialog, |this: &mut Self, _, event, cx| match event {
            CreatePullRequestEvent::Close => {
                this.create_pull_request = None;
                cx.notify();
            }
        });
        let handle = dialog.read(cx).title_focus(cx);
        window.focus(&handle, cx);
        self.create_pull_request = Some((dialog, events));
        cx.notify();
    }

    pub(super) fn render_create_pull_request(
        &self,
        viewport: gpui::Size<Pixels>,
    ) -> Option<AnyElement> {
        let (dialog, _) = self.create_pull_request.as_ref()?;
        Some(popover::modal(
            "create-pull-request-dialog",
            viewport,
            dialog.clone().into_any_element(),
        ))
    }

    /// Escape closes the dialog.
    pub(super) fn close_create_pull_request(&mut self, cx: &mut Context<Self>) -> bool {
        if self.create_pull_request.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    /// The focused chat's checkout, for palette git commands.
    pub(super) fn palette_git_target(&self, cx: &App) -> Option<GitTarget> {
        GitTarget::for_selected_chat(self.state.read(cx))
    }

    /// Palette → Commit: show the working tree diff and focus its commit box.
    pub(super) fn palette_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_git_target(cx).is_none() {
            return;
        }
        let existing = self
            .active_changes(cx)
            .filter(|changes| changes.read(cx).can_commit());
        let changes = match existing {
            Some(changes) => changes,
            None => {
                let key = self.panel_key(cx);
                let was_open = self.panels.get(&key).changes_open;
                let from = self.right_target(cx);
                self.panels.update(&key, |panel| panel.changes_open = true);
                if !was_open {
                    self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
                }
                self.add_diff_surface(window, cx);
                let Some(changes) = self.active_changes(cx) else {
                    return;
                };
                changes
            }
        };
        changes.update(cx, |changes, cx| changes.focus_commit_message(window, cx));
    }

    pub(super) fn palette_sync_action(&mut self, method: &'static str, cx: &mut Context<Self>) {
        let (Some(target), Some(engine)) = (
            self.palette_git_target(cx),
            self.state.read(cx).engine().cloned(),
        ) else {
            return;
        };
        run_sync_action(engine, target, method, cx);
    }

    pub(super) fn palette_create_pull_request(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = self.palette_git_target(cx) {
            self.open_create_pull_request(target, window, cx);
        }
    }
}
