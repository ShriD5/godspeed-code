//! What a Telegram message asks for: one of the bot's commands, or text
//! for the agent. Slash commands the bot doesn't know go to the agent too,
//! so a provider's own `/review`-style commands keep working.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Start,
    Help,
    Link(String),
    Unlink,
    New,
    Provider,
    Model,
    Project,
    Access,
    Status,
    Stop,
    Timezone(String),
    Schedule(String),
    Schedules,
    Pause(String),
    Resume(String),
    Unschedule(String),
    RunNow(String),
}

/// `None` for text meant for the agent.
pub(crate) fn parse(text: &str) -> Option<Command> {
    let text = text.trim();
    let rest = text.strip_prefix('/')?;
    let (head, args) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(head, args)| (head, args.trim()));
    // `/status@my_bot` in a client that appends the bot's name.
    let name = head.split('@').next().unwrap_or(head).to_ascii_lowercase();
    let args = args.to_string();
    Some(match name.as_str() {
        "start" => Command::Start,
        "help" => Command::Help,
        "link" => Command::Link(args),
        "unlink" => Command::Unlink,
        "new" => Command::New,
        "provider" => Command::Provider,
        "model" => Command::Model,
        "project" => Command::Project,
        "access" => Command::Access,
        "status" => Command::Status,
        "stop" => Command::Stop,
        "tz" | "timezone" => Command::Timezone(args),
        "schedule" => Command::Schedule(args),
        "schedules" => Command::Schedules,
        "pause" => Command::Pause(args),
        "resume" => Command::Resume(args),
        "unschedule" => Command::Unschedule(args),
        "runnow" => Command::RunNow(args),
        _ => return None,
    })
}

pub(crate) const HELP: &str = "\
Message me and I'll pass it to your Godspeed agent. Replies come back here.

Threads
• Just type to continue the current thread (the first message starts one)
• Reply to one of my answers to continue that thread
/new — start a fresh thread
/stop — stop the current thread's run
/status — current thread and settings

Settings for new threads
/provider — pick the agent
/model — pick its model
/project — pick a project (or none)
/access — pick the access mode
/tz Area/City — your time zone for schedules

Schedules (zoomies)
/schedule hourly <prompt>
/schedule daily HH:MM <prompt>
/schedule weekdays HH:MM <prompt>
/schedule weekly mon HH:MM <prompt>
/schedule every 30m <prompt>
/schedules — list your zoomies with buttons
/pause ID · /resume ID · /runnow ID · /unschedule ID

/unlink — stop this chat from driving Godspeed
Any other /command goes to the agent as text.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_with_args_and_bot_suffix() {
        assert_eq!(parse("/link AB12CD"), Some(Command::Link("AB12CD".into())));
        assert_eq!(parse("  /status@godspeed_bot "), Some(Command::Status));
        assert_eq!(
            parse("/schedule daily 09:00 Summarize PRs"),
            Some(Command::Schedule("daily 09:00 Summarize PRs".into()))
        );
        assert_eq!(parse("/PAUSE 3"), Some(Command::Pause("3".into())));
        assert_eq!(
            parse("/tz Asia/Kolkata"),
            Some(Command::Timezone("Asia/Kolkata".into()))
        );
    }

    #[test]
    fn plain_text_and_unknown_commands_go_to_the_agent() {
        assert_eq!(parse("fix the flaky test"), None);
        assert_eq!(parse("/review the last commit"), None);
        assert_eq!(parse(""), None);
    }
}
