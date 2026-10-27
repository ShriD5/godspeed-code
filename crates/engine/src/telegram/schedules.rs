//! `/schedule` and friends. A schedule is an ordinary always-on agent (it
//! shows in Settings → Zoomies) made with the chat's defaults; this side
//! only remembers which chat owns it, under a short number for commands.

use chrono::{DateTime, Utc};
use zeron_proto::agents::{AgentDraft, AgentRunStatus, AgentSummary};

use super::Inner;
use super::api::{BotApi, Button, Format, Keyboard, Message, SendOptions};
use super::callback::{Callback, ScheduleAction};
use super::schedule_syntax;
use super::store::{LinkedChat, ScheduleRow};

const NAME_PROMPT_CHARS: usize = 60;
const CARD_PROMPT_CHARS: usize = 200;

fn excerpt(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max).collect();
    out.push('…');
    out
}

/// "Fri 16 Oct 09:00" in the chat's zone.
pub(super) fn local_time(at: DateTime<Utc>, timezone: &str) -> String {
    match timezone.parse::<chrono_tz::Tz>() {
        Ok(tz) => at.with_timezone(&tz).format("%a %d %b %H:%M").to_string(),
        Err(_) => at.format("%a %d %b %H:%M UTC").to_string(),
    }
}

fn schedule_status(agent: &AgentSummary, timezone: &str) -> String {
    if let Some(reason) = &agent.paused_reason {
        format!("⏸ Paused: {reason}")
    } else if !agent.draft.enabled {
        "⏸ Paused".into()
    } else if agent.active_run_id.is_some() {
        "▶️ Running now".into()
    } else if let Some(next) = agent.next_fire_at {
        format!("Next run {}", local_time(next, timezone))
    } else {
        "No upcoming run".into()
    }
}

impl Inner {
    fn schedule_card(
        &self,
        row: &ScheduleRow,
        agent: &AgentSummary,
        timezone: &str,
    ) -> (String, Keyboard) {
        let project = std::path::Path::new(&agent.workspace_path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| agent.workspace_path.clone());
        let harness = self
            .available_harnesses()
            .into_iter()
            .find(|h| h.id == agent.draft.harness)
            .map_or_else(|| format!("{:?}", agent.draft.harness), |h| h.name);
        let text = format!(
            "#{} · {}\n{}\n{harness} · {project}\n{}",
            row.id,
            row.label,
            excerpt(&agent.draft.instructions, CARD_PROMPT_CHARS),
            schedule_status(agent, timezone),
        );
        let paused = !agent.draft.enabled || agent.paused_reason.is_some();
        let toggle = if paused {
            Button::new(
                "Resume",
                Callback::Schedule {
                    id: row.id,
                    action: ScheduleAction::Resume,
                }
                .encode(),
            )
        } else {
            Button::new(
                "Pause",
                Callback::Schedule {
                    id: row.id,
                    action: ScheduleAction::Pause,
                }
                .encode(),
            )
        };
        let keyboard = vec![vec![
            toggle,
            Button::new(
                "Run now",
                Callback::Schedule {
                    id: row.id,
                    action: ScheduleAction::RunNow,
                }
                .encode(),
            ),
            Button::new(
                "Delete",
                Callback::Schedule {
                    id: row.id,
                    action: ScheduleAction::Delete,
                }
                .encode(),
            ),
        ]];
        (text, keyboard)
    }

    fn agent_summary(&self, agent_id: &str) -> Option<AgentSummary> {
        self.deps
            .agents
            .list()
            .ok()?
            .into_iter()
            .find(|agent| agent.id == agent_id)
    }

    /// This chat's schedule `id`, dropping rows whose agent was deleted in
    /// the app.
    fn owned_schedule(&self, link: &LinkedChat, id: i64) -> Option<(ScheduleRow, AgentSummary)> {
        let row = self
            .store
            .schedules()
            .ok()?
            .into_iter()
            .find(|row| row.id == id && row.tg_chat == link.chat_id)?;
        match self.agent_summary(&row.agent_id) {
            Some(agent) => Some((row, agent)),
            None => {
                let _ = self.store.delete_schedule(row.id);
                None
            }
        }
    }

    pub(super) async fn create_schedule(&self, api: &BotApi, link: &LinkedChat, args: &str) {
        let reply = match self.schedule_agent(link, args) {
            Ok((row, agent)) => format!(
                "Scheduled #{}: {} ({}).\n{}\nManage it with /schedules.",
                row.id,
                row.label,
                link.timezone,
                schedule_status(&agent, &link.timezone)
            ),
            Err(err) => err,
        };
        self.say(api, link.chat_id, &reply, None).await;
    }

    fn schedule_agent(
        &self,
        link: &LinkedChat,
        args: &str,
    ) -> Result<(ScheduleRow, AgentSummary), String> {
        let spec = schedule_syntax::parse(args, &link.timezone)?;
        let resolved = self.resolve(&link.defaults)?;
        let space = resolved
            .space
            .ok_or("Zoomies run in a project. Pick one with /project first.")?;
        let (sandbox, auto_approve) = resolved.access.legacy();
        let draft = AgentDraft {
            name: format!("Telegram · {}", excerpt(&spec.prompt, NAME_PROMPT_CHARS)),
            harness: resolved.harness,
            model: resolved.model,
            reasoning: None,
            space_id: space.id,
            instructions: spec.prompt,
            enabled: true,
            sandbox,
            auto_approve,
            access_mode: Some(resolved.access),
            triggers: vec![spec.trigger],
        };
        let agent = self
            .deps
            .agents
            .create(draft)
            .map_err(|err| format!("Couldn't schedule it: {err}"))?;
        let row = self
            .store
            .add_schedule(
                &agent.id,
                link.chat_id,
                &spec.label,
                self.options.clock.now(),
            )
            .map_err(|err| err.to_string())?;
        self.changed();
        Ok((row, agent))
    }

