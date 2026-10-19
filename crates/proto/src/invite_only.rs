//! The edge's invite-only refusal: an invite-only deployment answers sign-in,
//! refresh and bearer requests from users not on its list with
//! `403 {"error":"invite_only","message":…}` (edge/src/allowlist.ts). Clients
//! show the message instead of a generic sign-in failure.

/// Shown when the edge's reply carries no message of its own.
pub const INVITE_ONLY_MESSAGE: &str = "Godspeed Code cloud sync is invite-only for now. Godspeed Code works fully without it, or you can host your own sync server: see edge/DEPLOY.md.";

/// The JSON error code the edge uses.
pub const INVITE_ONLY_CODE: &str = "invite_only";

/// The user-facing message when an edge reply is an invite-only refusal.
pub fn invite_only_message(status: u16, body: &str) -> Option<String> {
    if status != 403 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    if value.get("error").and_then(|e| e.as_str()) != Some(INVITE_ONLY_CODE) {
        return None;
    }
    let message = value
        .get("message")
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(INVITE_ONLY_MESSAGE);
    Some(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_invite_only_to_the_message() {
        let body = format!(r#"{{"error":"invite_only","message":"{INVITE_ONLY_MESSAGE}"}}"#);
        assert_eq!(
            invite_only_message(403, &body).as_deref(),
            Some(INVITE_ONLY_MESSAGE)
        );
        assert_eq!(
            invite_only_message(403, r#"{"error":"invite_only"}"#).as_deref(),
            Some(INVITE_ONLY_MESSAGE)
        );
    }

    #[test]
    fn ignores_other_errors() {
        assert_eq!(invite_only_message(403, r#"{"error":"forbidden"}"#), None);
        assert_eq!(invite_only_message(401, r#"{"error":"invite_only"}"#), None);
        assert_eq!(invite_only_message(403, "not json"), None);
    }
}
