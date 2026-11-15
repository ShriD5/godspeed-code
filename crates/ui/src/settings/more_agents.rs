//! Settings → Providers → "More agents": the public ACP agent registry as the
//! target device sees it, with one-click install, update and uninstall.
//! Installed agents then appear as ordinary providers above (enable, pick in
//! the composer). Registry rows are untrusted data: the engine validated
//! them, refuses anything not pinned to a checksum, and this page only shows
//! text from them.

use super::HarnessesPage;
use crate::popover::Loadable;
use crate::{settings::widgets, theme::Theme};
use gpui::{AnyElement, Context, SharedString, div, prelude::*, px};
use zeron_proto::{AcpDistributionKind, AcpRegistryAgentInfo, AcpRegistrySnapshot};
use zeron_rpc::methods;

/// `12.3 MB`-style size label.
pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// The row's meta line: publisher, version, install size, route.
pub(crate) fn agent_meta(agent: &AcpRegistryAgentInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(publisher) = &agent.publisher {
        parts.push(publisher.clone());
    }
    match &agent.installed_version {
        Some(installed) if agent.update_available() => {
            parts.push(format!("v{installed} → v{}", agent.version))
        }
        Some(installed) => parts.push(format!("v{installed} installed")),
        None => parts.push(format!("v{}", agent.version)),
    }
    if let Some(size) = agent.install_size {
        parts.push(format_size(size));
    }
    match agent.distribution {
        Some(AcpDistributionKind::Binary) => parts.push("checksum-pinned binary".into()),
        Some(AcpDistributionKind::Npm) => parts.push("npm".into()),
        None => {}
    }
    parts.join(" · ")
}

/// Rows the page lists: built-in duplicates are already providers above.
pub(crate) fn listed_agents(snapshot: &AcpRegistrySnapshot) -> Vec<&AcpRegistryAgentInfo> {
    snapshot
        .agents
        .iter()
        .filter(|agent| agent.built_in.is_none())
        .collect()
}

