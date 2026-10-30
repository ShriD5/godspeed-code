//! Which client calls mean a person is using the app.
//!
//! Background work keys off this: the PR watch slows down and then pauses
//! without it, and diff sync skips its periodic safety repair. A window that
//! is open but unattended still makes calls on its own (the PR list refresh
//! every two minutes, the usage gauge, every `Watch*` stream it holds), so
//! "any call" kept all of that running at full speed forever. Only calls a
//! person makes count: sending and steering, answering a question or an
//! approval, opening a chat, focusing the window, acting on a PR, typing in
//! a terminal.

use zeron_rpc::methods;

/// True for methods a client only sends because someone acted.
pub(crate) fn is_user_activity(method: &str) -> bool {
    matches!(
        method,
        // Sending, interrupting and answering input all ride QueueCommand.
        methods::QUEUE_COMMAND
            | methods::QUEUE_MESSAGE
            | methods::UPDATE_QUEUED_MESSAGE
            | methods::MOVE_QUEUED_MESSAGE
            | methods::REMOVE_QUEUED_MESSAGE
            | methods::SEND_QUEUED_MESSAGE_NOW
            | methods::STEER_QUEUED_MESSAGE_NOW
            | methods::RETRY_DELIVERY
            | methods::FORK_SIDE_CHAT
            // Opening a chat (explicit navigation only, never a prefetch).
            | methods::FOCUS_CHAT
            // The window was focused.
            | methods::PROBE_SYNC
            // Needs-you answers.
            | methods::RESPOND_ATTENTION
            // Creating, renaming, archiving chats and projects.
            | methods::MUTATE
            | methods::CONTINUE_CHAT_ON_ACCOUNT
            // Pull request and git actions.
            | methods::SET_PULL_REQUEST_WATCH
            | methods::PERFORM_CHANGE_REQUEST_ACTION
            | methods::POST_CHANGE_REQUEST_COMMENT
            | methods::CREATE_PULL_REQUEST
            | methods::GIT_COMMIT
            | methods::GIT_PUSH
            | methods::GIT_PULL
            | methods::GIT_SYNC
            | methods::RUN_PROJECT_ACTION
            // A terminal opened or typed into.
            | methods::OPEN_TERMINAL
            | methods::WRITE_TERMINAL
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn people_count_and_background_polls_do_not() {
        for method in [
            methods::QUEUE_COMMAND,
            methods::QUEUE_MESSAGE,
            methods::FOCUS_CHAT,
            methods::PROBE_SYNC,
            methods::RESPOND_ATTENTION,
            methods::SET_PULL_REQUEST_WATCH,
        ] {
            assert!(is_user_activity(method), "{method} is a person acting");
        }
        // What an open, unattended window sends on its own.
        for method in [
            methods::LIST_PULL_REQUEST_WATCHES,
            methods::LIST_AGENT_ACCOUNTS,
            methods::WATCH_CHATS,
            methods::WATCH_SESSIONS,
            methods::WATCH_CHECKOUT_DIFFS,
            methods::WATCH_DOC_MESSAGES,
            methods::WATCH_HARNESS_UPDATES,
            methods::ENGINE_INFO,
            methods::ENGINE_READY,
            methods::SYNC_STATUS,
            methods::GET_USAGE_HISTORY,
            methods::LIST_HARNESSES,
            methods::GET_CHECKOUT_DIFF,
            methods::GIT_BRANCH_STATUS,
        ] {
            assert!(!is_user_activity(method), "{method} is a background poll");
        }
    }
}
