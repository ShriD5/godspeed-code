//! Inline-button payloads. Telegram caps `callback_data` at 64 bytes, so
//! each is a short tagged string; anything bigger is referenced by a row id.

use zeron_proto::{AccessMode, HarnessId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleAction {
    Pause,
    Resume,
    RunNow,
    Delete,
}

impl ScheduleAction {
    fn code(self) -> &'static str {
        match self {
            Self::Pause => "p",
            Self::Resume => "r",
            Self::RunNow => "n",
            Self::Delete => "d",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "p" => Self::Pause,
            "r" => Self::Resume,
            "n" => Self::RunNow,
            "d" => Self::Delete,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Callback {
    Provider(HarnessId),
    /// An index into the provider's model list; `None` is its default.
    Model(Option<usize>),
    ModelPage(usize),
    /// A space id; `None` is "no project".
    Project(Option<String>),
    Access(AccessMode),
    Schedule {
        id: i64,
        action: ScheduleAction,
    },
    Answer {
        prompt: i64,
        question: usize,
        option: usize,
    },
}

fn wire<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn from_wire<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(text.to_owned())).ok()
}

impl Callback {
    pub fn encode(&self) -> String {
        match self {
            Self::Provider(harness) => format!("h:{}", wire(harness)),
            Self::Model(Some(index)) => format!("m:{index}"),
            Self::Model(None) => "m:-".into(),
            Self::ModelPage(page) => format!("mp:{page}"),
            Self::Project(Some(space)) => format!("p:{space}"),
            Self::Project(None) => "p:-".into(),
            Self::Access(mode) => format!("x:{}", wire(mode)),
            Self::Schedule { id, action } => format!("s:{id}:{}", action.code()),
            Self::Answer {
                prompt,
                question,
                option,
            } => format!("a:{prompt}:{question}:{option}"),
        }
    }

    pub fn decode(data: &str) -> Option<Self> {
        let (tag, rest) = data.split_once(':')?;
        Some(match tag {
            "h" => Self::Provider(from_wire(rest)?),
            "m" if rest == "-" => Self::Model(None),
            "m" => Self::Model(Some(rest.parse().ok()?)),
            "mp" => Self::ModelPage(rest.parse().ok()?),
            "p" if rest == "-" => Self::Project(None),
            "p" if !rest.is_empty() => Self::Project(Some(rest.to_string())),
            "x" => Self::Access(from_wire(rest)?),
            "s" => {
                let (id, action) = rest.split_once(':')?;
                Self::Schedule {
                    id: id.parse().ok()?,
                    action: ScheduleAction::from_code(action)?,
                }
            }
            "a" => {
                let mut parts = rest.splitn(3, ':');
                Self::Answer {
                    prompt: parts.next()?.parse().ok()?,
                    question: parts.next()?.parse().ok()?,
                    option: parts.next()?.parse().ok()?,
                }
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_payload_round_trips_within_the_size_cap() {
        let all = [
            Callback::Provider(HarnessId::ClaudeCode),
            Callback::Model(Some(12)),
            Callback::Model(None),
            Callback::ModelPage(3),
            Callback::Project(Some("8c1f0f0e-58f3-4b8a-9d55-0d4c3b7f5e21".into())),
            Callback::Project(None),
            Callback::Access(AccessMode::AutoAcceptEdits),
            Callback::Schedule {
                id: 42,
                action: ScheduleAction::RunNow,
            },
            Callback::Answer {
                prompt: 9_000_000,
                question: 1,
                option: 2,
            },
        ];
        for callback in all {
            let data = callback.encode();
            assert!(data.len() <= 64, "{data}");
            assert_eq!(Callback::decode(&data), Some(callback), "{data}");
        }
        assert_eq!(
            Callback::Provider(HarnessId::ClaudeCode).encode(),
            "h:claude-code"
        );
    }

    #[test]
    fn garbage_is_ignored() {
        for data in ["", "zz", "h:nope", "s:1:q", "a:1:2", "m:x", "p:"] {
            assert_eq!(Callback::decode(data), None, "{data}");
        }
    }
}