impl HarnessesPage {
    pub(super) fn load_acp_registry(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({ "force": force }));
        let target = self.target_device.clone();
        if !matches!(self.acp, Loadable::Ready(_)) {
            self.acp = Loadable::Loading;
        }
        self.acp_load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_ACP_REGISTRY, params)
                .await
                .map_err(|e| e.to_string())
                .and_then(|value| {
                    serde_json::from_value::<AcpRegistrySnapshot>(value).map_err(|e| e.to_string())
                });
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                page.acp = match result {
                    Ok(snapshot) => Loadable::Ready(snapshot),
                    Err(error) => Loadable::Error(error),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn acp_action(&mut self, agent_id: String, uninstall: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self.acp_busy.is_some() {
            return;
        }
        let method = if uninstall {
            methods::UNINSTALL_ACP_AGENT
        } else {
            methods::INSTALL_ACP_AGENT
        };
        let params = self.with_target(serde_json::json!({ "agentId": agent_id }));
        let target = self.target_device.clone();
        self.acp_busy = Some(agent_id);
        self.error = None;
        self.acp_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|e| e.to_string())
                .and_then(|value| {
                    serde_json::from_value::<AcpRegistrySnapshot>(value).map_err(|e| e.to_string())
                });
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                page.acp_busy = None;
                match result {
                    Ok(snapshot) => {
                        page.acp = Loadable::Ready(snapshot);
                        // The installed set changed: the provider rows and
                        // every composer's catalog reload.
                        page.refresh_harness_list(cx);
                        crate::pickers::bump_harness_catalog(cx);
                    }
                    Err(error) => {
                        page.error = Some(if uninstall {
                            format!("Uninstall failed — {error}")
                        } else {
                            format!("Installation failed — {error}")
                        })
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn render_more_agents(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let refresh = widgets::text_action(theme, widgets::ActionTone::Quiet, "Refresh")
            .id("acp-registry-refresh")
            .on_click(cx.listener(|page, _, _, cx| page.load_acp_registry(true, cx)));
        let body: AnyElement = match &self.acp {
            Loadable::Idle | Loadable::Loading => widgets::section_card(theme)
                .p(px(16.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child("Loading the ACP agent registry…"),
                )
                .into_any_element(),
            Loadable::Error(message) => widgets::error_strip(
                theme,
                format!("The ACP agent registry is unavailable: {message}"),
            )
            .into_any_element(),
            Loadable::Ready(snapshot) => {
                let agents = listed_agents(snapshot);
                let rows: Vec<AnyElement> = agents
                    .iter()
                    .enumerate()
                    .map(|(ix, agent)| self.render_registry_row(ix, agent, theme, cx))
                    .collect();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .when_some(snapshot.error.clone(), |el, error| {
                        el.child(widgets::warning_strip(
                            theme,
                            format!("Showing the cached registry — refresh failed: {error}"),
                        ))
                    })
                    .child(widgets::section_card(theme).children(rows))
                    .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(widgets::section_label(theme, "More agents"))
                    .child(refresh),
            )
            .child(widgets::page_subtitle(
                theme,
                "From the public ACP agent registry. Installs land in Godspeed's own data \
                 folder and only run when pinned to a checksum.",
            ))
            .child(body)
            .into_any_element()
    }

    fn render_registry_row(
        &self,
        ix: usize,
        agent: &AcpRegistryAgentInfo,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.acp_busy.as_deref() == Some(agent.id.as_str());
        let installed = agent.installed_version.is_some();
        let installable = agent.unavailable.is_none();
        let id = agent.id.clone();
        let mut actions: Vec<AnyElement> = Vec::new();
        if busy {
            actions.push(
                crate::loaders::mini_mono_spinner(
                    format!("acp-agent-spinner-{}", agent.id),
                    1.5,
                    theme.text_muted,
                    cx.entity_id(),
                    cx,
                )
                .into_any_element(),
            );
        } else {
            if (!installed || agent.update_available()) && installable {
                let id = id.clone();
                actions.push(
                    widgets::ghost_action(theme)
                        .id(("acp-agent-install", ix))
                        .flex_none()
                        .when(self.acp_busy.is_none(), |el| {
                            el.on_click(cx.listener(move |page, _, _, cx| {
                                page.acp_action(id.clone(), false, cx)
                            }))
                        })
                        .child(if installed { "Update" } else { "Install" })
                        .into_any_element(),
                );
            }
            if installed {
                let id = id.clone();
                actions.push(
                    widgets::ghost_action(theme)
                        .id(("acp-agent-uninstall", ix))
                        .flex_none()
                        .when(self.acp_busy.is_none(), |el| {
                            el.on_click(cx.listener(move |page, _, _, cx| {
                                page.acp_action(id.clone(), true, cx)
                            }))
                        })
                        .child("Uninstall")
                        .into_any_element(),
                );
            }
        }
        let meta = match (&agent.unavailable, busy) {
            (_, true) if installed => format!("Updating {}…", agent.name),
            (_, true) => format!("Installing {}…", agent.name),
            (Some(reason), false) if !installed => format!("{} · {reason}", agent_meta(agent)),
            _ => agent_meta(agent),
        };
        widgets::card_row(theme, ix == 0)
            .id(("acp-agent-row", ix))
            .when(!installable && !installed, |el| el.opacity(0.6))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(widgets::row_title(theme, agent.name.clone()))
                    .child(widgets::meta_line(
                        theme,
                        vec![div().child(SharedString::from(meta)).into_any_element()],
                    ))
                    .when(!agent.description.trim().is_empty(), |el| {
                        el.child(
                            div()
                                .text_size(crate::typography::ui_rems(11.5))
                                .text_color(theme.text_muted.opacity(0.8))
                                .line_clamp(2)
                                .child(SharedString::from(agent.description.clone())),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .children(actions),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str) -> AcpRegistryAgentInfo {
        AcpRegistryAgentInfo {
            id: id.into(),
            name: id.into(),
            version: "1.2.0".into(),
            description: String::new(),
            publisher: Some("GitHub".into()),
            website: None,
            license: None,
            distribution: Some(AcpDistributionKind::Npm),
            install_size: Some(12_949_000),
            unavailable: None,
            installed_version: None,
            built_in: None,
        }
    }

    #[test]
    fn meta_names_publisher_version_size_and_route() {
        assert_eq!(agent_meta(&agent("x")), "GitHub · v1.2.0 · 13 MB · npm");
        let mut installed = agent("x");
        installed.installed_version = Some("1.1.0".into());
        assert_eq!(agent_meta(&installed), "GitHub · v1.1.0 → v1.2.0 · 13 MB · npm");
        installed.installed_version = Some("1.2.0".into());
        assert!(agent_meta(&installed).contains("v1.2.0 installed"));
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1_500_000), "1.5 MB");
    }

    #[test]
    fn built_in_duplicates_are_not_listed() {
        let mut codex = agent("codex-acp");
        codex.built_in = Some(zeron_proto::HarnessId::Codex);
        let snapshot = AcpRegistrySnapshot {
            agents: vec![agent("gemini"), codex],
            fetched_at: None,
            error: None,
        };
        let ids: Vec<&str> = listed_agents(&snapshot).iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["gemini"]);
    }
}
