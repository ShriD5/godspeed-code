//! Settings → Providers → (provider) → Instances: several named setups of one
//! agent on the target device — another account, config/home dir, binary,
//! env vars or custom models. The provider's default instance is implicit.
//!
//! Env vars are edited as `KEY=value` lines. Values that look secret (or
//! that the engine already holds in the keychain, shown as `KEY=••••••`) are
//! never echoed back: the engine moves them into the OS keychain on save and
//! replies without them.

use super::HarnessesPage;
use crate::composer::ComposerInput;
use crate::popover::{self, Loadable};
use crate::{settings::widgets, theme::Theme};
use gpui::{AnyElement, Context, Entity, SharedString, div, prelude::*, px};
use zeron_proto::provider_instance::{config_dir_env, instance_id_from_name};
use zeron_proto::{HarnessId, ProviderInstance, ProviderInstanceEnvVar};
use zeron_rpc::methods;

/// Stands in for a stored secret in the env editor; sending it back keeps
/// the stored value.
pub(crate) const SECRET_MASK: &str = "••••••";

pub(super) struct InstanceEditor {
    harness: HarnessId,
    /// `None` creates a new instance.
    existing: Option<String>,
    name: Entity<ComposerInput>,
    binary: Entity<ComposerInput>,
    config_dir: Entity<ComposerInput>,
    env: Entity<ComposerInput>,
    default_model: Entity<ComposerInput>,
    custom_models: Entity<ComposerInput>,
    account: Entity<ComposerInput>,
}

/// `KEY=value` lines → env vars; blank lines and `#` comments are skipped.
pub(crate) fn parse_env_lines(text: &str) -> Result<Vec<ProviderInstanceEnvVar>, String> {
    let mut vars = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("“{line}” is not KEY=value"));
        };
        let key = key.trim().trim_start_matches("export ").trim().to_string();
        let value = value.trim();
        vars.push(if value == SECRET_MASK {
            ProviderInstanceEnvVar {
                key,
                value: None,
                secret: true,
            }
        } else {
            ProviderInstanceEnvVar {
                key,
                value: Some(value.to_string()),
                secret: false,
            }
        });
    }
    Ok(vars)
}

