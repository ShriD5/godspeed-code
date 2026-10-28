//! The "Delete session?" confirmation, for one row or a bulk-bar selection.
//! A chat that runs in its own worktree offers to remove that worktree too;
//! the engine refuses a dirty one, and the dialog then offers to force it.

use gpui::{AnyElement, Context, Pixels, SharedString, Task, div, prelude::*, px};
use zeron_rpc::methods;

use super::Shell;
use crate::popover;
use crate::theme::Theme;
use crate::transcript;

pub(super) struct DeleteChatConfirm {
    /// The chats still to delete; a batch keeps the ones the engine refused.
    pub chat_ids: Vec<String>,
    /// Whether any of them has its own worktree.
    has_worktree: bool,
    remove_worktree: bool,
    refusal: Option<WorktreeRefusal>,
    task: Option<Task<()>>,
}

struct WorktreeRefusal {
    message: String,
    dirty: bool,
}

impl DeleteChatConfirm {
    fn force(&self) -> bool {
        self.remove_worktree && self.refusal.as_ref().is_some_and(|r| r.dirty)
    }
}

impl Shell {
    pub(super) fn request_delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.request_delete_chats(vec![chat_id], cx);
    }

    pub(super) fn request_delete_chats(&mut self, chat_ids: Vec<String>, cx: &mut Context<Self>) {
        let has_worktree = chat_ids
            .iter()
            .any(|chat_id| self.chat_has_own_worktree(chat_id, cx));
        self.delete_confirm = Some(DeleteChatConfirm {
            chat_ids,
            has_worktree,
            remove_worktree: false,
            refusal: None,
            task: None,
        });
        cx.notify();
    }

    /// Same test as the composer's checkout chip: a cwd away from the
    /// project folder. Only this device's engine can remove it.
    fn chat_has_own_worktree(&self, chat_id: &str, cx: &Context<Self>) -> bool {
        let state = self.state.read(cx);
        let Some(chat) = state.chats.iter().find(|chat| chat.id == chat_id) else {
            return false;
        };
        let local = state.local_device_id.as_deref() == Some(chat.device_id.as_str());
        let space = chat.space_id.as_deref().and_then(|id| state.space_row(id));
        local
            && chat
                .cwd
                .as_deref()
                .zip(space)
                .is_some_and(|(cwd, space)| cwd != space.path)
    }

    fn toggle_delete_worktree(&mut self, cx: &mut Context<Self>) {
        if let Some(confirm) = self.delete_confirm.as_mut().filter(|c| c.task.is_none()) {
            confirm.remove_worktree = !confirm.remove_worktree;
            confirm.refusal = None;
            cx.notify();
        }
    }

    fn confirm_delete_chat(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.delete_confirm.as_ref() else {
            return;
        };
        if confirm.task.is_some() {
            return;
        }
        let chat_ids = confirm.chat_ids.clone();
        let force = confirm.force();
        if !confirm.remove_worktree {
            self.delete_confirm = None;
            for chat_id in chat_ids {
                self.delete_chat(chat_id, cx);
            }
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.show_notice(crate::toast::ToastKind::Error, "Engine not connected", cx);
            return;
        };
        // Only a chat with its own worktree asks the engine to remove one.
        let requests: Vec<(String, serde_json::Value)> = chat_ids
            .into_iter()
            .map(|chat_id| {
                let params = if self.chat_has_own_worktree(&chat_id, cx) {
                    serde_json::json!({
                        "op": "deleteChat",
                        "chatId": chat_id,
                        "removeWorktree": true,
                        "force": force,
                    })
                } else {
                    serde_json::json!({ "op": "deleteChat", "chatId": chat_id })
                };
                (chat_id, params)
            })
            .collect();
        let task = cx.spawn(async move |this, cx| {
            let mut refused: Vec<(String, String)> = Vec::new();
            for (chat_id, params) in requests {
                let result = engine.client().call(methods::MUTATE, params).await;
                let alive = this
                    .update(cx, |shell, cx| match result {
                        Ok(_) => shell.forget_deleted_chat(chat_id, cx),
                        Err(error) => refused.push((chat_id, error.to_string())),
                    })
                    .is_ok();
                if !alive {
                    return;
                }
            }
            this.update(cx, |shell, cx| {
                if refused.is_empty() {
                    shell.delete_confirm = None;
                } else if let Some(confirm) = shell.delete_confirm.as_mut() {
                    let dirty = refused
                        .iter()
                        .any(|(_, message)| message.starts_with(zeron_engine::repos::WORKTREE_DIRTY));
                    let message = match refused.as_slice() {
                        [(_, message)] => message.clone(),
                        [(_, first), ..] => {
                            format!("{} worktrees could not be removed: {first}", refused.len())
                        }
                        [] => unreachable!(),
                    };
                    confirm.chat_ids = refused.into_iter().map(|(chat_id, _)| chat_id).collect();
                    confirm.task = None;
                    confirm.refusal = Some(WorktreeRefusal { dirty, message });
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(confirm) = self.delete_confirm.as_mut() {
            confirm.task = Some(task);
        }
        cx.notify();
    }

    pub(super) fn render_delete_chat_dialog(
        &mut self,
        viewport: gpui::Size<Pixels>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let confirm = self.delete_confirm.as_ref()?;
        let batch = confirm.chat_ids.len();
        let (heading, body) = if batch == 1 {
            let title = transcript::single_line(
                &self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| Some(&c.id) == confirm.chat_ids.first())
                    .and_then(|c| c.title.clone())
                    .unwrap_or_else(|| "New session".into()),
            );
            (
                "Delete session?".to_owned(),
                format!("\u{201C}{title}\u{201D} will be permanently deleted. This can\u{2019}t be undone."),
            )
        } else {
            (
                format!("Delete {batch} sessions?"),
                format!("{batch} sessions will be permanently deleted. This can\u{2019}t be undone."),
            )
        };
        let busy = confirm.task.is_some();
        let confirm_label = match (busy, confirm.force()) {
            (true, _) => "Deleting…",
            (false, true) => "Delete anyway",
            (false, false) => "Delete",
        };
        let worktree_option = confirm
            .has_worktree
            .then(|| self.render_worktree_option(confirm.remove_worktree, batch > 1, theme, cx));
        let refusal = confirm
            .refusal
            .as_ref()
            .map(|refusal| refusal_copy(refusal, theme));
        let card = popover::dialog_card(theme)
            .child(popover::dialog_title(theme, &heading))
            .child(div().mt(px(6.0)).child(popover::dialog_body(theme, body)))
            .children(worktree_option)
            .children(refusal)
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(theme, "Cancel", "delete-chat-cancel")
                            .id("delete-chat-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.delete_confirm = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_danger(theme, confirm_label)
                            .id("delete-chat-confirm")
                            .when(busy, |el| el.opacity(0.5).cursor_default())
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_delete_chat(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal("delete-chat-dialog", viewport, card))
    }

    fn render_worktree_option(
        &self,
        checked: bool,
        plural: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shell = cx.entity().downgrade();
        let checkbox = gpui_base::Checkbox::new("delete-chat-worktree")
            .checked(checked)
            .accessibility_label(if plural {
                "Also delete their worktrees"
            } else {
                "Also delete its worktree"
            })
            .focus_visible(|style| style.border_color(theme.text))
            .size(px(16.0))
            .flex_none()
            .border_1()
            .rounded(px(3.0))
            .border_color(if checked { theme.accent } else { theme.border })
            .bg(if checked {
                theme.accent
            } else {
                gpui::transparent_black()
            })
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(checked, |checkbox| {
                checkbox.child(
                    crate::icons::icon(crate::icons::CHECK)
                        .size(px(12.0))
                        .text_color(theme.bg),
                )
            })
            .on_change(move |_, _, _, cx| {
                shell
                    .update(cx, |shell, cx| shell.toggle_delete_worktree(cx))
                    .ok();
            });
        div()
            .mt(px(14.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(checkbox)
            .child(
                div()
                    .id("delete-chat-worktree-label")
                    .cursor_pointer()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text)
                    .child(SharedString::from(if plural {
                        "Also delete their worktrees"
                    } else {
                        "Also delete its worktree"
                    }))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_delete_worktree(cx))),
            )
            .into_any_element()
    }
}

fn refusal_copy(refusal: &WorktreeRefusal, theme: &Theme) -> AnyElement {
    let hint = if refusal.dirty {
        " Delete anyway to discard those changes."
    } else {
        ""
    };
    div()
        .mt(px(10.0))
        .text_size(crate::typography::ui_rems(12.0))
        .line_height(px(17.0))
        .text_color(theme.danger)
        .child(SharedString::from(format!(
            "{}.{hint}",
            refusal.message.trim_end_matches('.')
        )))
        .into_any_element()
}
