//! The prompt an agent run starts with: who it is, what woke it, its memory
//! file, and its instructions with any webhook payload substituted.
//!
//! Placeholders are plain `{{path}}` lookups (no conditionals or filters, so
//! it never grows into a template language — same rule as T3 Code's
//! webhook templates):
//!
//! - `{{body}}` the raw body text; `{{body.a.b.0}}` a field of a JSON body
//! - `{{headers.name}}` one request header (case-insensitive, raw value)
//! - `{{headers}}` every header, credential-looking ones redacted
//!
//! Strings render as text, other JSON as JSON, and a missing path as empty.

use std::path::Path;

use serde_json::Value;

/// Memory beyond this is cut from the prompt (the file itself is untouched).
const MAX_MEMORY_BYTES: usize = 32 * 1024;
/// A payload appended verbatim (no placeholders used) is cut at this size.
const MAX_APPENDED_PAYLOAD_BYTES: usize = 16 * 1024;

/// The HTTP request behind a webhook run.
#[derive(Debug, Clone, Default)]
pub(crate) struct WebhookPayload {
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// What woke the run, for the preamble.
pub(crate) struct RunContext<'a> {
    pub agent_name: &'a str,
    pub trigger_label: &'a str,
    pub scheduled_at: chrono::DateTime<chrono::Utc>,
    pub memory_path: &'a Path,
    pub memory: &'a str,
    pub instructions: &'a str,
    pub webhook: Option<&'a WebhookPayload>,
    pub changed_paths: &'a [String],
}

pub(crate) fn build_prompt(ctx: &RunContext<'_>) -> String {
    let mut instructions = match ctx.webhook {
        Some(payload) => render_template(ctx.instructions, payload),
        None => ctx.instructions.to_string(),
    };
    if let Some(payload) = ctx.webhook
        && !ctx.instructions.contains("{{body")
        && !payload.body.trim().is_empty()
    {
        // No placeholder asked for the body, but the payload is the whole
        // point of a webhook run: hand it over rather than drop it.
        instructions.push_str("\n\nWebhook payload:\n```\n");
        instructions.push_str(truncate(&payload.body, MAX_APPENDED_PAYLOAD_BYTES));
        instructions.push_str("\n```");
    }
    if !ctx.changed_paths.is_empty() {
        instructions.push_str("\n\nChanged paths:\n");
        for path in ctx.changed_paths {
            instructions.push_str("- ");
            instructions.push_str(path);
            instructions.push('\n');
        }
    }

    let memory = ctx.memory.trim();
    let memory_block = if memory.is_empty() {
        "(empty)".to_string()
    } else if memory.len() > MAX_MEMORY_BYTES {
        format!(
            "{}\n…(truncated; read the file for the rest)",
            truncate(memory, MAX_MEMORY_BYTES)
        )
    } else {
        memory.to_string()
    };
    format!(
        "You are \"{name}\", an always-on agent running unattended in Godspeed. \
This run was started by: {trigger} (scheduled {at}).\n\n\
## Memory\n\
Your persistent memory file is `{path}`. You may update it in place with anything \
future runs should know (decisions, progress, open items); keep it short. Current contents:\n\
<memory>\n{memory}\n</memory>\n\n\
## Instructions\n{instructions}",
        name = ctx.agent_name,
        trigger = ctx.trigger_label,
        at = ctx
            .scheduled_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        path = ctx.memory_path.display(),
        memory = memory_block,
        instructions = instructions.trim_end(),
    )
}

pub(crate) fn render_template(template: &str, payload: &WebhookPayload) -> String {
    let body_json: Option<Value> = serde_json::from_str(&payload.body).ok();
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = after[..end].trim();
        match lookup(key, payload, body_json.as_ref()) {
            Some(value) => out.push_str(&value),
            // Not one of ours: leave it for the agent to read as written.
            None if !is_placeholder(key) => out.push_str(&rest[start..start + 2 + end + 2]),
            None => {}
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn is_placeholder(key: &str) -> bool {
    key == "body" || key == "headers" || key.starts_with("body.") || key.starts_with("headers.")
}

fn lookup(key: &str, payload: &WebhookPayload, body: Option<&Value>) -> Option<String> {
    if key == "body" {
        return Some(payload.body.clone());
    }
    if key == "headers" {
        let redacted: serde_json::Map<String, Value> = payload
            .headers
            .iter()
            .map(|(name, value)| {
                let shown = if is_credential(name) {
                    "[redacted]"
                } else {
                    value
                };
                (name.clone(), Value::String(shown.to_string()))
            })
            .collect();
        return Some(Value::Object(redacted).to_string());
    }
    if let Some(name) = key.strip_prefix("headers.") {
        return payload
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone());
    }
    let path = key.strip_prefix("body.")?;
    let mut node = body?;
    for segment in path.split('.') {
        node = match node {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(match node {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    })
}

fn is_credential(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    ) || ["token", "secret", "signature", "key", "password", "auth"]
        .iter()
        .any(|needle| name.contains(needle))
}

fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// First `max` characters of the final assistant message, on one line.
pub(crate) fn excerpt(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(body: &str) -> WebhookPayload {
        WebhookPayload {
            headers: vec![
                ("x-github-event".into(), "push".into()),
                ("x-hub-signature-256".into(), "sha256=abc".into()),
            ],
            body: body.into(),
        }
    }

    #[test]
    fn renders_body_paths_headers_and_leaves_foreign_braces() {
        let p = payload(
            r#"{"repo":{"name":"godspeed"},"commits":[{"id":"c1"},{"id":"c2"}],"n":3,"obj":{"a":true}}"#,
        );
        let out = render_template(
            "{{ headers.X-GitHub-Event }} to {{body.repo.name}}: {{body.commits.1.id}} n={{body.n}} {{body.obj}} [{{body.missing.x}}] {{other}}",
            &p,
        );
        assert_eq!(out, r#"push to godspeed: c2 n=3 {"a":true} [] {{other}}"#);
        let headers = render_template("{{headers}}", &p);
        assert!(headers.contains("[redacted]") && !headers.contains("sha256=abc"));
        assert_eq!(render_template("raw={{body}}", &payload("hi")), "raw=hi");
        assert_eq!(
            render_template("open {{body", &payload("{}")),
            "open {{body"
        );
    }

    #[test]
    fn prompt_injects_memory_and_appends_unreferenced_payload() {
        let p = payload(r#"{"action":"opened"}"#);
        let ctx = RunContext {
            agent_name: "Triage",
            trigger_label: "Webhook",
            scheduled_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            memory_path: Path::new("/data/agents/a1/MEMORY.md"),
            memory: "- saw issue #4\n",
            instructions: "Triage the new issue.",
            webhook: Some(&p),
            changed_paths: &[],
        };
        let prompt = build_prompt(&ctx);
        assert!(prompt.contains("`/data/agents/a1/MEMORY.md`"));
        assert!(prompt.contains("<memory>\n- saw issue #4\n</memory>"));
        assert!(prompt.contains(
            "Triage the new issue.\n\nWebhook payload:\n```\n{\"action\":\"opened\"}\n```"
        ));

        let templated = RunContext {
            instructions: "Action: {{body.action}}",
            ..ctx
        };
        let prompt = build_prompt(&templated);
        assert!(
            prompt.ends_with("## Instructions\nAction: opened"),
            "{prompt}"
        );
    }

    #[test]
    fn excerpt_flattens_and_truncates() {
        assert_eq!(excerpt("a\n\n b  c", 10), "a b c");
        assert_eq!(excerpt("abcdefghijk", 5), "abcd…");
    }
}