/// Env vars → editor text; secrets show only the mask.
pub(crate) fn env_text(vars: &[ProviderInstanceEnvVar]) -> String {
    vars.iter()
        .map(|var| match (&var.value, var.secret) {
            (Some(value), false) => format!("{}={value}", var.key),
            _ => format!("{}={SECRET_MASK}", var.key),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Comma- or newline-separated model ids.
pub(crate) fn parse_models(text: &str) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    for model in text.split([',', '\n']).map(str::trim).filter(|m| !m.is_empty()) {
        if !models.iter().any(|known| known == model) {
            models.push(model.to_string());
        }
    }
    models
}

/// A fresh id for a new instance, unique among the provider's instances.
pub(crate) fn unique_instance_id(name: &str, taken: &[&str]) -> String {
    let base = instance_id_from_name(name);
    if !taken.contains(&base.as_str()) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(&candidate.as_str()))
        .expect("an unused suffix exists")
}

fn optional(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

impl HarnessesPage {
    pub(super) fn load_instances(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({}));
        let target = self.target_device.clone();
        self.instances_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_PROVIDER_INSTANCES, params)
                .await
                .map_err(|e| e.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<ProviderInstance>>(value).map_err(|e| e.to_string())
                });
            this.update(cx, |page, cx| {
                if page.target_device == target {
                    page.instances = match result {
                        Ok(list) => Loadable::Ready(list),
                        Err(error) => Loadable::Error(error),
                    };
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn open_instance_editor(
        &mut self,
        harness: HarnessId,
        instance: Option<ProviderInstance>,
        cx: &mut Context<Self>,
    ) {
        let input = |placeholder: &str, text: String, single: bool, cx: &mut Context<Self>| {
            let placeholder = placeholder.to_string();
            let entity = cx.new(|cx| {
                let input = ComposerInput::new(placeholder, cx);
                if single { input.with_single_line() } else { input }
            });
            entity.update(cx, |input, cx| input.set_text(text, cx));
            entity
        };
        let i = instance.as_ref();
        let config_hint = config_dir_env(harness)
            .map(|var| format!("Config dir, exported as {var}"))
            .unwrap_or_else(|| "No config-dir variable for this provider".into());
        self.instance_editor = Some(InstanceEditor {
            harness,
            existing: i.map(|i| i.id.clone()),
            name: input("Work account", i.map(|i| i.name.clone()).unwrap_or_default(), true, cx),
            binary: input(
                "/absolute/path/to/the/agent (optional)",
                i.and_then(|i| i.binary_path.clone()).unwrap_or_default(),
                true,
                cx,
            ),
            config_dir: input(
                &config_hint,
                i.and_then(|i| i.config_dir.clone()).unwrap_or_default(),
                true,
                cx,
            ),
            env: input(
                "KEY=value, one per line — secrets go to the keychain",
                i.map(|i| env_text(&i.env)).unwrap_or_default(),
                false,
                cx,
            ),
            default_model: input(
                "Model id (optional)",
                i.and_then(|i| i.default_model.clone()).unwrap_or_default(),
                true,
                cx,
            ),
            custom_models: input(
                "gateway/model-a, gateway/model-b",
                i.map(|i| i.custom_models.join(", ")).unwrap_or_default(),
                true,
                cx,
            ),
            account: input(
                "you@example.com (optional)",
                i.and_then(|i| i.account.clone()).unwrap_or_default(),
                true,
                cx,
            ),
        });
        self.error = None;
        cx.notify();
    }

    fn save_instance(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.instance_editor else {
            return;
        };
        let text = |input: &Entity<ComposerInput>| input.read(cx).text().to_string();
        let name = text(&editor.name).trim().to_string();
        if name.is_empty() {
            self.error = Some("Name the instance".into());
            cx.notify();
            return;
        }
        let env = match parse_env_lines(&text(&editor.env)) {
            Ok(env) => env,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let harness = editor.harness;
        let id = editor.existing.clone().unwrap_or_else(|| {
            let taken: Vec<String> = self
                .instances
                .ready()
                .map(|list| {
                    list.iter()
                        .filter(|i| i.harness == harness)
                        .map(|i| i.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            let taken: Vec<&str> = taken.iter().map(String::as_str).collect();
            unique_instance_id(&name, &taken)
        });
        let instance = ProviderInstance {
            id,
            harness,
            name,
            binary_path: optional(&text(&editor.binary)),
            config_dir: optional(&text(&editor.config_dir)),
            env,
            default_model: optional(&text(&editor.default_model)),
            custom_models: parse_models(&text(&editor.custom_models)),
            account: optional(&text(&editor.account)),
        };
        if let Err(error) = instance.validate() {
            self.error = Some(error);
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({ "instance": instance }));
        self.instance_mutation(engine, methods::SAVE_PROVIDER_INSTANCE, params, cx);
    }

    fn delete_instance(&mut self, harness: HarnessId, id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({ "harness": harness, "id": id }));
        self.instance_mutation(engine, methods::DELETE_PROVIDER_INSTANCE, params, cx);
    }

    fn instance_mutation(
        &mut self,
        engine: crate::state::EngineHandle,
        method: &'static str,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let target = self.target_device.clone();
        self.instances_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|e| e.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<ProviderInstance>>(value).map_err(|e| e.to_string())
                });
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                match result {
                    Ok(list) => {
                        page.instances = Loadable::Ready(list);
                        page.instance_editor = None;
                        // Pickers list instances from the harness catalog.
                        page.refresh_harness_list(cx);
                        crate::pickers::bump_harness_catalog(cx);
                    }
                    Err(error) => page.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The expanded provider's Instances section.
    pub(super) fn render_instances_for(
        &self,
        harness: HarnessId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let instances: Vec<ProviderInstance> = self
            .instances
            .ready()
            .map(|list| list.iter().filter(|i| i.harness == harness).cloned().collect())
            .unwrap_or_default();
        let mut rows: Vec<AnyElement> = vec![
            div()
                .min_h(px(44.0))
                .py(px(8.0))
                .flex()
                .flex_col()
                .child(widgets::row_title(theme, "Default"))
                .child(widgets::meta_line(
                    theme,
                    vec![div()
                        .child("The detected install, its usual config dir and login")
                        .into_any_element()],
                ))
                .into_any_element(),
        ];
        for (ix, instance) in instances.iter().enumerate() {
            let mut meta: Vec<String> = Vec::new();
            if let Some(account) = &instance.account {
                meta.push(account.clone());
            }
            if let Some(dir) = &instance.config_dir {
                meta.push(dir.clone());
            }
            if instance.binary_path.is_some() {
                meta.push("custom binary".into());
            }
            if !instance.env.is_empty() {
                let secrets = instance.env.iter().filter(|v| v.secret).count();
                meta.push(match secrets {
                    0 => format!("{} env vars", instance.env.len()),
                    n => format!("{} env vars ({n} in keychain)", instance.env.len()),
                });
            }
            if !instance.custom_models.is_empty() {
                meta.push(format!("{} custom models", instance.custom_models.len()));
            }
            let edit = instance.clone();
            let id = instance.id.clone();
            rows.push(
                div()
                    .id(("provider-instance", ix))
                    .min_h(px(44.0))
                    .py(px(8.0))
                    .border_t_1()
                    .border_color(widgets::row_divider(theme))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, instance.name.clone()))
                            .when(!meta.is_empty(), |el| {
                                el.child(widgets::meta_line(
                                    theme,
                                    vec![div()
                                        .truncate()
                                        .child(SharedString::from(meta.join(" · ")))
                                        .into_any_element()],
                                ))
                            }),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id(("provider-instance-edit", ix))
                            .flex_none()
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.open_instance_editor(harness, Some(edit.clone()), cx)
                            }))
                            .child("Edit"),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id(("provider-instance-remove", ix))
                            .flex_none()
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.delete_instance(harness, id.clone(), cx)
                            }))
                            .child("Remove"),
                    )
                    .into_any_element(),
            );
        }
        let editor = self
            .instance_editor
            .as_ref()
            .filter(|editor| editor.harness == harness)
            .map(|editor| self.render_instance_editor(editor, theme, cx));
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(widgets::details_label(theme, "Instances"))
                    .when(editor.is_none(), |el| {
                        el.child(
                            widgets::ghost_action(theme)
                                .id(format!("provider-instance-add-{harness}"))
                                .on_click(cx.listener(move |page, _, _, cx| {
                                    page.open_instance_editor(harness, None, cx)
                                }))
                                .child("Add instance"),
                        )
                    }),
            )
            .children(rows)
            .children(editor)
            .into_any_element()
    }

    fn render_instance_editor(
        &self,
        editor: &InstanceEditor,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let field = |label: &str, input: &Entity<ComposerInput>| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(widgets::field_label(theme, label.to_string()))
                .child(popover::dialog_field(input.clone().into_any_element()))
        };
        let pair = |a: gpui::Div, b: gpui::Div| {
            div()
                .flex()
                .flex_row()
                .gap(px(12.0))
                .child(div().flex_1().min_w_0().child(a))
                .child(div().flex_1().min_w_0().child(b))
        };
        let has_config_dir = config_dir_env(editor.harness).is_some();
        widgets::section_card(theme)
            .mt(px(8.0))
            .p(px(16.0))
            .gap(px(12.0))
            .child(pair(field("Name", &editor.name), field("Account", &editor.account)))
            .child(field("Binary path", &editor.binary))
            .when(has_config_dir, |el| {
                el.child(field("Config / home dir", &editor.config_dir))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(widgets::field_label(theme, "Environment variables"))
                    .child(
                        popover::dialog_field(editor.env.clone().into_any_element())
                            .min_h(px(72.0)),
                    ),
            )
            .child(pair(
                field("Default model", &editor.default_model),
                field("Custom models", &editor.custom_models),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        widgets::text_action(theme, widgets::ActionTone::Quiet, "Cancel")
                            .id("provider-instance-cancel")
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.instance_editor = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        widgets::text_action(theme, widgets::ActionTone::Solid, "Save")
                            .id("provider-instance-save")
                            .on_click(cx.listener(|page, _, _, cx| page.save_instance(cx))),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_lines_round_trip_without_revealing_secrets() {
        let vars = parse_env_lines(
            "# proxy\nHTTPS_PROXY=http://proxy:8080\nexport OPENAI_API_KEY = sk-new\nANTHROPIC_API_KEY=••••••\n\n",
        )
        .unwrap();
        assert_eq!(vars.len(), 3);
        assert_eq!(vars[0].value.as_deref(), Some("http://proxy:8080"));
        assert_eq!(vars[1].key, "OPENAI_API_KEY");
        assert_eq!(vars[1].value.as_deref(), Some("sk-new"));
        assert!(vars[2].secret && vars[2].value.is_none());
        let stored = vec![
            ProviderInstanceEnvVar {
                key: "HTTPS_PROXY".into(),
                value: Some("http://proxy:8080".into()),
                secret: false,
            },
            ProviderInstanceEnvVar {
                key: "OPENAI_API_KEY".into(),
                value: None,
                secret: true,
            },
        ];
        assert_eq!(
            env_text(&stored),
            "HTTPS_PROXY=http://proxy:8080\nOPENAI_API_KEY=••••••"
        );
        assert!(parse_env_lines("NOT A PAIR").is_err());
    }

    #[test]
    fn models_and_ids() {
        assert_eq!(parse_models("a, b\nb , ,c"), ["a", "b", "c"]);
        assert_eq!(unique_instance_id("Work", &[]), "work");
        assert_eq!(unique_instance_id("Work", &["work", "work-2"]), "work-3");
    }
}
