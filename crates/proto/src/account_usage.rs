//! Account usage shared by the engine and its clients: which login a usage
//! limit belongs to, and the "continue on another account" request.
//!
//! A Limited chat records the login it hit as an opaque fingerprint of the
//! harness's active account ids ([`login_fingerprint`]), never the ids
//! themselves, so the marker can sync on the chat row. Both sides recompute
//! it from an accounts snapshot to tell which account a limit sits on.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{AgentAccount, AgentAccountsSnapshot, HarnessId};

/// An opaque digest of a set of account ids, safe to sync on the chat row.
pub fn login_fingerprint<'a>(ids: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut ids: Vec<&str> = ids.collect();
    if ids.is_empty() {
        return None;
    }
    ids.sort_unstable();
    let digest = Sha256::digest(ids.join("\n").as_bytes());
    Some(digest[..12].iter().map(|b| format!("{b:02x}")).collect())
}

impl AgentAccountsSnapshot {
    /// The harness's live account(s).
    pub fn active_ids(&self, harness: HarnessId) -> impl Iterator<Item = &str> {
        self.accounts
            .iter()
            .filter(move |a| a.harness == harness && a.active)
            .map(|a| a.id.as_str())
    }

    /// The fingerprint the harness would be signed in with were `account`
    /// its live login: agents that keep one login per model provider swap
    /// only that provider's, the rest keep theirs. Pure.
    pub fn fingerprint_with(&self, account: &AgentAccount) -> Option<String> {
        login_fingerprint(
            self.accounts
                .iter()
                .filter(|a| a.harness == account.harness)
                .filter(|a| {
                    if a.provider == account.provider {
                        a.id == account.id
                    } else {
                        a.active
                    }
                })
                .map(|a| a.id.as_str()),
        )
    }
}

/// `ContinueChatOnAccount`: fork a chat into a new one that runs on another
/// signed-in account of the same provider, or on another provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinueOnAccountRequest {
    /// The new chat's id (client-minted, so a retry is idempotent).
    pub chat_id: String,
    pub source_chat_id: String,
    /// The provider to continue on.
    pub harness: HarnessId,
    /// The account of `harness` to continue on.
    pub account_id: String,
    /// The model for a different provider; `None` keeps the source's model
    /// on the same provider and takes the provider's default elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// How the continued chat carries the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContinuationContext {
    /// The provider forks the source's own session on the first turn.
    NativeFork,
    /// A compact summary built from the transcript rides the first prompt.
    Brief,
}

/// The `ContinueChatOnAccount` reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinueOnAccountReply {
    pub chat: crate::Chat,
    pub context: ContinuationContext,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, harness: HarnessId, provider: Option<&str>, active: bool) -> AgentAccount {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "harness": harness,
            "email": null,
            "planLabel": null,
            "active": active,
            "provider": provider,
        }))
        .unwrap()
    }

    #[test]
    fn fingerprints_ignore_order_and_name_nothing() {
        assert_eq!(login_fingerprint(std::iter::empty()), None);
        let a = login_fingerprint(["x", "y"].into_iter()).unwrap();
        assert_eq!(Some(a.clone()), login_fingerprint(["y", "x"].into_iter()));
        assert_eq!(a.len(), 24);
        assert!(!a.contains('x'));
    }

    #[test]
    fn fingerprint_with_swaps_only_the_accounts_group() {
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account("c1", HarnessId::ClaudeCode, None, true),
                account("c2", HarnessId::ClaudeCode, None, false),
                account("o-openai", HarnessId::Opencode, Some("openai"), true),
                account("o-openai-2", HarnessId::Opencode, Some("openai"), false),
                account(
                    "o-copilot",
                    HarnessId::Opencode,
                    Some("github-copilot"),
                    true,
                ),
            ],
            warnings: Vec::new(),
        };
        let fp = |ids: &[&str]| login_fingerprint(ids.iter().copied());
        assert_eq!(
            snapshot.fingerprint_with(&snapshot.accounts[0]),
            fp(&["c1"])
        );
        assert_eq!(
            snapshot.fingerprint_with(&snapshot.accounts[1]),
            fp(&["c2"])
        );
        assert_eq!(
            snapshot.fingerprint_with(&snapshot.accounts[3]),
            fp(&["o-openai-2", "o-copilot"])
        );
        assert_eq!(
            login_fingerprint(snapshot.active_ids(HarnessId::Opencode)),
            fp(&["o-openai", "o-copilot"])
        );
    }
}