    pub(super) async fn list_schedules(&self, api: &BotApi, link: &LinkedChat) {
        let rows: Vec<ScheduleRow> = self
            .store
            .schedules()
            .unwrap_or_default()
            .into_iter()
            .filter(|row| row.tg_chat == link.chat_id)
            .collect();
        let mut shown = 0;
        for row in rows {
            let Some((row, agent)) = self.owned_schedule(link, row.id) else {
                continue;
            };
            let (text, keyboard) = self.schedule_card(&row, &agent, &link.timezone);
            let options = SendOptions {
                keyboard: Some(keyboard),
                ..Default::default()
            };
            if self.send(api, link.chat_id, &text, options).await.is_some() {
                shown += 1;
            }
        }
        if shown == 0 {
            self.say(
                api,
                link.chat_id,
                "No zoomies yet. Try /schedule weekdays 09:00 Summarize yesterday's merged PRs",
                None,
            )
            .await;
        }
    }

    async fn apply_schedule_action(
        &self,
        row: &ScheduleRow,
        action: ScheduleAction,
    ) -> Result<String, String> {
        let agents = &self.deps.agents;
        let failed = |err: crate::EngineError| err.to_string();
        Ok(match action {
            ScheduleAction::Pause => {
                agents.set_enabled(&row.agent_id, false).map_err(failed)?;
                format!("Paused #{}.", row.id)
            }
            ScheduleAction::Resume => {
                agents.set_enabled(&row.agent_id, true).map_err(failed)?;
                format!("Resumed #{}.", row.id)
            }
            ScheduleAction::RunNow => {
                let run = agents.run_now(&row.agent_id).await.map_err(failed)?;
                if run.status == AgentRunStatus::Skipped {
                    format!("#{} is still running its previous run.", row.id)
                } else {
                    format!("▶️ Running #{} now. The result comes here.", row.id)
                }
            }
            ScheduleAction::Delete => {
                agents.delete(&row.agent_id).map_err(failed)?;
                self.store.delete_schedule(row.id).map_err(failed)?;
                self.changed();
                format!("Deleted #{}.", row.id)
            }
        })
    }

    pub(super) async fn schedule_command(
        &self,
        api: &BotApi,
        link: &LinkedChat,
        id: &str,
        action: ScheduleAction,
    ) {
        let reply = match id.trim().trim_start_matches('#').parse::<i64>() {
            Err(_) => {
                "Which schedule? Send its number, e.g. /pause 3 (see /schedules).".to_string()
            }
            Ok(id) => match self.owned_schedule(link, id) {
                None => format!("No schedule #{id} here. See /schedules."),
                Some((row, _)) => self
                    .apply_schedule_action(&row, action)
                    .await
                    .unwrap_or_else(|err| format!("Couldn't do that: {err}")),
            },
        };
        self.say(api, link.chat_id, &reply, None).await;
    }

    pub(super) async fn schedule_button(
        &self,
        api: &BotApi,
        link: &LinkedChat,
        message: &Message,
        id: i64,
        action: ScheduleAction,
    ) -> Option<String> {
        let Some((row, _)) = self.owned_schedule(link, id) else {
            let _ = api
                .edit_message_text(
                    link.chat_id,
                    message.message_id,
                    &format!("Schedule #{id} is gone."),
                    None,
                    Format::Plain,
                )
                .await;
            return Some("That schedule no longer exists.".into());
        };
        let outcome = match self.apply_schedule_action(&row, action).await {
            Ok(outcome) => outcome,
            Err(err) => return Some(err),
        };
        let edited = match self.agent_summary(&row.agent_id) {
            Some(agent) => {
                let (text, keyboard) = self.schedule_card(&row, &agent, &link.timezone);
                api.edit_message_text(
                    link.chat_id,
                    message.message_id,
                    &text,
                    Some(&keyboard),
                    Format::Plain,
                )
                .await
            }
            None => {
                api.edit_message_text(
                    link.chat_id,
                    message.message_id,
                    &outcome,
                    None,
                    Format::Plain,
                )
                .await
            }
        };
        if let Err(err) = edited {
            tracing::warn!(%err, "telegram: schedule card not refreshed");
        }
        Some(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    #[test]
    fn times_render_in_the_chat_zone() {
        let at = Utc.with_ymd_and_hms(2026, 10, 16, 3, 30, 0).unwrap();
        assert_eq!(local_time(at, "Asia/Kolkata"), "Fri 16 Oct 09:00");
        assert_eq!(local_time(at, "Nowhere/Land"), "Fri 16 Oct 03:30 UTC");
    }

    #[test]
    fn excerpts_flatten_and_cut() {
        assert_eq!(excerpt("a\n  b", 10), "a b");
        assert_eq!(excerpt("abcdef", 3), "abc…");
    }
}
