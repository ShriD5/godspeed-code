//! Right-pane tabs the user closed, newest last, so mod+shift+t can bring
//! back the last one in the current session. Terminals and subagent
//! transcripts are not reopenable: their content does not survive the close.

use gpui::{Context, Focusable as _, Window};
use zeron_proto::GitHistoryCommit;

use super::{RightSurface, Shell};

const CLOSED_TABS_KEPT: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ClosedSurface {
    File { owner: String, path: String },
    Browser { url: Option<String> },
    Diff,
    History,
    Commit(GitHistoryCommit),
    PullRequest { url: String },
    SideChat { chat_id: String },
}

#[derive(Default)]
pub(super) struct ClosedTabs {
    entries: Vec<(String, ClosedSurface)>,
}

impl ClosedTabs {
    pub fn push(&mut self, panel_key: &str, surface: ClosedSurface) {
        self.entries.push((panel_key.to_owned(), surface));
        if self.entries.len() > CLOSED_TABS_KEPT {
            self.entries.remove(0);
        }
    }

    pub fn pop(&mut self, panel_key: &str) -> Option<ClosedSurface> {
        let ix = self.entries.iter().rposition(|(key, _)| key == panel_key)?;
        Some(self.entries.remove(ix).1)
    }
}

impl Shell {
    /// Call before the surface's entities are dropped.
    pub(super) fn remember_closed_surface(
        &mut self,
        surface: RightSurface,
        panel_key: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(closed) = self.closed_surface(surface, cx) {
            self.closed_tabs.push(panel_key, closed);
        }
    }

    fn closed_surface(&self, surface: RightSurface, cx: &Context<Self>) -> Option<ClosedSurface> {
        match surface {
            RightSurface::File(id) => Some(ClosedSurface::File {
                owner: self.file_surfaces.get(&id)?.read(cx).chat_id().to_owned(),
                path: self.file_surface_paths.get(&id)?.clone(),
            }),
            RightSurface::Browser(id) => Some(ClosedSurface::Browser {
                url: self.browsers.get(&id)?.read(cx).url(),
            }),
            RightSurface::Diff(id) => {
                let changes = self.diffs.get(&id)?.read(cx);
                Some(match changes.commit() {
                    Some(commit) => ClosedSurface::Commit(commit.clone()),
                    None if changes.is_history() => ClosedSurface::History,
                    None => ClosedSurface::Diff,
                })
            }
            RightSurface::PullRequest(id) => Some(ClosedSurface::PullRequest {
                url: self.chat_pull_requests.get(&id)?.url.clone(),
            }),
            RightSurface::SideChat(id) => Some(ClosedSurface::SideChat {
                chat_id: self
                    .side_chats
                    .get(&id)?
                    .state
                    .read(cx)
                    .selected_chat
                    .clone()?,
            }),
            RightSurface::Picker | RightSurface::Terminal(_) | RightSurface::Subagent(_) => None,
        }
    }

    pub(super) fn reopen_closed_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_chat.is_empty() {
            return;
        }
        let key = self.panel_key(cx);
        let Some(closed) = self.closed_tabs.pop(&key) else {
            return;
        };
        self.set_surfaces_open(true, cx);
        match closed {
            ClosedSurface::File { owner, path } => {
                if let Some(owner) = self.link_owner(&owner, cx) {
                    self.add_file_surface_at(owner, path, None, window, cx);
                }
            }
            ClosedSurface::Browser { url } => self.add_browser_surface(url, window, cx),
            ClosedSurface::Diff => self.add_diff_surface(window, cx),
            ClosedSurface::History => self.add_history_surface(window, cx),
            ClosedSurface::Commit(commit) => self.add_commit_diff_surface(commit, window, cx),
            ClosedSurface::PullRequest { url } => {
                let device = self.chat_pull_request_device(None, cx);
                self.open_chat_pull_request(url, device, window, cx);
            }
            ClosedSurface::SideChat { chat_id } => {
                let chat = self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|chat| chat.id == chat_id)
                    .cloned();
                if let Some(chat) = chat {
                    self.open_side_chat(chat, key, cx);
                }
            }
        }
        cx.notify();
    }

    /// The changes pane: shows the session's diff tab (opening one if
    /// needed), or hides the pane when that tab is already in front.
    pub(super) fn toggle_diff_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_chat.is_empty() {
            return;
        }
        let active = self.resolved_right_active(cx);
        let showing_diff = self.right_pane_open(cx)
            && matches!(active, RightSurface::Diff(id)
                if self.diffs.get(&id).is_some_and(|d| d.read(cx).can_commit()));
        if showing_diff {
            self.set_surfaces_open(false, cx);
            window.focus(&self.composer.focus_handle(cx), cx);
            cx.notify();
            return;
        }
        self.set_surfaces_open(true, cx);
        let key = self.panel_key(cx);
        let existing = self
            .right_tabs
            .get(&key)
            .into_iter()
            .flatten()
            .copied()
            .find(|surface| {
                matches!(surface, RightSurface::Diff(id)
                    if self.diffs.get(id).is_some_and(|d| d.read(cx).can_commit()))
            });
        match existing {
            Some(surface) => self.set_right_active(surface, cx),
            None => self.add_diff_surface(window, cx),
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser(url: &str) -> ClosedSurface {
        ClosedSurface::Browser {
            url: Some(url.into()),
        }
    }

    #[test]
    fn reopens_newest_first_within_its_own_session() {
        let mut tabs = ClosedTabs::default();
        tabs.push("a", browser("one"));
        tabs.push("b", browser("other"));
        tabs.push("a", ClosedSurface::Diff);
        assert_eq!(tabs.pop("a"), Some(ClosedSurface::Diff));
        assert_eq!(tabs.pop("a"), Some(browser("one")));
        assert_eq!(tabs.pop("a"), None);
        assert_eq!(tabs.pop("b"), Some(browser("other")));
    }

    #[test]
    fn keeps_only_the_most_recent_closes() {
        let mut tabs = ClosedTabs::default();
        for ix in 0..CLOSED_TABS_KEPT + 5 {
            tabs.push("a", browser(&ix.to_string()));
        }
        let mut reopened = 0;
        while tabs.pop("a").is_some() {
            reopened += 1;
        }
        assert_eq!(reopened, CLOSED_TABS_KEPT);
    }
}
