//! The banner above the composer of a project just added to another device
//! while its ignored files are still being copied ([`crate::add_to_device`]).
//! Sends wait until the copy lands; a failed copy says which files and why,
//! and offers Retry or Dismiss (which releases the composer without them).
use super::*;
use crate::add_to_device::RelayPhase;
use crate::settings::widgets::{self, ActionTone};

impl Composer {
    pub(super) fn render_relay_banner(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let (space_id, title, failures) = {
            let state = self.state.read(cx);
            let (space_id, relay) = state.composer_project_relay()?;
            let failures = match &relay.phase {
                RelayPhase::Copying => None,
                RelayPhase::Failed(failures) => Some(failures.clone()),
            };
            (space_id.to_string(), relay.title(), failures)
        };
        let failed = failures.is_some();
        let tone = if failed {
            theme.danger
        } else {
            theme.text_muted
        };
        let detail: SharedString = match &failures {
            None => "Messages wait until the files are in place.".into(),
            Some(failures) => failures.join("\n").into(),
        };
        let actions = failed.then(|| {
            let retry_id = space_id.clone();
            let dismiss_id = space_id.clone();
            div()
                .flex()
                .flex_row()
                .gap(px(4.0))
                .child(
                    widgets::text_action(theme, ActionTone::Outlined, "Retry")
                        .id("relay-banner-retry")
                        .tab_index(0)
                        .role(gpui::Role::Button)
                        .focus_visible(|s| s.border_2().border_color(theme.accent))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            crate::add_to_device::run_relay(&this.state, retry_id.clone(), cx);
                        })),
                )
                .child(
                    widgets::text_action(theme, ActionTone::Quiet, "Dismiss")
                        .id("relay-banner-dismiss")
                        .tab_index(0)
                        .role(gpui::Role::Button)
                        .focus_visible(|s| s.border_2().border_color(theme.accent))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.state.update(cx, |state, cx| {
                                state.dismiss_project_relay(&dismiss_id, cx)
                            });
                        })),
                )
        });
        Some(
            crate::motion::fade_in(
                "composer-relay-banner",
                div()
                    .id("composer-relay-banner")
                    .debug_selector(|| "composer-relay-banner".into())
                    .mx(px(4.0))
                    .mt(px(6.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(tone.opacity(0.22))
                    .bg(tone.opacity(0.06))
                    .px(px(12.0))
                    .py(px(8.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .line_height(px(16.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                crate::icons::icon(if failed {
                                    crate::icons::DANGER_TRIANGLE
                                } else {
                                    crate::icons::COPY
                                })
                                .size(px(14.0))
                                .text_color(tone),
                            )
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from(title)),
                            ),
                    )
                    .child(
                        div()
                            .text_color(theme.text_muted)
                            .whitespace_normal()
                            .child(detail),
                    )
                    .children(actions),
            )
            .into_any_element(),
        )
    }
}
